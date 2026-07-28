//! Finding a provider binary: which files on PATH name a `buzz-backend-*`
//! provider, and which id resolves to which executable.
//!
//! Split out of `backend.rs` so the question "is this file a provider I may
//! execute?" — a per-platform naming and executability rule with its own
//! security argument — sits apart from actually invoking one.

use std::path::{Path, PathBuf};

const PROVIDER_PREFIX: &str = "buzz-backend-";

/// Executable extensions a provider binary may carry on Windows.
///
/// `.cmd` and `.bat` are deliberately absent. `std::process::Command` runs
/// those through `cmd.exe`, which would put a shell-quoting surface in front of
/// a code path that pipes an agent's private key over stdin. `.exe` and `.com`
/// are launched directly by `CreateProcess` with no interpreter in between.
/// This is a security boundary, not a style preference.
#[cfg(any(windows, test))]
const SAFE_EXEC_EXTENSIONS: &[&str] = &["exe", "com"];

/// Windows' documented `PATHEXT` default, used when the variable is unset or empty.
#[cfg(any(windows, test))]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Parse `PATHEXT` into this machine's provider-executable allowlist:
/// `PATHEXT` ∩ `SAFE_EXEC_EXTENSIONS`, lowercased and without the leading dot.
///
/// Taking the intersection means a machine whose `PATHEXT` drops an extension
/// drops it here too, while `.cmd`/`.bat` can never re-enter through a
/// user-configured `PATHEXT`.
///
/// `PATHEXT` order is preserved: it is the order Windows itself resolves a
/// bare command name in, and [`ExecNaming::rank`] reuses it to break ties
/// between two files in one directory.
#[cfg(any(windows, test))]
fn allowed_exec_extensions_from(pathext: Option<&str>) -> Vec<String> {
    let raw = pathext
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PATHEXT);
    raw.split(';')
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| SAFE_EXEC_EXTENSIONS.contains(&ext.as_str()))
        .collect()
}

/// How this platform names an executable file.
///
/// An explicit two-variant seam rather than "an allowlist that may be empty":
/// an empty Windows allowlist (a `PATHEXT` of only `.CMD`/`.BAT`) means
/// *nothing* is executable, which is the opposite of the unix meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecNaming {
    /// Unix: no extension convention; the execute permission bit is the contract.
    NoExtension,
    /// Windows: the file name must end in one of these extensions (lowercased,
    /// no leading dot). See `SAFE_EXEC_EXTENSIONS`.
    ///
    /// Only constructed on Windows (and in tests, which exercise the Windows
    /// rule from any host) — the variant still has to exist everywhere so the
    /// derivation logic is compiled and tested once rather than per-platform.
    #[cfg_attr(not(windows), allow(dead_code))]
    Extensions(Vec<String>),
}

impl ExecNaming {
    /// Where `ext` (no leading dot) sits in this platform's extension
    /// precedence, or `None` when it does not name an executable at all.
    ///
    /// The order is `PATHEXT`'s, which `allowed_exec_extensions_from`
    /// preserves: when one directory holds both `foo.com` and `foo.exe`,
    /// Windows command lookup runs whichever extension `PATHEXT` lists first.
    fn rank(&self, ext: &str) -> Option<usize> {
        match self {
            ExecNaming::NoExtension => None,
            ExecNaming::Extensions(allowed) => {
                allowed.iter().position(|a| a.eq_ignore_ascii_case(ext))
            }
        }
    }

    /// Does `ext` (no leading dot) name an executable on this platform?
    fn allows(&self, ext: &str) -> bool {
        self.rank(ext).is_some()
    }
}

/// The executable-naming rule for the running platform.
fn provider_exec_naming() -> ExecNaming {
    #[cfg(windows)]
    {
        ExecNaming::Extensions(allowed_exec_extensions_from(
            std::env::var("PATHEXT").ok().as_deref(),
        ))
    }
    #[cfg(not(windows))]
    {
        ExecNaming::NoExtension
    }
}

/// Derive a provider id from a directory entry's file name, or `None` when the
/// entry is not a usable provider.
///
/// Under [`ExecNaming::NoExtension`] the name after the prefix is the id
/// verbatim. Under [`ExecNaming::Extensions`] the name MUST end in an allowed
/// extension and that extension is stripped: `buzz-backend-ssh.exe` yields
/// `ssh`, because `ssh.exe` fails [`provider_id_is_valid`] and would leave
/// every Windows provider undeployable. `buzz-backend-ssh.cmd` yields `None`
/// — see `SAFE_EXEC_EXTENSIONS`.
fn provider_id_from_file_name(name: &str, naming: &ExecNaming) -> Option<String> {
    let stem = name.strip_prefix(PROVIDER_PREFIX)?;
    if stem.is_empty() {
        return None;
    }
    if *naming == ExecNaming::NoExtension {
        return Some(stem.to_string());
    }
    let (base, ext) = stem.rsplit_once('.')?;
    if base.is_empty() || !naming.allows(ext) {
        return None;
    }
    Some(base.to_string())
}

/// Provider ids must match `^[a-z0-9][a-z0-9_-]*$`: no path components, no
/// shell metacharacters, and no file extension.
fn provider_id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Enumerate PATH for buzz-backend-* executables. Returns (id, path) pairs.
/// Only includes files that are executable. Does NOT execute any binaries.
///
/// On macOS, GUI apps inherit a minimal PATH from launchd (`/usr/bin:/bin:/usr/sbin:/sbin`)
/// which excludes both the app bundle's `Contents/MacOS/` dir and `~/.local/bin`.
/// We augment the search with those directories so bundled and user-installed providers
/// are always discovered regardless of how the desktop was launched.
///
/// On Windows the executable extension is stripped from the id and constrained
/// to `SAFE_EXEC_EXTENSIONS` — see [`provider_id_from_file_name`].
pub fn discover_provider_candidates() -> Vec<(String, PathBuf)> {
    let naming = provider_exec_naming();
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();

    // Prepend the exe parent dir (Contents/MacOS/ in a .app bundle) so bundled
    // providers are found even when the process PATH is minimal.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let parent_buf = parent.to_path_buf();
            if !dirs.contains(&parent_buf) {
                dirs.insert(0, parent_buf);
            }
        }
    }

    // Also include ~/.local/bin — the conventional location for user-installed
    // provider binaries (symlinks created by install scripts).
    if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local").join("bin");
        if !dirs.contains(&local_bin) {
            dirs.push(local_bin);
        }
    }

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let names = entries.flatten().filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Cheap name filter before the `is_executable` stat, so an
            // unrelated directory entry never costs a metadata call.
            (name.starts_with(PROVIDER_PREFIX) && is_executable(&entry.path())).then_some(name)
        });
        // Dedupe on the derived id, not the file name: on Windows two
        // extensions map to one id, and the earlier PATH entry must win as it
        // would for any other command lookup.
        for (id, name) in provider_candidates_in_dir(names, &naming) {
            if seen.insert(id.clone()) {
                results.push((id, dir.join(name)));
            }
        }
    }
    results
}

/// Reduce one directory's file names to its `(id, file name)` providers,
/// ordered so a same-id conflict resolves the way the platform's command
/// lookup would.
///
/// `read_dir` has no defined iteration order, so a directory holding both
/// `buzz-backend-foo.com` and `buzz-backend-foo.exe` — one id, two files —
/// would otherwise yield whichever the filesystem happened to list first, and
/// discovery could deploy a different binary than Windows would execute.
/// Candidates are ranked by their extension's position in `PATHEXT` (which
/// [`allowed_exec_extensions_from`] preserves) and only the winner per id is
/// kept. Ids that `resolve_provider_binary` would reject are dropped here so
/// the catalog never advertises a provider that can never be executed.
fn provider_candidates_in_dir(
    names: impl IntoIterator<Item = String>,
    naming: &ExecNaming,
) -> Vec<(String, String)> {
    let mut ranked: Vec<(String, usize, String)> = names
        .into_iter()
        .filter_map(|name| {
            let id = provider_id_from_file_name(&name, naming)?;
            if !provider_id_is_valid(&id) {
                return None;
            }
            let rank = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|ext| naming.rank(ext))
                .unwrap_or(0);
            Some((id, rank, name))
        })
        .collect();
    // Sort by id so duplicates are adjacent, then by precedence; the file name
    // breaks any remaining tie so the result never depends on `read_dir` order.
    ranked.sort();
    ranked.dedup_by(|a, b| a.0 == b.0);
    ranked.into_iter().map(|(id, _, name)| (id, name)).collect()
}

/// Resolve a provider ID to a discovered, executable binary path.
///
/// This is the ONLY way to resolve provider binaries for execution. It:
/// 1. Validates the ID against `^[a-z0-9][a-z0-9_-]*$` (no path traversal)
/// 2. Looks up the ID in `discover_provider_candidates()` (PATH-discovered only)
/// 3. Returns the canonical path of the discovered binary
///
/// All deploy, start, and create paths MUST use this instead of raw
/// `resolve_command(format!("buzz-backend-{id}"))` to prevent a compromised
/// frontend/IPC caller from steering execution to an arbitrary binary.
pub fn resolve_provider_binary(provider_id: &str) -> Result<PathBuf, String> {
    // Reject IDs that could be path components or shell metacharacters.
    if !provider_id_is_valid(provider_id) {
        return Err(format!(
            "invalid provider ID '{provider_id}': must match [a-z0-9][a-z0-9_-]*"
        ));
    }

    let candidates = discover_provider_candidates();
    let found = candidates
        .into_iter()
        .find(|(id, _)| id == provider_id)
        .map(|(_, path)| path);

    match found {
        Some(path) => path
            .canonicalize()
            .map_err(|e| format!("provider binary not accessible: {e}")),
        None => Err(format!(
            "provider 'buzz-backend-{provider_id}' not found on PATH"
        )),
    }
}

/// Check if a file is executable.
///
/// Unix: a regular file with at least one execute mode bit.
/// Windows: a regular file whose extension is allowed by [`provider_exec_naming`]
/// — Windows has no execute bit, so the extension *is* the contract.
/// Other platforms: regular file only.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(windows)]
    {
        let naming = provider_exec_naming();
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| naming.allows(ext))
    }

    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendProviderInfo {
    pub id: String,
    pub binary_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The naming rule a Windows host with a default `PATHEXT` produces. Lets
    /// the id-derivation tests exercise the Windows branch from any host —
    /// `provider_id_from_file_name` takes the rule as an argument precisely so
    /// this logic is testable without a Windows runner.
    fn windows_naming() -> ExecNaming {
        ExecNaming::Extensions(allowed_exec_extensions_from(None))
    }

    #[test]
    fn provider_id_strips_windows_executable_extension() {
        // The W1 bug: `buzz-backend-ssh.exe` yielded id "ssh.exe", which
        // `provider_id_is_valid` rejects on the `.`, so no provider was ever
        // runnable on Windows.
        let id = provider_id_from_file_name("buzz-backend-ssh.exe", &windows_naming());
        assert_eq!(id.as_deref(), Some("ssh"));
        assert!(provider_id_is_valid(id.as_deref().unwrap()));
        // Case-insensitive: Windows file names are routinely upper-cased.
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh.EXE", &windows_naming()).as_deref(),
            Some("ssh")
        );
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh.com", &windows_naming()).as_deref(),
            Some("ssh")
        );
        // Only the final extension is stripped — a dotted id stays invalid
        // rather than collapsing two files onto one id.
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh.v2.exe", &windows_naming()).as_deref(),
            Some("ssh.v2")
        );
        assert!(!provider_id_is_valid("ssh.v2"));
    }

    #[test]
    fn provider_id_rejects_shell_script_extensions_on_windows() {
        // `.cmd`/`.bat` route through cmd.exe, which would add shell quoting
        // in front of a stdin channel that carries an nsec. Security boundary.
        for name in [
            "buzz-backend-ssh.cmd",
            "buzz-backend-ssh.bat",
            "buzz-backend-ssh.CMD",
            "buzz-backend-ssh.ps1",
        ] {
            assert!(
                provider_id_from_file_name(name, &windows_naming()).is_none(),
                "{name} must not yield a provider id"
            );
        }
    }

    #[test]
    fn provider_id_rejects_extensionless_and_empty_names_on_windows() {
        // Windows requires an extension to execute, so a bare name is not a
        // provider there even though it is the normal case on unix.
        assert!(provider_id_from_file_name("buzz-backend-ssh", &windows_naming()).is_none());
        assert!(provider_id_from_file_name("buzz-backend-.exe", &windows_naming()).is_none());
        assert!(provider_id_from_file_name("buzz-backend-", &windows_naming()).is_none());
        assert!(provider_id_from_file_name("other-tool.exe", &windows_naming()).is_none());
    }

    #[test]
    fn provider_id_on_unix_keeps_the_name_verbatim() {
        // The unix arm must behave exactly like the old bare `strip_prefix`:
        // no extension handling at all.
        let unix = ExecNaming::NoExtension;
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh", &unix).as_deref(),
            Some("ssh")
        );
        assert_eq!(
            provider_id_from_file_name("buzz-backend-my_provider-2", &unix).as_deref(),
            Some("my_provider-2")
        );
        assert!(provider_id_from_file_name("buzz-backend-", &unix).is_none());
        assert!(provider_id_from_file_name("buzz-agent", &unix).is_none());
        // A dotted name on unix stays dotted and is then rejected as an id —
        // it is not silently normalized into a different provider.
        let dotted = provider_id_from_file_name("buzz-backend-ssh.exe", &unix);
        assert_eq!(dotted.as_deref(), Some("ssh.exe"));
        assert!(!provider_id_is_valid(dotted.as_deref().unwrap()));
    }

    #[test]
    fn allowed_exec_extensions_honors_pathext_and_never_admits_scripts() {
        // Unset/empty → documented Windows default, minus the script types.
        assert_eq!(allowed_exec_extensions_from(None), vec!["com", "exe"]);
        assert_eq!(allowed_exec_extensions_from(Some("  ")), vec!["com", "exe"]);
        // Intersection: a PATHEXT that omits .COM omits it here too.
        assert_eq!(
            allowed_exec_extensions_from(Some(".EXE;.BAT;.CMD")),
            vec!["exe"]
        );
        // A user-configured PATHEXT cannot re-admit shell scripts.
        let scripts_only = allowed_exec_extensions_from(Some(".CMD;.BAT;.PS1;.VBS"));
        assert!(scripts_only.is_empty());
    }

    #[test]
    fn empty_windows_allowlist_discovers_nothing_rather_than_everything() {
        // Regression guard for the seam itself: an empty Windows allowlist and
        // the unix rule are NOT the same thing. If `PATHEXT` admits no safe
        // extension, no file is a provider — the opposite of the unix arm,
        // where the absence of extensions means the name is the id.
        let empty = ExecNaming::Extensions(Vec::new());
        assert!(provider_id_from_file_name("buzz-backend-ssh.cmd", &empty).is_none());
        assert!(provider_id_from_file_name("buzz-backend-ssh.exe", &empty).is_none());
        assert!(provider_id_from_file_name("buzz-backend-ssh", &empty).is_none());
        assert_eq!(
            provider_id_from_file_name("buzz-backend-ssh", &ExecNaming::NoExtension).as_deref(),
            Some("ssh")
        );
    }

    #[test]
    fn provider_id_is_valid_matches_resolve_provider_binary_rules() {
        assert!(provider_id_is_valid("ssh"));
        assert!(provider_id_is_valid("my_provider-2"));
        assert!(!provider_id_is_valid(""));
        assert!(!provider_id_is_valid("ssh.exe"));
        assert!(!provider_id_is_valid("MyProvider"));
        assert!(!provider_id_is_valid("-leading-dash"));
        assert!(!provider_id_is_valid("_leading_underscore"));
        assert!(!provider_id_is_valid("foo;rm -rf /"));
    }

    /// The whole discovery path against a real directory: `PATHEXT` ranking,
    /// id validation, executability and dedupe are unit-tested in isolation
    /// above, but only running `discover_provider_candidates` proves they
    /// compose — that a real `read_dir` entry survives all four and comes back
    /// as a usable (id, path) pair.
    #[cfg(unix)]
    #[test]
    fn discover_provider_candidates_finds_an_executable_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temp dir");
        let write = |name: &str, mode: u32| {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("chmod file");
            path
        };
        let provider = write("buzz-backend-zzztest", 0o755);
        // Non-executable, wrong prefix, and an id the resolver would reject —
        // each must be filtered by a different guard in the loop.
        write("buzz-backend-zzznotexec", 0o644);
        write("some-other-tool", 0o755);
        write("buzz-backend-ZZZUPPER", 0o755);

        // Prepended, so every other entry the process already had stays
        // visible and a concurrent test still resolves what it expects.
        let original_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{original_path}", dir.path().display()));
        let found = discover_provider_candidates();
        // The discovered pair is one `resolve_provider_binary` accepts — the
        // catalog never advertises a provider that cannot then be executed.
        let resolved = resolve_provider_binary("zzztest");
        std::env::set_var("PATH", &original_path);

        let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"zzztest"), "{ids:?}");
        assert!(!ids.contains(&"zzznotexec"), "{ids:?}");
        assert!(!ids.contains(&"ZZZUPPER"), "{ids:?}");
        assert_eq!(
            found.iter().find(|(id, _)| id == "zzztest").map(|(_, p)| p),
            Some(&provider)
        );
        assert_eq!(resolved.ok(), provider.canonicalize().ok());
    }

    #[test]
    fn same_directory_conflicts_resolve_by_pathext_precedence() {
        // `read_dir` order is undefined, so both listings of the same
        // directory must select the same file — the one Windows command
        // lookup would run, i.e. the extension `PATHEXT` lists first.
        let both = ["buzz-backend-foo.exe", "buzz-backend-foo.com"];
        for order in [both, [both[1], both[0]]] {
            let names = || order.iter().map(|s| s.to_string());

            let com_first = ExecNaming::Extensions(allowed_exec_extensions_from(Some(".COM;.EXE")));
            assert_eq!(
                provider_candidates_in_dir(names(), &com_first),
                vec![("foo".to_string(), "buzz-backend-foo.com".to_string())]
            );

            let exe_first = ExecNaming::Extensions(allowed_exec_extensions_from(Some(".EXE;.COM")));
            assert_eq!(
                provider_candidates_in_dir(names(), &exe_first),
                vec![("foo".to_string(), "buzz-backend-foo.exe".to_string())]
            );
        }
    }

    #[test]
    fn distinct_ids_in_a_directory_all_survive_dedup() {
        // Dedup collapses same-id conflicts only — unrelated providers in the
        // same directory must all be discovered, and non-providers dropped.
        let names = [
            "buzz-backend-zed.exe",
            "buzz-backend-ssh.com",
            "buzz-backend-ssh.exe",
            "buzz-backend-ssh.cmd", // script extension: never a provider
            "buzz-backend-Bad.exe", // uppercase id: resolve would reject it
            "unrelated.exe",
        ];
        let found = provider_candidates_in_dir(
            names.iter().map(|s| s.to_string()),
            &windows_naming(), // default PATHEXT → .COM before .EXE
        );
        assert_eq!(
            found,
            vec![
                ("ssh".to_string(), "buzz-backend-ssh.com".to_string()),
                ("zed".to_string(), "buzz-backend-zed.exe".to_string()),
            ]
        );
    }

    #[test]
    fn unix_directory_listing_needs_no_extension_ranking() {
        // The unix arm has no extension precedence: every name is its own id,
        // and ranking must not reorder or drop anything.
        let found = provider_candidates_in_dir(
            ["buzz-backend-ssh", "buzz-backend-zed"]
                .iter()
                .map(|s| s.to_string()),
            &ExecNaming::NoExtension,
        );
        assert_eq!(
            found,
            vec![
                ("ssh".to_string(), "buzz-backend-ssh".to_string()),
                ("zed".to_string(), "buzz-backend-zed".to_string()),
            ]
        );
    }

    #[test]
    fn resolve_provider_binary_rejects_invalid_ids() {
        // Path traversal
        assert!(resolve_provider_binary("../evil").is_err());
        // Empty
        assert!(resolve_provider_binary("").is_err());
        // Uppercase
        assert!(resolve_provider_binary("MyProvider").is_err());
        // Spaces
        assert!(resolve_provider_binary("my provider").is_err());
        // Shell metacharacters
        assert!(resolve_provider_binary("foo;rm -rf /").is_err());
        // Valid format but not on PATH — should fail with "not found"
        assert!(resolve_provider_binary("nonexistent-test-id-12345").is_err());
    }

    #[test]
    fn resolve_provider_binary_accepts_valid_id_format() {
        // Valid ID format should pass validation. If the binary happens to
        // exist on PATH, Ok is returned; otherwise Err contains "not found"
        // (not "invalid provider ID"). Either outcome proves validation passed.
        match resolve_provider_binary("zzz-nonexistent-test-provider") {
            Ok(_) => {} // unlikely but fine — binary exists
            Err(e) => assert!(
                e.contains("not found"),
                "expected 'not found' error, got: {e}"
            ),
        }
    }
}
