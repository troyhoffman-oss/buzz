//! Tailscale device enumeration, via the `tailscale` CLI.
//!
//! The CLI is used deliberately in place of the LocalAPI. LocalAPI means three
//! transports (unix socket on Linux, a named pipe on Windows, and a localhost
//! TCP port plus a `sameuserproof` token scavenged from `/Library/Tailscale`
//! on macOS GUI builds) plus a Host-header gate that 403s on the obvious
//! guesses. The CLI is one `Command::new`, one JSON parse, one code path.
//!
//! Everything here degrades silently. Tailscale being absent, logged out, or
//! stopped must leave the remote flow exactly as good as it is without
//! Tailscale — which is why every failure maps to "no devices" and never to an
//! error. `tailscale status --help` warns that `--json` "format [is] subject to
//! change", so every field is optional and a parse failure is just as quiet.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

/// `tailscale status --json`, reduced to the fields we consume.
#[derive(Debug, Default, Deserialize)]
struct StatusDoc {
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: Option<Tailnet_>,
    #[serde(rename = "Peer")]
    peer: Option<std::collections::BTreeMap<String, Peer>>,
}

#[derive(Debug, Default, Deserialize)]
struct Tailnet_ {
    #[serde(rename = "MagicDNSEnabled")]
    magic_dns_enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct Peer {
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "OS")]
    os: Option<String>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    /// Present only when the node advertises Tailscale SSH. Absence is the
    /// negative signal: it means "not SSH-ready", not "unknown".
    #[serde(rename = "sshHostKeys")]
    ssh_host_keys: Option<Vec<String>>,
}

/// A peer that could plausibly host an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The address to hand to `ssh`: MagicDNS FQDN when available, else the
    /// first Tailscale IP.
    pub address: String,
    /// What the picker shows. Tailscale-SSH readiness is folded in here rather
    /// than carried as a separate field: the schema's `oneOf` entries are
    /// `{const, title}` pairs, so the label is the only channel to the user.
    pub label: String,
    /// Ordering only — the label already says it. Kept separate so sorting
    /// never has to parse its own rendering.
    online: bool,
}

/// The set of tailnet peers usable as SSH targets. Empty whenever Tailscale is
/// missing, logged out, stopped, or has nothing that can host an agent.
#[derive(Debug, Clone, Default)]
pub struct Tailnet {
    devices: Vec<Device>,
}

impl Tailnet {
    /// Run `tailscale status --json` and parse it. Never fails.
    pub fn detect() -> Self {
        let Some(binary) = resolve_cli(&cli_candidates(), &|path: &PathBuf| path.is_file()) else {
            return Self::default();
        };
        let mut command = Command::new(binary);
        command.arg("status").arg("--json");
        crate::ssh::hide_console_window(&mut command);
        // We never branch on the exit code: a logged-out daemon exits 0 with
        // `BackendState: "NeedsLogin"` and `Peer: null`, while a missing daemon
        // socket exits 1. `parse` handles both by looking at the document.
        match command.output() {
            Ok(output) => Self::parse(&String::from_utf8_lossy(&output.stdout)),
            Err(_) => Self::default(),
        }
    }

    /// Pure half of [`Tailnet::detect`], over the raw `--json` document.
    pub fn parse(stdout: &str) -> Self {
        let Ok(doc) = serde_json::from_str::<StatusDoc>(stdout) else {
            return Self::default();
        };
        if doc.backend_state.as_deref() != Some("Running") {
            return Self::default();
        }
        let magic_dns = doc
            .current_tailnet
            .as_ref()
            .and_then(|t| t.magic_dns_enabled)
            .unwrap_or(false);

        // `Self` is deliberately not in `Peer`, so "this computer" never shows
        // up as a remote host.
        let mut devices: Vec<Device> = doc
            .peer
            .unwrap_or_default()
            .into_values()
            .filter_map(|peer| device_from_peer(&peer, magic_dns))
            .collect();
        // Online first, then by label, so the list opens on what is usable now.
        // Ordering reads the `online` flag rather than sniffing the rendered
        // label: a host named "· offline" would otherwise sort itself last.
        devices.sort_by(|a, b| {
            a.online
                .cmp(&b.online)
                .reverse()
                .then_with(|| a.label.cmp(&b.label))
        });
        Self { devices }
    }

    #[cfg(test)]
    fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// True when `host` is one of the enumerated tailnet addresses.
    ///
    /// This gates `StrictHostKeyChecking=accept-new`: a tailnet address is
    /// reached over an already-WireGuard-authenticated transport, so TOFU adds
    /// nothing. A manually typed host keeps the user's own known-hosts
    /// semantics, where an unknown key is a decision, not a default.
    pub fn contains(&self, host: &str) -> bool {
        self.devices
            .iter()
            .any(|device| device.address.eq_ignore_ascii_case(host))
    }

    /// The `oneOf` decoration for the `ssh_host` schema property. Empty when
    /// there is nothing to offer, which drops the key entirely and leaves the
    /// desktop rendering today's plain text field.
    pub fn schema_options(&self) -> Vec<serde_json::Value> {
        self.devices
            .iter()
            .map(|device| serde_json::json!({ "const": device.address, "title": device.label }))
            .collect()
    }
}

fn device_from_peer(peer: &Peer, magic_dns: bool) -> Option<Device> {
    let os = peer.os.as_deref().unwrap_or("");
    // Phones and TVs are tailnet members but cannot host an agent.
    if matches!(os.to_ascii_lowercase().as_str(), "ios" | "android" | "tvos") {
        return None;
    }

    let fqdn = peer
        .dns_name
        .as_deref()
        .map(|name| name.trim_end_matches('.'))
        .filter(|name| !name.is_empty());
    let ip = peer
        .tailscale_ips
        .as_ref()
        .and_then(|ips| ips.first())
        .map(String::as_str);
    let address = match (magic_dns, fqdn, ip) {
        (true, Some(fqdn), _) => fqdn,
        (_, _, Some(ip)) => ip,
        (false, Some(fqdn), None) => fqdn,
        _ => return None,
    }
    .to_string();

    let name = peer
        .host_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&address);
    let tailscale_ssh = peer
        .ssh_host_keys
        .as_ref()
        .is_some_and(|keys| !keys.is_empty());

    let online = peer.online.unwrap_or(false);
    let mut label = name.to_string();
    if !os.is_empty() {
        label.push_str(" — ");
        label.push_str(os);
    }
    label.push_str(if online { " · online" } else { " · offline" });
    if tailscale_ssh {
        label.push_str(" · Tailscale SSH");
    }

    Some(Device {
        address,
        label,
        online,
    })
}

/// Where the `tailscale` CLI lives when PATH does not have it. macOS GUI apps
/// inherit a minimal launchd PATH and the App Store build only ships the CLI
/// inside the bundle; Windows registers an install dir but not a PATH entry.
/// Mirrors the explicit-candidates pattern the desktop already uses for
/// provider discovery.
fn cli_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let exe = if cfg!(windows) {
        "tailscale.exe"
    } else {
        "tailscale"
    };
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(exe)));
    }
    if cfg!(windows) {
        let program_files = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        candidates.push(program_files.join("Tailscale").join(exe));
    } else {
        candidates.push(PathBuf::from("/usr/bin/tailscale"));
        candidates.push(PathBuf::from("/usr/local/bin/tailscale"));
        candidates.push(PathBuf::from(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        ));
    }
    candidates
}

/// First existing candidate. `exists` is injected so path resolution is
/// testable off-platform — a Windows install dir is data here, not a
/// filesystem fact.
fn resolve_cli(candidates: &[PathBuf], exists: &dyn Fn(&PathBuf) -> bool) -> Option<PathBuf> {
    candidates.iter().find(|path| exists(path)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = r#"{
      "BackendState": "Running",
      "CurrentTailnet": { "MagicDNSEnabled": true },
      "Self": { "HostName": "vmi3160506", "OS": "linux", "Online": true },
      "Peer": {
        "nodekey:aa": { "HostName": "troys-mac-mini", "DNSName": "troys-mac-mini.tailcfd703.ts.net.",
                        "OS": "macOS", "Online": false, "TailscaleIPs": ["100.64.0.2"] },
        "nodekey:bb": { "HostName": "troys_machine", "DNSName": "troys-machine.tailcfd703.ts.net.",
                        "OS": "windows", "Online": true, "TailscaleIPs": ["100.121.179.68"] },
        "nodekey:cc": { "HostName": "localhost", "DNSName": "iphone181.tailcfd703.ts.net.",
                        "OS": "iOS", "Online": true, "TailscaleIPs": ["100.64.0.4"] },
        "nodekey:dd": { "HostName": "vps-prod", "DNSName": "vps-prod.tailcfd703.ts.net.",
                        "OS": "linux", "Online": true, "TailscaleIPs": ["100.64.0.5"],
                        "sshHostKeys": ["ssh-ed25519 AAAA"] }
      }
    }"#;

    /// A logged-out daemon exits 0. Branching on the exit code gets this wrong.
    const NEEDS_LOGIN: &str = r#"{
      "BackendState": "NeedsLogin",
      "Health": ["Tailscale is stopped."],
      "Peer": null
    }"#;

    #[test]
    fn running_tailnet_yields_hostable_peers_only() {
        let tailnet = Tailnet::parse(RUNNING);
        let addresses: Vec<&str> = tailnet
            .devices()
            .iter()
            .map(|d| d.address.as_str())
            .collect();
        // iOS is filtered out; `Self` was never a peer to begin with.
        assert_eq!(
            addresses,
            [
                "troys-machine.tailcfd703.ts.net",
                "vps-prod.tailcfd703.ts.net",
                "troys-mac-mini.tailcfd703.ts.net",
            ]
        );
        // Online first, offline last.
        assert!(tailnet.devices()[2].label.contains("· offline"));
    }

    #[test]
    fn ordering_reads_the_online_flag_not_the_rendered_label() {
        // A hostname that happens to contain the offline marker must not sort
        // itself last — the flag decides, never the label text.
        let doc = RUNNING.replace(
            "\"HostName\": \"vps-prod\"",
            "\"HostName\": \"a · offline\"",
        );
        let tailnet = Tailnet::parse(&doc);
        let devices = tailnet.devices();
        assert!(
            devices[0].label.starts_with("a · offline"),
            "{:?}",
            devices[0].label
        );
        assert!(devices[0].label.ends_with("· online · Tailscale SSH"));
        assert!(devices[2].label.contains("· offline"));
    }

    #[test]
    fn ssh_host_keys_drive_the_tailscale_ssh_marker() {
        let tailnet = Tailnet::parse(RUNNING);
        let vps = tailnet
            .devices()
            .iter()
            .find(|d| d.address.starts_with("vps-prod"))
            .unwrap();
        assert_eq!(vps.label, "vps-prod — linux · online · Tailscale SSH");

        // Absent `sshHostKeys` means not SSH-ready, not unknown — so the peer
        // is still offered, just without the marker.
        let windows = tailnet
            .devices()
            .iter()
            .find(|d| d.address.starts_with("troys-machine"))
            .unwrap();
        assert_eq!(windows.label, "troys_machine — windows · online");
    }

    #[test]
    fn magic_dns_disabled_falls_back_to_the_tailscale_ip() {
        let doc = RUNNING.replace("\"MagicDNSEnabled\": true", "\"MagicDNSEnabled\": false");
        let tailnet = Tailnet::parse(&doc);
        assert!(tailnet
            .devices()
            .iter()
            .all(|d| d.address.starts_with("100.")));
    }

    #[test]
    fn non_running_and_garbage_documents_yield_nothing_quietly() {
        for doc in [
            NEEDS_LOGIN,
            r#"{"BackendState":"Stopped","Peer":{}}"#,
            "{\"BackendState\": \"Runn",
            "",
            "not json at all",
        ] {
            let tailnet = Tailnet::parse(doc);
            assert!(
                tailnet.devices().is_empty(),
                "unexpected devices for {doc:?}"
            );
            assert!(tailnet.schema_options().is_empty());
        }
    }

    #[test]
    fn schema_options_are_const_title_pairs() {
        let options = Tailnet::parse(RUNNING).schema_options();
        assert_eq!(options.len(), 3);
        assert_eq!(options[0]["const"], "troys-machine.tailcfd703.ts.net");
        assert!(options[0]["title"].as_str().unwrap().contains("windows"));
    }

    #[test]
    fn contains_matches_only_enumerated_addresses() {
        let tailnet = Tailnet::parse(RUNNING);
        assert!(tailnet.contains("VPS-PROD.tailcfd703.ts.net"));
        assert!(!tailnet.contains("vps.example.com"));
        assert!(!Tailnet::parse(NEEDS_LOGIN).contains("vps-prod.tailcfd703.ts.net"));
    }

    #[test]
    fn cli_resolution_prefers_the_first_existing_candidate() {
        let candidates = vec![
            PathBuf::from("/nope/tailscale"),
            PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe"),
            PathBuf::from("/usr/bin/tailscale"),
        ];
        let found = resolve_cli(&candidates, &|path| {
            path.to_string_lossy().contains("Program Files")
        });
        assert_eq!(found, Some(candidates[1].clone()));
        assert_eq!(resolve_cli(&candidates, &|_| false), None);
    }

    #[test]
    fn cli_candidates_cover_the_platform_install_locations() {
        let candidates = cli_candidates();
        let joined = candidates
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("|");
        if cfg!(windows) {
            assert!(joined.contains("Tailscale\\tailscale.exe"));
        } else {
            assert!(joined.contains("/usr/bin/tailscale"));
            assert!(joined.contains("/Applications/Tailscale.app"));
        }
    }
}
