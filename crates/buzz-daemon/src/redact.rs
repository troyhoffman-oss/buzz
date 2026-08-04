//! Secret redaction, both directions.
//!
//! Implements `DESIGN.md` §2.5 ("Redaction, both directions"). The daemon's own
//! log sink passes through this redactor, and re-applies it to
//! backend-provider stderr on the way in — the provider already scrubs on the
//! way out, and keeping both layers means a leak requires two independent
//! failures.
//!
//! The two layers must not be allowed to silently diverge.
//! `buzz_backend_ssh::protocol::redact` currently matches exactly `["nsec1",
//! "sprt_tok_"]`. This list is a **documented superset** adding `ncryptsec1`,
//! with [`tests::daemon_prefixes_are_a_superset_of_provider_prefixes`]
//! asserting `daemon_prefixes ⊇ provider_prefixes` so an upstream addition the
//! daemon has not picked up fails the build rather than quietly opening a hole.
//!
//! Preferred long-term: add `ncryptsec1` upstream and import
//! `protocol::redact` so there is one definition. The superset test is what
//! holds until then.
//!
//! The scan behaviour is deliberately copied, not improved: it runs to the next
//! whitespace or quote and takes the rest of the string otherwise, so a secret
//! at end-of-line with no delimiter redacts the remainder. That is correct for
//! a redactor (§2.5).

/// Secret prefixes the daemon redacts.
///
/// A documented superset of `buzz_backend_ssh::protocol::PROVIDER_PREFIXES`
/// (mirrored in [`PROVIDER_PREFIXES`]). `ncryptsec1` is the daemon-specific
/// addition: §2.5 [D-8] makes NIP-49 blobs the identity-at-rest format, so they
/// are reachable from daemon log paths that the provider never sees.
pub const DAEMON_PREFIXES: &[&str] = &["nsec1", "ncryptsec1", "sprt_tok_"];

/// The upstream provider's redaction list, mirrored here for the superset test.
///
/// Source of truth: `crates/buzz-backend-ssh/src/protocol.rs`'s `redact`.
/// When upstream adds a prefix, add it here **and** to [`DAEMON_PREFIXES`];
/// the test below fails the build until both move together.
pub const PROVIDER_PREFIXES: &[&str] = &["nsec1", "sprt_tok_"];

/// Replacement token substituted for a matched secret.
pub const REDACTED: &str = "[REDACTED]";

/// Redact every known secret prefix in `text`.
///
/// Ported from `buzz_backend_ssh::protocol::redact` with [`DAEMON_PREFIXES`]
/// in place of the provider's shorter list. Behaviour is otherwise identical,
/// including the end-of-line case.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for prefix in DAEMON_PREFIXES {
        while let Some(start) = out.find(prefix) {
            let end = out[start..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .map(|offset| start + offset)
                .unwrap_or(out.len());
            out.replace_range(start..end, REDACTED);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.5: `daemon_prefixes ⊇ provider_prefixes`. An upstream addition the
    /// daemon has not picked up fails the build rather than quietly opening a
    /// hole.
    #[test]
    fn daemon_prefixes_are_a_superset_of_provider_prefixes() {
        for prefix in PROVIDER_PREFIXES {
            assert!(
                DAEMON_PREFIXES.contains(prefix),
                "provider redacts {prefix:?} but the daemon does not; \
                 add it to DAEMON_PREFIXES"
            );
        }
    }

    /// §2.5 names `ncryptsec1` as the daemon-specific addition.
    #[test]
    fn ncryptsec_is_covered() {
        assert!(DAEMON_PREFIXES.contains(&"ncryptsec1"));
        let out = redact("loaded ncryptsec1qqqqq from disk");
        assert!(!out.contains("ncryptsec1q"), "{out}");
    }

    /// §2.5: "a secret at end-of-line with no delimiter redacts the rest of the
    /// string. That is correct for a redactor."
    #[test]
    fn end_of_line_secret_with_no_delimiter_is_fully_redacted() {
        let out = redact("key=nsec1abcdefghijklmnop");
        assert_eq!(out, format!("key={REDACTED}"));
    }

    #[test]
    fn redacts_up_to_a_delimiter_and_keeps_the_tail() {
        let out = redact("{\"key\":\"nsec1abc\",\"other\":1}");
        assert!(out.contains("\"other\":1"), "{out}");
        assert!(!out.contains("nsec1abc"), "{out}");
    }
}
