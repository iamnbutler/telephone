//! Codex adapter.
//!
//! Codex is the hard case, and the one that decides whether "agent-agnostic"
//! means anything: it has no inter-session peer protocol. There is no socket
//! to connect to and no way to push a turn into a running thread from outside.
//!
//! So discovery and delivery come from completely different places:
//!
//! - **Discovery** reads Codex's own thread registry, the `threads` table in
//!   `~/.codex/state_<n>.sqlite`, which carries the thread id, cwd, title and
//!   last-updated time. When that database isn't readable -- Codex holds it
//!   open in WAL mode -- we fall back to parsing the `session_meta` line at the
//!   head of each rollout log under `~/.codex/sessions/`.
//! - **Delivery** uses `codex queue --thread <id> --message <text>`, which
//!   pushes a message into a thread's queue. This works even for a thread that
//!   isn't currently open -- Codex delivers it when the thread next runs.
//!   When the CLI isn't available we fall back to a filesystem inbox that
//!   Codex drains via the telephone MCP server's `check_inbox` tool.
//!
//! `codex queue` is a real push channel, so Codex is not the pull-only case it
//! first appears to be. The difference from Claude Code's socket is latency and
//! confirmation: we learn the message was queued, not that it was seen.
//!
//! ## Why liveness here is inferred, not confirmed
//!
//! Discovery reports Codex threads as [`Liveness::Inferred`] -- `recent?` in
//! `telephone list` -- and that is a deliberate stop, not a gap waiting to be
//! filled. Recency cannot distinguish a running thread from one that exited a
//! second after its last write, and the alternatives were investigated and
//! ruled out:
//!
//! **The app-server protocol has the right answer but not for us.**
//! `thread/list` returns a runtime status of `notLoaded`, `idle`, `active` or
//! `systemError`. But that status is only known to the daemon that actually
//! loaded and ran the thread. Spawning an app-server to ask is useless: a
//! fresh instance owns nothing and returns `notLoaded` for every thread on the
//! machine. A *managed* daemon (`codex app-server daemon start`) can answer,
//! but only for threads it runs itself, which means it must already have been
//! running when the session started. Started on demand it owns nothing, so
//! there is no version of on-demand that works.
//!
//! Reaching that daemon, should anyone need to: its control socket speaks
//! WebSocket over a Unix socket, not raw JSON. A correct upgrade handshake
//! returns `101 Switching Protocols`, after which the app-server protocol runs
//! inside text frames. Plain JSON writes are silently ignored, which makes a
//! naive attempt look like a hang rather than an error.
//!
//! **The ChatGPT desktop app owns its threads and does not expose them.** Its
//! app-server runs as a child of the app, spoken to over stdio pipes, so there
//! is no socket to query. The app's own `~/.codex/ipc/ipc.sock` is a separate
//! private router and was ruled out on three grounds: frames are a
//! little-endian `u32` byte count followed by JSON rather than newline-
//! delimited (a request opening `{"id` reads as a length of 1,684,611,707,
//! which is why a naive handshake gets an instant EOF); the router answers
//! thread *ownership*, not thread status, so even a correct handshake would
//! not yield the signal; and connecting broadcasts a client-status change to
//! every other connected client, so it is not an invisible read.
//!
//! The honest summary: for desktop-owned threads this is not currently
//! knowable from outside, and `notLoaded` means "cannot tell", never "dead".

use crate::envelope::Envelope;
use crate::inbox;
use crate::registry::{Adapter, Agent, Delivered, Liveness, Status, Transport};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const RUNTIME: &str = "codex";

/// How recently a thread must have been touched for us to list it.
///
/// Codex records no liveness at all, so this is a heuristic. Agents found this
/// way are reported with `unknown` status rather than `idle`, because claiming
/// to know would be worse than admitting we don't.
const LIVE_WINDOW_MILLIS: u64 = 30 * 60 * 1000;

/// Changes the default recency window in minutes; --all bypasses it.
const WINDOW_ENV: &str = "TELEPHONE_WINDOW_MINS";

fn live_window_millis() -> Result<u64> {
    match std::env::var(WINDOW_ENV) {
        Ok(value) => value
            .parse::<u64>()
            .context("TELEPHONE_WINDOW_MINS must be an unsigned integer")?
            .checked_mul(60_000)
            .context("TELEPHONE_WINDOW_MINS is too large"),
        Err(std::env::VarError::NotPresent) => Ok(LIVE_WINDOW_MILLIS),
        Err(e) => Err(e).context("reading TELEPHONE_WINDOW_MINS"),
    }
}

pub struct Codex {
    codex_home: PathBuf,
}

impl Codex {
    pub fn new() -> Result<Self> {
        // Respect CODEX_HOME so this works for non-default installs.
        let codex_home = match std::env::var_os("CODEX_HOME") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => dirs::home_dir()
                .context("no home directory")?
                .join(".codex"),
        };
        Ok(Codex { codex_home })
    }

    /// Codex versions its state database (`state_5.sqlite`, and so on), so
    /// pick the highest version present rather than pinning to one.
    fn state_db(&self) -> Result<Option<PathBuf>> {
        let mut best: Option<(u32, PathBuf)> = None;
        for entry in fs::read_dir(&self.codex_home).context("reading Codex directory")? {
            let path = entry.context("reading Codex directory entry")?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(rest) = name.strip_prefix("state_") else {
                continue;
            };
            let Some(version) = rest.strip_suffix(".sqlite") else {
                continue;
            };
            let Ok(version) = version.parse::<u32>() else {
                continue;
            };
            if best.as_ref().is_none_or(|(b, _)| version > *b) {
                best = Some((version, path));
            }
        }
        Ok(best.map(|(_, p)| p))
    }

    fn discover_via_sqlite(&self, cutoff: u64) -> Result<Vec<Agent>> {
        let db = self.state_db()?.context("no state database")?;
        let metadata = fs::symlink_metadata(&db).context("inspecting Codex database")?;
        // SAFETY: geteuid has no arguments or caller-owned memory.
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!("Codex database must be a regular file owned by this user");
        }
        // SQLite NOFOLLOW rejects ancestor aliases too (e.g. macOS /var).
        // Resolve those only after rejecting a symlink at the database itself.
        let db = db.canonicalize().context("resolving Codex database path")?;

        // Codex holds this open in WAL mode. Read-only is both correct and
        // the only safe thing to do to another process's live database.
        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", db.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;

        let mut stmt = conn.prepare(
            "SELECT id, cwd, \
                    COALESCE(NULLIF(agent_nickname, ''), '') AS label, \
                    COALESCE(updated_at_ms, updated_at * 1000) AS updated \
             FROM threads \
             WHERE archived = 0 \
               AND COALESCE(updated_at_ms, updated_at * 1000) >= ?1 \
             ORDER BY updated DESC",
        )?;

        let rows = stmt.query_map([cutoff as i64], |row| {
            let id: String = row.get(0)?;
            let cwd: Option<String> = row.get(1)?;
            let label: String = row.get(2)?;
            let updated: i64 = row.get(3)?;
            Ok((id, cwd, label, updated))
        })?;

        let mut agents = Vec::new();
        let native = codex_cli().is_some();
        for row in rows {
            let (id, cwd, label, updated) = row?;
            agents.push(build_agent(
                &id,
                cwd,
                &label,
                updated.max(0) as u64,
                native,
            )?);
        }
        Ok(agents)
    }

    /// Fallback for when the state database can't be read: every live thread
    /// appends to a rollout log whose first line is a `session_meta` record.
    fn discover_via_rollouts(&self, cutoff: u64) -> Result<Vec<Agent>> {
        let sessions = self.codex_home.join("sessions");
        if !sessions
            .try_exists()
            .context("checking rollout directory")?
        {
            return Ok(Vec::new());
        }

        let mut agents = Vec::new();
        let native = codex_cli().is_some();
        let mut stack = vec![sessions];
        while let Some(dir) = stack.pop() {
            let entries =
                fs::read_dir(&dir).with_context(|| format!("reading rollout directory {dir:?}"))?;
            for entry in entries {
                let path = entry.context("reading rollout entry")?.path();
                let metadata = fs::symlink_metadata(&path).context("inspecting rollout entry")?;
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                let is_rollout = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
                if !is_rollout {
                    continue;
                }
                let modified = mtime_millis(&path)?;
                if modified < cutoff {
                    continue;
                }
                // The meta record is the first line; don't read a whole
                // transcript just to learn a session id.
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(&path)
                    .with_context(|| format!("opening rollout {path:?}"))?;
                let mut first = String::new();
                {
                    use std::io::BufRead;
                    use std::io::Read;
                    let mut reader = std::io::BufReader::new(file).take(1024 * 1024 + 1);
                    reader
                        .read_line(&mut first)
                        .with_context(|| format!("reading rollout metadata {path:?}"))?;
                    if first.len() > 1024 * 1024 {
                        crate::warn(format!("rollout metadata exceeds size limit: {path:?}"));
                        continue;
                    }
                }
                let line = match serde_json::from_str::<RolloutLine>(&first) {
                    Ok(line) => line,
                    Err(e) => {
                        crate::warn(format!("skipping malformed rollout {path:?}: {e}"));
                        continue;
                    }
                };
                if line.kind != "session_meta" {
                    continue;
                }
                let meta = match serde_json::from_value::<SessionMeta>(line.payload) {
                    Ok(meta) => meta,
                    Err(e) => {
                        crate::warn(format!("skipping invalid session metadata {path:?}: {e}"));
                        continue;
                    }
                };
                let id = meta
                    .id
                    .or(meta.session_id)
                    .context("rollout metadata has no thread id")?;
                agents.push(build_agent(&id, meta.cwd, "", modified, native)?);
            }
        }
        agents.sort_by(|a, b| a.addr.cmp(&b.addr).then(b.last_seen.cmp(&a.last_seen)));
        agents.dedup_by(|a, b| a.addr == b.addr);
        Ok(agents)
    }
}

#[derive(Debug, Deserialize)]
struct RolloutLine {
    #[serde(rename = "type")]
    kind: String,
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct SessionMeta {
    id: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
}

fn build_agent(
    id: &str,
    cwd: Option<String>,
    label: &str,
    last_seen: u64,
    native: bool,
) -> Result<Agent> {
    let address: crate::address::Address = format!("{RUNTIME}:{id}").parse()?;
    // Names must be stable: Codex's `name` column holds an auto-generated
    // title that it rewrites as the thread evolves, so routing on it would
    // change an agent's address under the user mid-conversation. Only
    // `agent_nickname`, which a human sets deliberately, is stable enough.
    // Everything else gets a short id prefix.
    let short: String = id.chars().take(8).collect();
    let name = if label.is_empty() {
        format!("codex-{short}")
    } else {
        format!("codex-{}", slugify(label))
    };
    Ok(Agent {
        addr: address.to_string(),
        runtime: RUNTIME,
        name,
        cwd: cwd.map(PathBuf::from),
        // Recency cannot distinguish a running thread from one that exited
        // just after its last write, so status stays unknown and liveness
        // stays inferred. See the note on liveness at the top of this module.
        status: Status::Unknown,
        liveness: Liveness::Inferred,
        last_seen,
        transports: if native {
            vec![Transport::CodexQueue, Transport::Inbox]
        } else {
            vec![Transport::Inbox]
        },
    })
}

/// Nicknames are free text. Names are for typing at a shell, so keep them
/// short and shell-safe.
fn slugify(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    cleaned
        .split('-')
        .filter(|p| !p.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
}

fn mtime_millis(path: &PathBuf) -> Result<u64> {
    let modified = fs::metadata(path)
        .and_then(|m| m.modified())
        .context("reading rollout modification time")?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .context("rollout modification time precedes epoch")?
        .as_millis() as u64)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Adapter for Codex {
    fn runtime(&self) -> &'static str {
        RUNTIME
    }

    fn discover(&self) -> Result<Vec<Agent>> {
        if !self
            .codex_home
            .try_exists()
            .context("checking Codex directory")?
        {
            return Ok(Vec::new());
        }
        let cutoff = now_millis().saturating_sub(live_window_millis()?);

        self.discover_since(cutoff)
    }

    fn discover_all(&self) -> Result<Vec<Agent>> {
        self.discover_since(0)
    }

    fn deliver(&self, agent: &Agent, env: &Envelope) -> Result<Delivered> {
        env.validate()?;
        if env.to.as_str() != agent.addr {
            anyhow::bail!("Codex target does not match envelope recipient");
        }
        let thread = agent
            .addr
            .strip_prefix("codex:")
            .context("invalid Codex address")?;
        if let Some(cli) = codex_cli() {
            // Capability probing is read-only. A missing/unsupported command is
            // safe to fall back from; an actual send failure is not.
            let mut probe = Command::new(&cli);
            probe.args(["queue", "--help"]);
            match crate::process::run(probe, std::time::Duration::from_secs(3)) {
                Ok(out) if out.status.success() => {
                    queue_message(&cli, thread, env)?;
                    return Ok(Delivered::Accepted { via: "codex queue" });
                }
                Ok(_) => crate::debug(|| "codex queue is unsupported; using inbox".into()),
                Err(e) => {
                    crate::debug(|| format!("codex capability probe failed: {e}; using inbox"))
                }
            }
        }
        let path = inbox::deposit(&agent.addr, env)?;
        Ok(Delivered::Queued {
            path,
            note: "native queue unavailable; recipient must check its Telephone inbox".into(),
        })
    }
}

impl Codex {
    fn discover_since(&self, cutoff: u64) -> Result<Vec<Agent>> {
        if !self
            .codex_home
            .try_exists()
            .context("checking Codex home")?
        {
            return Ok(Vec::new());
        }
        match self.discover_via_sqlite(cutoff) {
            Ok(agents) => Ok(agents),
            Err(e) => {
                crate::debug(|| format!("codex: state db unreadable ({e}); using rollouts"));
                self.discover_via_rollouts(cutoff)
            }
        }
    }
}

/// Locates the Codex CLI.
///
/// Codex ships inside the ChatGPT desktop app as well as standalone, so an
/// installed-and-working Codex often isn't on `PATH` at all.
fn codex_cli() -> Option<PathBuf> {
    // Codex sets this in the environment of MCP servers it spawns, which makes
    // it the most reliable source when we're running under Codex ourselves.
    if let Ok(p) = std::env::var("CODEX_CLI_PATH") {
        let path = PathBuf::from(p);
        if executable(&path) {
            return Some(path);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let candidate = directory.join("codex");
            if executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    let bundled = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
    executable(&bundled).then_some(bundled)
}

fn executable(path: &std::path::Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Pushes a message into a Codex thread's queue.
fn queue_message(cli: &PathBuf, thread: &str, env: &Envelope) -> Result<()> {
    let mut command = Command::new(cli);
    command
        .arg("queue")
        .arg(format!("--thread={thread}"))
        .arg(format!(
            "--message={}",
            crate::adapters::format_for_delivery(env)
        ));
    let out = crate::process::run(command, std::time::Duration::from_secs(8))
        .context("codex queue did not confirm acceptance; no automatic retry or fallback")?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "codex queue failed; delivery is unconfirmed and was not retried: {:?}",
            err.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn special_files_cannot_block_discovery() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        for path in [
            temp.path().join("state_5.sqlite"),
            sessions.join("rollout-pipe.jsonl"),
        ] {
            let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: path is a live NUL-terminated string inside this test's tempdir.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let adapter = Codex {
            codex_home: temp.path().to_owned(),
        };
        assert!(adapter.discover_via_sqlite(0).is_err());
        assert!(adapter.discover_via_rollouts(0).unwrap().is_empty());
    }
    #[test]
    fn real_rollout_files_keep_thread_ids_distinct_from_session_roots() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions");
        fs::create_dir(&dir).unwrap();
        for (file, payload) in [
            (
                "rollout-child.jsonl",
                serde_json::json!({"id":"child", "session_id":"root", "cwd":"/example"}),
            ),
            (
                "rollout-old.jsonl",
                serde_json::json!({"id":"older", "cwd":"/example"}),
            ),
        ] {
            fs::write(
                dir.join(file),
                serde_json::json!({"type":"session_meta", "payload":payload}).to_string(),
            )
            .unwrap();
        }
        let adapter = Codex {
            codex_home: temp.path().to_owned(),
        };
        let agents = adapter.discover_via_rollouts(0).unwrap();
        assert_eq!(
            agents.iter().map(|a| a.addr.as_str()).collect::<Vec<_>>(),
            vec!["codex:child", "codex:older"]
        );
    }
    #[test]
    fn exact_resolution_can_find_quiet_threads_in_a_real_database() {
        let temp = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(temp.path().join("state_5.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE threads(id TEXT,cwd TEXT,agent_nickname TEXT,updated_at_ms INTEGER,updated_at INTEGER,archived INTEGER);
            INSERT INTO threads VALUES('old','/example','',1,0,0);
            INSERT INTO threads VALUES('archived','/example','',1,0,1);").unwrap();
        let adapter = Codex {
            codex_home: temp.path().to_owned(),
        };
        assert!(adapter.discover_via_sqlite(2).unwrap().is_empty());
        let registry = crate::registry::Registry::new(vec![Box::new(adapter)]);
        assert_eq!(
            registry.resolve(&[], "codex:old").unwrap().addr,
            "codex:old"
        );
        assert!(registry.resolve(&[], "codex:archived").is_err());
    }
}
