//! Durable channel → ACP session bindings.
//!
//! One file per (agent pubkey, channel) under the platform data directory:
//! `<data_local_dir>/buzz-acp/sessions/<pubkey_hex>/<channel_uuid>`, whose whole
//! content is the session ID. Keying on the agent's own pubkey means two managed
//! agents are two Buzz identities and can never share a store, whatever command
//! they were spawned with.
//!
//! One value per file removes read-modify-write: writers for different channels
//! never touch the same bytes, and writers for the same channel resolve as
//! last-writer-wins over an atomic rename. That makes the map corruption-proof,
//! but it does not arbitrate ownership — running two harnesses on one identity
//! is unsupported here exactly as it is everywhere else.
//!
//! Every operation is best-effort: a lost binding costs one fresh session, so
//! failures warn and continue rather than failing a turn.

use std::path::{Path, PathBuf};

use uuid::Uuid;

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Store for the agent identified by `pubkey_hex`, or `None` when the
    /// platform reports no data directory — the harness then behaves exactly as
    /// it did before bindings were durable.
    pub fn for_agent(pubkey_hex: &str) -> Option<Self> {
        let root = dirs::data_local_dir().or_else(dirs::data_dir)?;
        Some(Self::in_root(&root, pubkey_hex))
    }

    /// Store rooted at an explicit directory.
    pub fn in_root(root: &Path, pubkey_hex: &str) -> Self {
        Self {
            dir: root.join("buzz-acp").join("sessions").join(pubkey_hex),
        }
    }

    /// The session ID bound to `channel_id`, if any.
    pub fn get(&self, channel_id: &Uuid) -> Option<String> {
        let contents = std::fs::read_to_string(self.path(channel_id)).ok()?;
        let trimmed = contents.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Bind `channel_id` to `session_id`, replacing any existing binding.
    pub fn put(&self, channel_id: &Uuid, session_id: &str) {
        if let Err(error) = self.write(channel_id, session_id) {
            tracing::warn!(
                target: "buzz_acp::pool::session",
                channel = %channel_id,
                "failed to persist session binding: {error}"
            );
        }
    }

    /// Drop `channel_id`'s binding. An absent binding is not an error.
    pub fn clear(&self, channel_id: &Uuid) {
        match std::fs::remove_file(self.path(channel_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                target: "buzz_acp::pool::session",
                channel = %channel_id,
                "failed to clear session binding: {error}"
            ),
        }
    }

    fn path(&self, channel_id: &Uuid) -> PathBuf {
        self.dir.join(channel_id.to_string())
    }

    /// Write through a PID-suffixed temporary — so concurrent writers neither
    /// collide on the temporary nor expose a partial file — then rename, which
    /// replaces the destination on every supported platform.
    fn write(&self, channel_id: &Uuid, session_id: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self
            .dir
            .join(format!("{channel_id}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, session_id)?;
        std::fs::rename(&tmp, self.path(channel_id)).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &tempfile::TempDir, pubkey_hex: &str) -> SessionStore {
        SessionStore::in_root(root.path(), pubkey_hex)
    }

    #[test]
    fn binding_survives_a_fresh_handle() {
        let root = tempfile::tempdir().unwrap();
        let channel = Uuid::new_v4();

        assert_eq!(store(&root, "aa").get(&channel), None);
        store(&root, "aa").put(&channel, "ses_1");
        assert_eq!(store(&root, "aa").get(&channel).as_deref(), Some("ses_1"));
    }

    #[test]
    fn bindings_are_isolated_per_pubkey() {
        let root = tempfile::tempdir().unwrap();
        let channel = Uuid::new_v4();

        store(&root, "aa").put(&channel, "ses_aa");
        store(&root, "bb").put(&channel, "ses_bb");

        assert_eq!(store(&root, "aa").get(&channel).as_deref(), Some("ses_aa"));
        assert_eq!(store(&root, "bb").get(&channel).as_deref(), Some("ses_bb"));
    }

    /// Two handles opened before either writes must not lose each other's
    /// bindings — the lost-update class that a read-modify-write map has.
    #[test]
    fn interleaved_writers_keep_every_binding() {
        let root = tempfile::tempdir().unwrap();
        let (one, two) = (store(&root, "aa"), store(&root, "aa"));
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

        one.put(&a, "ses_a");
        two.put(&b, "ses_b");
        one.put(&a, "ses_a2");

        assert_eq!(two.get(&a).as_deref(), Some("ses_a2"));
        assert_eq!(one.get(&b).as_deref(), Some("ses_b"));
    }

    #[test]
    fn clear_removes_the_binding_and_tolerates_absence() {
        let root = tempfile::tempdir().unwrap();
        let channel = Uuid::new_v4();

        store(&root, "aa").put(&channel, "ses_1");
        store(&root, "aa").clear(&channel);
        assert_eq!(store(&root, "aa").get(&channel), None);
        store(&root, "aa").clear(&channel);
    }

    #[test]
    fn blank_content_reads_as_absent() {
        let root = tempfile::tempdir().unwrap();
        let channel = Uuid::new_v4();

        store(&root, "aa").put(&channel, "  \n ");
        assert_eq!(store(&root, "aa").get(&channel), None);
    }

    /// An unusable root degrades to "no binding" instead of panicking.
    #[test]
    fn unwritable_root_degrades_to_absent() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let store = SessionStore::in_root(&blocker, "aa");
        let channel = Uuid::new_v4();

        store.put(&channel, "ses_1");
        assert_eq!(store.get(&channel), None);
        store.clear(&channel);
    }
}
