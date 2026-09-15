//! Transactional local journal. Pending inbox reads roll back until output flushes.
use crate::{
    address::Address,
    envelope::{Envelope, Trust, MAX_ENVELOPE_BYTES},
    private_fs,
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const BATCH_SIZE: usize = 100;
const MAX_PENDING: i64 = 1000;

pub mod opencode;
pub mod registrations;

pub struct Store {
    conn: Connection,
    root: PathBuf,
    pub path: PathBuf,
}
pub struct InboxBatch {
    conn: Connection,
    pub messages: Vec<Envelope>,
    pub warnings: Vec<String>,
}
impl InboxBatch {
    /// Call only after the response was successfully flushed. Dropping the
    /// connection first rolls back; SQLite owns that cleanup on every error path.
    pub fn acknowledge(self) -> Result<()> {
        self.conn
            .execute_batch("COMMIT")
            .context("committing inbox receipt after output; delivery is uncertain if this fails")
    }
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let _directory = private_fs::directory(root)?;
        // Resolve trusted ancestor aliases such as macOS /var -> /private/var
        // only after rejecting a symlink at the state directory itself.
        let root = root
            .canonicalize()
            .context("resolving private state directory")?;
        let path = root.join("messages.sqlite");
        private_fs::check_state_file(&path)?;
        private_fs::check_sidecars(&path)?;
        // All ancestors within our state root are owner-only. No network sender
        // may supply a database path. Same-UID filesystem attackers are not isolated.
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .context("opening private message journal")?;
        // SQLite alone owns database descriptors. The 0700 parent protects a
        // newly created database until its permissions are tightened here.
        private_fs::check_state_file(&path)?;
        conn.busy_timeout(Duration::from_secs(3))
            .context("setting journal lock deadline")?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, envelope TEXT NOT NULL, outcome TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS inbox (id TEXT PRIMARY KEY REFERENCES messages(id), recipient TEXT NOT NULL, read INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS inbox_recipient ON inbox(recipient, read);
            CREATE TABLE IF NOT EXISTS registrations (
                address TEXT PRIMARY KEY, runtime TEXT NOT NULL, name TEXT NOT NULL,
                last_seen INTEGER NOT NULL, expires_at INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS registrations_runtime ON registrations(runtime, expires_at);
            CREATE TABLE IF NOT EXISTS opencode_routes (
                address TEXT PRIMARY KEY REFERENCES registrations(address) ON DELETE CASCADE,
                route TEXT NOT NULL);")
            .context("initializing message journal")?;
        private_fs::check_sidecars(&path)?;
        Ok(Self { conn, root, path })
    }

    pub fn prepare(&mut self, mut env: Envelope, reply_to: Option<Uuid>) -> Result<Envelope> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("locking reply journal")?;
        if let Some(id) = reply_to {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT envelope FROM messages WHERE id=?1",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            let raw = raw
                .context("reply_to is not in the local journal; refusing to reset its ancestry")?;
            let parent = decode(&raw)?;
            if parent.to != env.from || parent.from != env.to {
                bail!("reply participants do not match the original message");
            }
            env.conversation = parent.conversation;
            env.hop_chain = parent.hop_chain;
            env.reply_to = Some(id);
        }
        env.add_hop(env.from.clone())?;
        env.validate()?;
        tx.execute(
            "INSERT INTO messages(id,envelope,outcome) VALUES (?1,?2,'prepared')",
            params![env.id.to_string(), serde_json::to_string(&env)?],
        )
        .context("recording message before delivery")?;
        tx.commit().context("committing message before delivery")?;
        Ok(env)
    }

    pub fn outcome(&self, id: Uuid, value: &str) -> Result<()> {
        let changed = self
            .conn
            .execute(
                "UPDATE messages SET outcome=?2 WHERE id=?1",
                params![id.to_string(), value],
            )
            .context("recording delivery outcome")?;
        if changed != 1 {
            bail!("delivery journal entry is missing");
        }
        Ok(())
    }

    pub fn deposit(&mut self, addr: &Address, env: &Envelope) -> Result<()> {
        env.validate()?;
        if &env.to != addr {
            bail!("inbox recipient does not match envelope");
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("locking inbox deposit")?;
        insert_inbox(&tx, addr, env)?;
        tx.commit().context("committing inbox deposit")
    }

    pub fn inbox(mut self, addr: &Address, peek: bool) -> Result<InboxBatch> {
        let warnings = self.import_legacy(addr)?;
        // Connection-owned transaction: every early return/serialization/output
        // failure closes this connection and rolls back the read flags.
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .context("locking inbox read")?;
        let messages = {
            let mut stmt = self.conn.prepare("SELECT m.envelope FROM messages m JOIN inbox i ON i.id=m.id WHERE i.recipient=?1 AND i.read=0 ORDER BY m.rowid LIMIT ?2")?;
            let rows = stmt.query_map(params![addr.as_str(), BATCH_SIZE as i64], |r| {
                r.get::<_, String>(0)
            })?;
            let mut messages = Vec::new();
            for row in rows {
                let env = decode(&row.context("reading inbox row")?)?;
                if &env.to != addr {
                    bail!("journal recipient mismatch");
                }
                messages.push(env);
            }
            messages
        };
        if !peek {
            for env in &messages {
                let changed = self.conn.execute(
                    "UPDATE inbox SET read=1 WHERE id=?1 AND read=0",
                    [env.id.to_string()],
                )?;
                if changed != 1 {
                    bail!("inbox receipt changed during transaction");
                }
            }
        }
        Ok(InboxBatch {
            conn: self.conn,
            messages,
            warnings,
        })
    }

    fn import_legacy(&mut self, addr: &Address) -> Result<Vec<String>> {
        let parent = self.root.join("inbox");
        if !parent
            .try_exists()
            .context("checking legacy inbox directory")?
        {
            return Ok(Vec::new());
        }
        private_fs::directory(&parent)?;
        let old_slug: String = addr
            .as_str()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let dir = parent.join(old_slug);
        if !dir
            .try_exists()
            .context("checking legacy recipient directory")?
        {
            return Ok(Vec::new());
        }
        private_fs::directory(&dir)?;
        let mut warnings = Vec::new();
        for (index, entry) in fs::read_dir(&dir)
            .context("reading legacy inbox")?
            .enumerate()
        {
            if index >= 1000 {
                warnings.push(
                    "Legacy inbox scan stopped at 1000 entries; files were preserved.".into(),
                );
                break;
            }
            let path = entry.context("reading legacy inbox entry")?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let result = (|| -> Result<()> {
                let env = decode(&private_fs::read_owned(&path, MAX_ENVELOPE_BYTES)?)?;
                if &env.to != addr {
                    bail!("legacy filename collision: envelope belongs to a different address");
                }
                self.deposit(addr, &env)
            })();
            if let Err(e) = result {
                warnings.push(format!("Legacy message {path:?} was not imported: {e:#}"));
            }
        }
        // Original files remain recoverable. INSERT OR IGNORE preserves read
        // state on subsequent scans; importing is idempotent, never a requeue.
        Ok(warnings)
    }
}

fn decode(raw: &str) -> Result<Envelope> {
    if raw.len() > MAX_ENVELOPE_BYTES {
        bail!("stored envelope exceeds size limit");
    }
    let mut env: Envelope = serde_json::from_str(raw).context("invalid stored envelope")?;
    env.validate()?;
    env.trust = Trust::Untrusted;
    Ok(env)
}
fn insert_inbox(conn: &Connection, addr: &Address, env: &Envelope) -> Result<()> {
    let raw = serde_json::to_string(env)?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT envelope FROM messages WHERE id=?1",
            [env.id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if serde_json::to_string(&decode(&existing)?)? != raw {
            bail!("message id collision; refusing to overwrite journal entry");
        }
    } else {
        conn.execute(
            "INSERT INTO messages(id,envelope,outcome) VALUES (?1,?2,'inbox')",
            params![env.id.to_string(), raw],
        )?;
    }
    let already: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM inbox WHERE id=?1)",
        [env.id.to_string()],
        |r| r.get(0),
    )?;
    if !already {
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM inbox WHERE recipient=?1 AND read=0",
            [addr.as_str()],
            |r| r.get(0),
        )?;
        if count >= MAX_PENDING {
            bail!("recipient inbox is full ({MAX_PENDING} unread messages)");
        }
        conn.execute(
            "INSERT INTO inbox(id,recipient) VALUES (?1,?2)",
            params![env.id.to_string(), addr.as_str()],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Kind;
    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn draft() -> Envelope {
        Envelope::new("codex:a", "claude:b", Kind::Request, "hello".into()).unwrap()
    }
    #[test]
    fn real_journal_bounds_a_reply_exchange_and_checks_participants() {
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let mut env = store.prepare(draft(), None).unwrap();
        let conversation = env.conversation;
        for _ in 1..crate::envelope::MAX_HOPS {
            let next = Envelope::new(
                env.to.as_str(),
                env.from.as_str(),
                Kind::Reply,
                "reply".into(),
            )
            .unwrap();
            env = store.prepare(next, Some(env.id)).unwrap();
            assert_eq!(env.conversation, conversation);
        }
        let next = Envelope::new(
            env.to.as_str(),
            env.from.as_str(),
            Kind::Reply,
            "reply".into(),
        )
        .unwrap();
        assert!(store.prepare(next, Some(env.id)).is_err());
        assert!(store.prepare(draft(), Some(Uuid::new_v4())).is_err());
        assert!(store.prepare(draft(), Some(env.id)).is_err());
    }
    #[test]
    fn failed_output_rolls_back_and_duplicate_deposit_does_not_requeue() {
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let env = store.prepare(draft(), None).unwrap();
        store.deposit(&env.to, &env).unwrap();
        {
            let batch = Store::open(root.path())
                .unwrap()
                .inbox(&env.to, false)
                .unwrap();
            assert_eq!(batch.messages.len(), 1);
        }
        let batch = Store::open(root.path())
            .unwrap()
            .inbox(&env.to, false)
            .unwrap();
        assert_eq!(batch.messages.len(), 1);
        batch.acknowledge().unwrap();
        store.deposit(&env.to, &env).unwrap();
        assert!(Store::open(root.path())
            .unwrap()
            .inbox(&env.to, false)
            .unwrap()
            .messages
            .is_empty());
    }
    #[test]
    fn independent_connections_never_deliver_the_same_message_twice() {
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let addr: Address = "claude:b".parse().unwrap();
        for _ in 0..500 {
            let env = store.prepare(draft(), None).unwrap();
            store.deposit(&addr, &env).unwrap();
        }
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path = root.path().to_owned();
                let addr = addr.clone();
                std::thread::spawn(move || {
                    let mut ids = Vec::new();
                    loop {
                        let batch = Store::open(&path).unwrap().inbox(&addr, false).unwrap();
                        let empty = batch.messages.is_empty();
                        ids.extend(batch.messages.iter().map(|e| e.id));
                        batch.acknowledge().unwrap();
                        if empty {
                            break;
                        }
                    }
                    ids
                })
            })
            .collect();
        let ids: Vec<_> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 500);
        assert_eq!(
            ids.into_iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            500
        );
    }
    #[test]
    fn addresses_are_not_lossily_encoded() {
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let env = store
            .prepare(
                Envelope::new("a:sender", "a:alpha-beta", Kind::Inform, "hello".into()).unwrap(),
                None,
            )
            .unwrap();
        store.deposit(&env.to, &env).unwrap();
        assert!(Store::open(root.path())
            .unwrap()
            .inbox(&"a-alpha:beta".parse().unwrap(), false)
            .unwrap()
            .messages
            .is_empty());
    }

    #[test]
    fn legacy_import_is_idempotent_and_preserves_files_and_wrong_recipients() {
        let root = root();
        let mut env = draft();
        env.add_hop(env.from.clone()).unwrap();
        let dir = root.path().join("inbox/claude-b");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("message.json");
        fs::write(&path, serde_json::to_vec(&env).unwrap()).unwrap();
        let wrong = dir.join("wrong.json");
        let mut wrong_env = env.clone();
        wrong_env.id = Uuid::new_v4();
        wrong_env.to = "claude:elsewhere".parse().unwrap();
        fs::write(&wrong, serde_json::to_vec(&wrong_env).unwrap()).unwrap();
        let batch = Store::open(root.path())
            .unwrap()
            .inbox(&env.to, false)
            .unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.warnings.len(), 1);
        batch.acknowledge().unwrap();
        let batch = Store::open(root.path())
            .unwrap()
            .inbox(&env.to, false)
            .unwrap();
        assert!(batch.messages.is_empty());
        batch.acknowledge().unwrap();
        assert!(path.exists() && wrong.exists());
    }

    #[test]
    fn a_real_broken_output_socket_leaves_messages_unread() {
        use std::io::Write;
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let env = store.prepare(draft(), None).unwrap();
        store.deposit(&env.to, &env).unwrap();
        {
            let batch = Store::open(root.path())
                .unwrap()
                .inbox(&env.to, false)
                .unwrap();
            let (mut output, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            drop(peer);
            assert!(writeln!(
                output,
                "{}",
                serde_json::to_string(&batch.messages).unwrap()
            )
            .is_err());
        }
        let batch = Store::open(root.path())
            .unwrap()
            .inbox(&env.to, false)
            .unwrap();
        assert_eq!(batch.messages.len(), 1);
        batch.acknowledge().unwrap();
    }

    #[test]
    #[ignore = "subprocess helper; run by separate_processes_consume_once"]
    fn consumer_child() {
        use std::io::Write;
        let root =
            PathBuf::from(std::env::var_os("TELEPHONE_TEST_STORE").expect("test store required"));
        let result =
            PathBuf::from(std::env::var_os("TELEPHONE_TEST_RESULT").expect("test result required"));
        let addr = "claude:b".parse().unwrap();
        let mut ids = Vec::new();
        loop {
            let batch = Store::open(&root).unwrap().inbox(&addr, false).unwrap();
            let empty = batch.messages.is_empty();
            ids.extend(batch.messages.iter().map(|m| m.id));
            if std::env::var_os("TELEPHONE_TEST_CRASH").is_some() {
                assert!(!empty);
                // Deliberately bypass Drop: test recovery after process death,
                // not just RAII rollback in a cooperative reader.
                std::process::exit(73);
            }
            batch.acknowledge().unwrap();
            if empty {
                break;
            }
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(result)
            .unwrap();
        file.write_all(&serde_json::to_vec(&ids).unwrap()).unwrap();
    }

    #[test]
    fn separate_processes_consume_once() {
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let addr = "claude:b".parse().unwrap();
        for _ in 0..300 {
            let env = store.prepare(draft(), None).unwrap();
            store.deposit(&addr, &env).unwrap();
        }
        let jobs: Vec<_> = (0..3)
            .map(|i| {
                let path = root.path().to_owned();
                let output = path.join(format!("result-{i}"));
                std::thread::spawn(move || {
                    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                    command
                        .args(["--exact", "store::tests::consumer_child", "--ignored"])
                        .env("TELEPHONE_TEST_STORE", path)
                        .env("TELEPHONE_TEST_RESULT", &output);
                    let result = crate::process::run(command, Duration::from_secs(10)).unwrap();
                    assert!(
                        result.status.success(),
                        "{}",
                        String::from_utf8_lossy(&result.stdout)
                    );
                    serde_json::from_slice::<Vec<Uuid>>(&fs::read(output).unwrap()).unwrap()
                })
            })
            .collect();
        let ids: Vec<_> = jobs.into_iter().flat_map(|j| j.join().unwrap()).collect();
        assert_eq!(ids.len(), 300);
        assert_eq!(
            ids.into_iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            300
        );
    }

    #[test]
    fn process_death_before_commit_preserves_pending_messages() {
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let env = store.prepare(draft(), None).unwrap();
        store.deposit(&env.to, &env).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "store::tests::consumer_child", "--ignored"])
            .env("TELEPHONE_TEST_STORE", root.path())
            .env("TELEPHONE_TEST_RESULT", root.path().join("unused"))
            .env("TELEPHONE_TEST_CRASH", "1");
        let result = crate::process::run(command, Duration::from_secs(5)).unwrap();
        assert_eq!(result.status.code(), Some(73));
        let batch = Store::open(root.path())
            .unwrap()
            .inbox(&env.to, false)
            .unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].id, env.id);
        batch.acknowledge().unwrap();
    }

    #[test]
    fn inbox_limits_and_id_collisions_are_transactional() {
        let root = root();
        let mut store = Store::open(root.path()).unwrap();
        let mut original = draft();
        original.add_hop(original.from.clone()).unwrap();
        store.deposit(&original.to, &original).unwrap();
        let mut collision = original.clone();
        collision.body = "changed".into();
        assert!(store.deposit(&collision.to, &collision).is_err());
        for _ in 1..MAX_PENDING {
            let mut env = draft();
            env.add_hop(env.from.clone()).unwrap();
            store.deposit(&env.to, &env).unwrap();
        }
        let mut excess = draft();
        excess.add_hop(excess.from.clone()).unwrap();
        assert!(store.deposit(&excess.to, &excess).is_err());
        let exists: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1)",
                [excess.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !exists,
            "failed deposit must not leave a partial journal entry"
        );
        let batch = Store::open(root.path())
            .unwrap()
            .inbox(&original.to, true)
            .unwrap();
        assert_eq!(batch.messages[0].body, "hello");
        batch.acknowledge().unwrap();
    }

    #[test]
    fn database_and_sidecars_are_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let root = root();
        let store = Store::open(root.path()).unwrap();
        for name in [
            "messages.sqlite",
            "messages.sqlite-wal",
            "messages.sqlite-shm",
        ] {
            let path = root.path().join(name);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(store);
        let target = root.path().join("unrelated");
        fs::write(&target, "preserve").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("messages.sqlite-wal")).unwrap();
        assert!(Store::open(root.path()).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "preserve");
    }
}
