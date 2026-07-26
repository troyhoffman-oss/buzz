//! Installing `buzz-acp` on the host from a copy that lives on the desktop.
//!
//! Deploy used to *verify* `buzz-acp` and fail with guidance when the host had
//! none. It now verifies **or installs**: when the deploy payload carries
//! `buzz_acp_binary` — a path on the desktop machine to a Linux `buzz-acp` —
//! and the host resolves none, the binary rides along inside the same script
//! that already carries the agent's identity and is installed to
//! `~/.local/bin/buzz-acp`. There is no second op, no provisioning step, and no
//! new UI state: install is an invisible, idempotent property of deploy.
//!
//! The transport is the constraint that shapes everything here. The script
//! travels on the SSH stdin channel (`ssh.rs`) as text, so raw bytes cannot be
//! embedded: a NUL, a heredoc delimiter, or a stray newline in the middle of an
//! ELF section would corrupt the *script*, not just the payload. base64 makes
//! that impossible by construction — the encoded alphabet is
//! `A-Za-z0-9+/=`, which contains no shell metacharacter, no newline, and (the
//! detail that keeps the heredoc safe) no `_`, so no encoded line can collide
//! with the `BUZZ_ACP_B64_EOF` delimiter.
//!
//! **Staleness rule: push-when-missing only.** A host that already resolves
//! `buzz-acp` keeps the binary it has, whatever its version. Deploy is also the
//! start path — `start_managed_agent` re-enters it — so a version-comparing
//! rule would silently reinstall the binary underneath a running fleet on every
//! start, and a desktop pinned to an older artifact would *downgrade* the host.
//! Refreshing an existing install is a deliberate act and belongs to a follow-up
//! that fetches release artifacts by version; see `docs/remote-agents.md`.

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Refuse to embed anything larger than this. A release `buzz-acp` is 10-30 MB;
/// base64 inflates it by a third and the result travels as one script on the
/// SSH stdin channel, so a wrong path (a disk image, a core dump, a directory
/// of them) must fail here rather than stream for minutes and then fail on the
/// host.
const MAX_BINARY_BYTES: usize = 200 * 1024 * 1024;

/// base64 line width. GNU `base64` wraps at 76 by default and `-d` ignores
/// newlines; one 40 MB line is legal but pathological for anything that reads
/// the script line-wise — including this crate's own tests.
const LINE_WIDTH: usize = 76;

/// The heredoc delimiter for the encoded binary. It contains `_`, which is not
/// in the base64 alphabet, so no data line can ever terminate the heredoc
/// early. `delimiter_cannot_appear_in_encoded_data` pins that.
const DELIMITER: &str = "BUZZ_ACP_B64_EOF";

/// A `buzz-acp` binary read from the desktop's filesystem, encoded for the
/// script and fingerprinted for the host to check.
///
/// The bytes are not secret — but they must not corrupt the script stream that
/// *is* carrying secrets, which is why only the encoded form is kept.
///
/// Deliberately not `Debug`, like `deploy::Agent` and `ssh::Output`: a derived
/// one would put tens of megabytes of base64 one `{:?}` away from a log line.
pub struct Payload {
    /// base64, wrapped to [`LINE_WIDTH`], one trailing newline per line.
    encoded: String,
    /// Lowercase hex SHA-256 of the raw bytes. Travels in the script in the
    /// clear: it is a fingerprint, not a credential.
    sha256: String,
}

/// The size rejection, or `None` when `len` is within the cap.
///
/// Split out so the boundary is testable without materializing a 200 MB file,
/// and applied twice in [`Payload::read`] — once to the metadata, once to the
/// bytes actually read.
fn oversized(len: u64, path: &str) -> Option<String> {
    (len > MAX_BINARY_BYTES as u64).then(|| {
        format!(
            "the buzz-acp binary to push is {len} bytes, over the {MAX_BINARY_BYTES}-byte limit: \
             {path}"
        )
    })
}

impl Payload {
    /// Read, validate and encode the binary at `path` on the **desktop**.
    ///
    /// Every rejection here is a failure the host could only report as
    /// something far less legible: an `Exec format error` from systemd five
    /// seconds after a deploy that looked successful, or a multi-minute stream
    /// of a file that was never a binary.
    pub fn read(path: &str) -> Result<Self, String> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| format!("cannot read the buzz-acp binary to push ({path}): {e}"))?;
        if !metadata.is_file() {
            return Err(format!("the buzz-acp binary to push is not a file: {path}"));
        }
        // Checked before the read, so a wrong path costs a `stat` rather than
        // pulling a disk image into memory.
        if let Some(error) = oversized(metadata.len(), path) {
            return Err(error);
        }

        let bytes = std::fs::read(path)
            .map_err(|e| format!("cannot read the buzz-acp binary to push ({path}): {e}"))?;
        // Re-checked against the bytes actually read: the metadata above is a
        // separate syscall, and the file may have grown between the two.
        if let Some(error) = oversized(bytes.len() as u64, path) {
            return Err(error);
        }
        if bytes.is_empty() {
            return Err(format!("the buzz-acp binary to push is empty: {path}"));
        }
        // Deploy targets are Linux + `systemd --user` throughout, and the
        // desktop pushing the binary is routinely macOS or Windows. Without
        // this check a Mach-O or PE binary installs cleanly and the unit then
        // restart-loops on `Exec format error` every five seconds, with the
        // deploy having reported success.
        if !bytes.starts_with(b"\x7fELF") {
            return Err(format!(
                "the buzz-acp binary to push is not a Linux (ELF) executable: {path}"
            ));
        }

        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            encoded: wrap(&base64::engine::general_purpose::STANDARD.encode(&bytes)),
            sha256,
        })
    }

    /// The fingerprint the host checks the decoded file against. Tests assert
    /// against it; the script embeds it through [`resolve_or_install`].
    #[cfg(test)]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The encoded body, so tests can corrupt it the way a truncated stream
    /// would. The script embeds it through [`resolve_or_install`].
    #[cfg(test)]
    pub fn encoded(&self) -> &str {
        &self.encoded
    }
}

/// base64 output is ASCII, so chunking bytes cannot split a character.
fn wrap(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len() + encoded.len() / LINE_WIDTH + 1);
    for chunk in encoded.as_bytes().chunks(LINE_WIDTH) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 output is ASCII"));
        out.push('\n');
    }
    out
}

/// The script that asks the host whether it already resolves `buzz-acp`.
///
/// Deploy is also the *start* path, so without this a desktop with the push
/// seam engaged would stream tens of megabytes of base64 on every single agent
/// start, to a host that has had the binary since the first one. The probe is
/// one cheap round trip that keeps the payload off the wire in that case.
///
/// It is an optimization and never the decision: [`resolve_or_install`] still
/// re-checks on the host and installs only into an empty `$acp`, so a host that
/// gains or loses the binary between the two round trips still ends up correct.
pub fn probe_script(acp: &str) -> String {
    format!("command -v {acp} >/dev/null 2>&1\n")
}

/// The deploy script's `buzz-acp` resolution block.
///
/// `acp` is the already-`quote()`d command or path to resolve. With no payload
/// this is byte-for-byte the line the crate has always emitted, so a deploy
/// that carries no binary behaves exactly as it did before this module existed.
///
/// With a payload it becomes resolve-or-install, in that order: an installed
/// `buzz-acp` is never replaced, and a host that had none ends the block with
/// `$acp` holding the absolute path of the copy just installed — which is what
/// the unit's `ExecStart` is substituted from later in the same pass.
pub fn resolve_or_install(acp: &str, push: Option<&Payload>) -> String {
    const MISSING: &str = "buzz-acp not found on the server's PATH — install it, or set 'buzz-acp \
                           path on the server'";

    let Some(payload) = push else {
        return format!(
            r#"acp=$(command -v {acp} 2>/dev/null) || {{ echo "{MISSING}" >&2; exit 90; }}"#
        );
    };

    // Every failure below removes the temp file before exiting, and the file is
    // only made executable *after* the digest matches, so no path through this
    // block can leave a runnable half-written binary in `~/.local/bin`.
    //
    // The `|| { ... }` on the `base64` line binds to the whole redirected
    // command; the heredoc body begins on the following line either way, so the
    // decode is guarded rather than left to `set -e`, which would exit before
    // the temp file could be removed.
    format!(
        r#"acp=$(command -v {acp} 2>/dev/null || true)
if [ -z "$acp" ]; then
command -v base64 >/dev/null 2>&1 || {{ echo "the server has no 'base64' (coreutils), so the desktop cannot install buzz-acp on it" >&2; exit 92; }}
command -v sha256sum >/dev/null 2>&1 || {{ echo "the server has no 'sha256sum' (coreutils), and buzz-acp is never installed unverified" >&2; exit 92; }}
acp_dir="$HOME/.local/bin"
mkdir -p "$acp_dir"
acp_tmp="$acp_dir/.buzz-acp.tmp.$$"
base64 -d > "$acp_tmp" <<'{DELIMITER}' || {{ rm -f "$acp_tmp"; echo "the pushed buzz-acp did not decode on the server" >&2; exit 93; }}
{encoded}{DELIMITER}
printf '%s  %s\n' '{sha256}' "$acp_tmp" | sha256sum -c - >/dev/null 2>&1 || {{ rm -f "$acp_tmp"; echo "the pushed buzz-acp failed its sha256 check on the server — refusing to install it" >&2; exit 94; }}
chmod 755 "$acp_tmp"
mv "$acp_tmp" "$acp_dir/buzz-acp"
acp="$acp_dir/buzz-acp"
fi"#,
        encoded = payload.encoded,
        sha256 = payload.sha256,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that is a legal ELF header followed by everything that would
    /// break a shell script if it ever reached one unencoded.
    fn hostile_binary() -> Vec<u8> {
        let mut bytes = b"\x7fELF\x02\x01\x01\x00".to_vec();
        bytes.extend_from_slice(b"\0\0'\"$(touch /tmp/buzz-should-not-exist)`id`\r\n");
        bytes.extend_from_slice(format!("{DELIMITER}\n").as_bytes());
        bytes.extend_from_slice(b"\\x00 \x00 ${HOME} $(id -u)\n");
        bytes.extend_from_slice(&(0u8..=255).collect::<Vec<u8>>());
        bytes
    }

    fn write_temp(name: &str, bytes: &[u8]) -> String {
        let path = std::env::temp_dir().join(format!("buzz-push-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path.display().to_string()
    }

    /// `Payload` is intentionally not `Debug` (see its doc comment), so the
    /// rejection comes out by hand — the same pattern `deploy::tests` uses for
    /// `Agent`.
    fn rejection(path: &str) -> String {
        match Payload::read(path) {
            Err(error) => error,
            Ok(_) => panic!("expected {path} to be rejected, it was accepted"),
        }
    }

    #[test]
    fn encoding_round_trips_bytes_that_would_break_a_shell_script() {
        let bytes = hostile_binary();
        let payload = Payload::read(&write_temp("hostile", &bytes)).unwrap();

        // The encoded form carries nothing a shell reads as syntax, which is
        // the whole reason a binary can travel inside the script at all.
        for line in payload.encoded.lines() {
            assert!(
                line.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
                "encoded line left the base64 alphabet: {line}"
            );
            assert!(line.len() <= LINE_WIDTH, "unwrapped line: {}", line.len());
        }

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload.encoded.replace('\n', ""))
            .unwrap();
        assert_eq!(decoded, bytes, "base64 round trip lost bytes");
    }

    #[test]
    fn delimiter_cannot_appear_in_encoded_data() {
        // The heredoc is only safe because `_` is outside the base64 alphabet:
        // a payload that could emit its own terminator would end the heredoc
        // early and hand the rest of the binary to the shell as commands.
        assert!(DELIMITER.contains('_'));
        let payload = Payload::read(&write_temp("delimiter", &hostile_binary())).unwrap();
        // Even though the *source bytes* literally contain the delimiter.
        assert!(!payload.encoded.contains(DELIMITER));
    }

    #[test]
    fn the_digest_is_the_sha256_of_the_raw_bytes() {
        let bytes = hostile_binary();
        let payload = Payload::read(&write_temp("digest", &bytes)).unwrap();
        let expected: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(payload.sha256(), expected);
        assert_eq!(payload.sha256().len(), 64);
    }

    #[test]
    fn only_a_linux_executable_is_accepted() {
        // A Mach-O binary from the desktop installs cleanly and then
        // restart-loops on the host with `Exec format error`, five seconds at a
        // time, after a deploy that reported success.
        let error = rejection(&write_temp("macho", b"\xcf\xfa\xed\xfe rest"));
        assert!(error.contains("ELF"), "{error}");

        let error = rejection(&write_temp("empty", b""));
        assert!(error.contains("empty"), "{error}");

        let missing = std::env::temp_dir().join("buzz-push-does-not-exist");
        let error = rejection(&missing.display().to_string());
        assert!(error.contains("cannot read"), "{error}");

        // A directory `stat`s fine and `read` would fail with something far
        // less legible, so it is refused by shape rather than by errno.
        let error = rejection(&std::env::temp_dir().display().to_string());
        assert!(error.contains("not a file"), "{error}");
    }

    #[test]
    fn the_size_cap_rejects_at_the_boundary_and_names_the_path() {
        // Exercised through `oversized` rather than by writing a 200 MB file:
        // the boundary is the whole content of the rule, and a real artifact
        // (10-30 MB) must pass it untouched.
        assert_eq!(oversized(MAX_BINARY_BYTES as u64, "/x"), None);
        assert_eq!(oversized(30 * 1024 * 1024, "/x"), None);
        let error = oversized(MAX_BINARY_BYTES as u64 + 1, "/tmp/wrong-file").unwrap();
        assert!(error.contains("limit"), "{error}");
        assert!(error.contains("/tmp/wrong-file"), "{error}");
        // A `u64` length from a huge file must not wrap on the way to the
        // comparison, which an `as usize` on a 32-bit target would do.
        assert!(oversized(u64::MAX, "/x").is_some());
    }

    #[test]
    fn no_payload_emits_exactly_the_line_the_crate_always_emitted() {
        let resolved = resolve_or_install("'buzz-acp'", None);
        assert_eq!(
            resolved,
            r#"acp=$(command -v 'buzz-acp' 2>/dev/null) || { echo "buzz-acp not found on the server's PATH — install it, or set 'buzz-acp path on the server'" >&2; exit 90; }"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_answers_whether_the_host_already_has_the_binary() {
        // The probe is what keeps a megabytes-large payload off the wire on
        // every start of every agent, so its answer has to be right in both
        // directions. Run against a real `/bin/sh`, since the whole content of
        // the script is one `command -v`.
        let run = |script: &str, path: &str| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .env("PATH", path)
                .status()
                .unwrap()
                .success()
        };
        // `sh` itself is in /bin on every unix, so it stands in for an
        // installed binary without creating one.
        assert!(run(&probe_script("'sh'"), "/bin:/usr/bin"));
        assert!(!run(&probe_script("'buzz-acp'"), "/nonexistent"));
        // An absolute `buzz_acp_path` is answered by existence, not by PATH.
        assert!(run(&probe_script("'/bin/sh'"), "/nonexistent"));
        // The argument is interpolated already-quoted, so a hostile configured
        // path is inert rather than executed.
        let canary = std::env::temp_dir().join(format!("buzz-probe-{}", std::process::id()));
        let _ = std::fs::remove_file(&canary);
        let hostile = crate::ssh::quote(&format!("$(touch {})", canary.display()));
        assert!(!run(&probe_script(&hostile), "/bin:/usr/bin"));
        assert!(!canary.exists(), "the probe executed its own argument");
    }

    #[test]
    fn the_install_block_verifies_before_it_installs() {
        let payload = Payload::read(&write_temp("order", &hostile_binary())).unwrap();
        let script = resolve_or_install("'buzz-acp'", Some(&payload));

        let decode = script.find("base64 -d").unwrap();
        let verify = script.find("sha256sum -c").unwrap();
        let chmod = script.find("chmod 755").unwrap();
        let install = script.find(r#"mv "$acp_tmp""#).unwrap();
        assert!(decode < verify, "decode must precede verification");
        assert!(
            verify < chmod,
            "nothing becomes executable before it verifies"
        );
        assert!(chmod < install, "the file is executable before it is moved");

        // Same directory as the target, so the `mv` is a rename and never a
        // cross-device copy that could be observed half-written.
        assert!(script.contains(r#"acp_tmp="$acp_dir/.buzz-acp.tmp.$$""#));
        assert!(script.contains(r#"mv "$acp_tmp" "$acp_dir/buzz-acp""#));
        // Resolution wins over installation, so an existing binary is kept.
        assert!(script.contains(r#"if [ -z "$acp" ]; then"#));
        // And the freshly installed path is what the rest of the deploy uses.
        assert!(script.contains(r#"acp="$acp_dir/buzz-acp""#));
        // Missing coreutils is a clear message, never a silent skip.
        assert!(script.contains("command -v base64"));
        assert!(script.contains("command -v sha256sum"));

        // The heredoc delimiter is QUOTED, so the remote shell performs no
        // expansion on the body. The base64 alphabet already contains nothing
        // expandable, so this is the crate's usual second independent failure
        // rather than the only one — but an unquoted delimiter would make the
        // payload's inertness depend entirely on the encoder, and no
        // behavioural test could see the difference.
        assert!(script.contains(&format!("<<'{DELIMITER}'")));
        assert!(!script.contains(&format!("<<{DELIMITER}")));
    }
}
