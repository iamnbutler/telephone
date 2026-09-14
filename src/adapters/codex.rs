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

use crate::discovery::{self, Budget, Code, Discovery, MAX_AGENTS};
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

    /// State-version selection is itself bounded; never silently select an
    /// older database after a truncated directory scan.
    fn state_db(&self, budget: &mut Budget, report: &mut Discovery) -> Result<Option<PathBuf>> {
        let paths = discovery::files(
            &self.codex_home,
            false,
            |path| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| name.starts_with("state_") && name.ends_with(".sqlite"))
            },
            RUNTIME,
            budget,
            report,
        );
        if !report.complete {
            anyhow::bail!(
                "cannot determine the latest Codex state database from an incomplete scan"
            );
        }
        Ok(paths
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                let version = name
                    .strip_prefix("state_")?
                    .strip_suffix(".sqlite")?
                    .parse::<u32>()
                    .ok()?;
                Some((version, path))
            })
            .max_by_key(|(v, _)| *v)
            .map(|(_, p)| p))
    }

    fn discover_via_sqlite(
        &self,
        cutoff: u64,
        exact: Option<&str>,
        budget: &mut Budget,
        report: &mut Discovery,
    ) -> Result<bool> {
        let Some(db) = self.state_db(budget, report)? else {
            return Ok(false);
        };
        let metadata = fs::symlink_metadata(&db).context("inspecting Codex database")?;
        // SAFETY: geteuid has no arguments or caller-owned memory.
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!("Codex database must be a regular file owned by this user");
        }
        let db = db.canonicalize().context("resolving Codex database path")?;
        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("opening Codex state database")?;
        conn.busy_timeout(std::time::Duration::from_millis(250))
            .context("setting discovery lock timeout")?;
        conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 64 * 1024)
            .context("limiting discovery row size")?;
        let deadline = budget.deadline;
        let mut ticks = 0u32;
        conn.progress_handler(
            1000,
            Some(move || {
                ticks += 1;
                ticks >= 5000 || std::time::Instant::now() >= deadline
            }),
        )
        .context("setting discovery SQL work budget")?;
        // Exact lookup has its own indexed predicate, not a search through the
        // first page of recent threads. All SQL values remain bound parameters.
        let (filter, value) = match exact {
            Some(id) => ("id = ?1", rusqlite::types::Value::Text(id.to_owned())),
            None => (
                "COALESCE(updated_at_ms, updated_at * 1000) >= ?1",
                rusqlite::types::Value::Integer(cutoff.min(i64::MAX as u64) as i64),
            ),
        };
        let sql = format!("SELECT id,cwd,COALESCE(NULLIF(agent_nickname,''),''),COALESCE(updated_at_ms,updated_at*1000)
            FROM threads WHERE archived=0 AND {filter} ORDER BY COALESCE(updated_at_ms,updated_at*1000) DESC LIMIT ?2");
        let mut stmt = conn
            .prepare(&sql)
            .context("preparing bounded Codex discovery")?;
        let mut rows = stmt
            .query(rusqlite::params![value, (MAX_AGENTS + 1) as i64])
            .context("querying Codex discovery")?;
        let native = codex_cli().is_some();
        let mut count = 0;
        loop {
            if !budget.check(RUNTIME, report) {
                break;
            }
            let row = match rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(e) => {
                    let code = match &e {
                        rusqlite::Error::SqliteFailure(error, _)
                            if error.code == rusqlite::ErrorCode::OperationInterrupted =>
                        {
                            Code::LimitReached
                        }
                        _ => Code::Unreadable,
                    };
                    report.warn(
                        RUNTIME,
                        code,
                        Some(&db),
                        format!("state query stopped (including SQL work/time limits): {e}"),
                    );
                    break;
                }
            };
            count += 1;
            if count > MAX_AGENTS {
                report.warn(
                    RUNTIME,
                    Code::LimitReached,
                    Some(&db),
                    "state result limit reached (256); use an exact address",
                );
                break;
            }
            let decoded = (|| -> Result<Agent> {
                let id: String = row.get(0).context("reading thread id")?;
                let cwd: Option<String> = row.get(1).context("reading thread directory")?;
                let label: String = row.get(2).context("reading thread nickname")?;
                let updated: i64 = row.get(3).context("reading thread timestamp")?;
                budget.bytes = budget
                    .bytes
                    .saturating_sub(id.len() + cwd.as_ref().map_or(0, String::len) + label.len());
                build_agent(&id, cwd, &label, updated.max(0) as u64, native)
            })();
            match decoded {
                Ok(agent) => {
                    if !report.push(agent) {
                        break;
                    }
                }
                Err(e) => report.warn(RUNTIME, Code::InvalidRecord, Some(&db), format!("{e:#}")),
            }
        }
        Ok(true)
    }

    fn discover_via_rollouts(
        &self,
        cutoff: u64,
        exact: Option<&str>,
        budget: &mut Budget,
        report: &mut Discovery,
    ) {
        let sessions = self.codex_home.join("sessions");
        let paths = discovery::files(
            &sessions,
            true,
            |path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            },
            RUNTIME,
            budget,
            report,
        );
        let native = codex_cli().is_some();
        for path in paths {
            if !budget.check(RUNTIME, report) {
                break;
            }
            let mut failure_code = Code::Unreadable;
            let result = (|| -> Result<Option<Agent>> {
                use std::io::{BufRead, Read};
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(&path)
                    .context("opening rollout")?;
                let metadata = file.metadata().context("inspecting open rollout")?;
                // SAFETY: geteuid has no arguments or caller-owned memory.
                if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
                    failure_code = Code::InvalidRecord;
                    anyhow::bail!("rollout must be a regular file owned by this user");
                }
                let modified = metadata
                    .modified()
                    .context("reading rollout timestamp")?
                    .duration_since(UNIX_EPOCH)
                    .context("rollout timestamp precedes epoch")?
                    .as_millis() as u64;
                if modified < cutoff {
                    return Ok(None);
                }
                let cap = budget.bytes.min(1024 * 1024);
                let mut raw = Vec::new();
                let read =
                    std::io::BufReader::new(file.take(cap as u64 + 1)).read_until(b'\n', &mut raw);
                budget.bytes = budget.bytes.saturating_sub(raw.len());
                read.context("reading rollout metadata")?;
                failure_code = Code::InvalidRecord;
                if raw.len() > cap {
                    anyhow::bail!("rollout metadata exceeds byte limit");
                }
                let line: RolloutLine =
                    serde_json::from_slice(&raw).context("invalid rollout JSON")?;
                if line.kind != "session_meta" {
                    anyhow::bail!("rollout does not start with session metadata");
                }
                let meta: SessionMeta =
                    serde_json::from_value(line.payload).context("invalid session metadata")?;
                let id = meta
                    .id
                    .or(meta.session_id)
                    .context("rollout metadata has no thread id")?;
                if exact.is_some_and(|wanted| wanted != id) {
                    return Ok(None);
                }
                build_agent(&id, meta.cwd, "", modified, native).map(Some)
            })();
            match result {
                Ok(Some(agent)) => {
                    if !report.push(agent) {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => report.warn(RUNTIME, failure_code, Some(&path), format!("{e:#}")),
            }
        }
        report.agents.sort_by(|a, b| a.addr.cmp(&b.addr));
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

    fn discover(&self) -> Result<Discovery> {
        if !self
            .codex_home
            .try_exists()
            .context("checking Codex directory")?
        {
            return Ok(Discovery::default());
        }
        let cutoff = now_millis().saturating_sub(live_window_millis()?);

        self.discover_since(cutoff, None)
    }

    fn discover_all(&self) -> Result<Discovery> {
        self.discover_since(0, None)
    }

    fn find_exact(&self, address: &str) -> Result<Discovery> {
        let address: crate::address::Address = address.parse()?;
        let id = address
            .as_str()
            .strip_prefix("codex:")
            .context("not a Codex address")?;
        self.discover_since(0, Some(id))
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
    fn discover_since(&self, cutoff: u64, exact: Option<&str>) -> Result<Discovery> {
        let mut report = Discovery::default();
        let mut budget = Budget::new();
        match self.discover_via_sqlite(cutoff, exact, &mut budget, &mut report) {
            Ok(true) => {}
            Ok(false) => self.discover_via_rollouts(cutoff, exact, &mut budget, &mut report),
            Err(e) => {
                report.warn(
                    RUNTIME,
                    Code::SourceUnavailable,
                    Some(&self.codex_home),
                    format!("state database unavailable; using rollouts: {e:#}"),
                );
                self.discover_via_rollouts(cutoff, exact, &mut budget, &mut report);
            }
        }
        Ok(report)
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
    fn state_fixture(root: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,cwd TEXT,agent_nickname TEXT,updated_at_ms INTEGER,updated_at INTEGER,archived INTEGER);").unwrap();
        conn
    }
    fn rollout(path: &std::path::Path, id: &str) {
        fs::write(
            path,
            serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":"/example"}})
                .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn capped_database_listing_does_not_hide_exact_addresses() {
        let root = tempfile::tempdir().unwrap();
        let mut conn = state_fixture(root.path());
        let tx = conn.transaction().unwrap();
        for i in 0..600 {
            tx.execute(
                "INSERT INTO threads VALUES(?1,'/example','',?2,0,0)",
                rusqlite::params![format!("thread-{i}"), i],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let adapter = Codex {
            codex_home: root.path().to_owned(),
        };
        let mut report = adapter.discover_all().unwrap();
        assert_eq!(report.agents.len(), MAX_AGENTS);
        assert!(!report.complete);
        assert!(!report.agents.iter().any(|a| a.addr == "codex:thread-0"));
        let registry = crate::registry::Registry::new(vec![Box::new(adapter)]);
        assert!(
            registry.resolve(&mut report, "thread-599").is_err(),
            "partial lists must not authorize name resolution"
        );
        assert_eq!(
            registry
                .resolve(&mut report, "codex:thread-0")
                .unwrap()
                .addr,
            "codex:thread-0"
        );
    }

    #[test]
    fn capped_rollout_listing_has_a_separate_bounded_exact_lookup() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        for i in 0..600 {
            rollout(
                &sessions.join(format!("rollout-{i}.jsonl")),
                &format!("r{i}"),
            );
        }
        let adapter = Codex {
            codex_home: root.path().to_owned(),
        };
        let report = adapter.discover_all().unwrap();
        assert_eq!(report.agents.len(), MAX_AGENTS);
        assert!(!report.complete);
        let missing = (0..600)
            .map(|i| format!("codex:r{i}"))
            .find(|addr| !report.agents.iter().any(|a| a.addr == *addr))
            .unwrap();
        let exact = adapter.find_exact(&missing).unwrap();
        assert!(exact.complete);
        assert_eq!(exact.agents.len(), 1);
        assert_eq!(exact.agents[0].addr, missing);
    }

    #[test]
    fn total_rollout_metadata_bytes_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        for i in 0..12 {
            fs::write(sessions.join(format!("rollout-{i}.jsonl")),serde_json::json!({
                "type":"session_meta", "payload":{"id":format!("r{i}"),"padding":"x".repeat(1024*1024-1024)}
            }).to_string()).unwrap();
        }
        let adapter = Codex {
            codex_home: root.path().to_owned(),
        };
        let report = adapter.discover_all().unwrap();
        assert!(!report.complete);
        assert!(report.agents.len() <= 8);
        assert!(report
            .warnings
            .iter()
            .any(|w| matches!(w.code, Code::LimitReached)));
    }

    #[test]
    fn malformed_database_rows_keep_valid_rows_and_reach_mcp_as_structured_warnings() {
        let root = tempfile::tempdir().unwrap();
        let conn = state_fixture(root.path());
        conn.execute_batch(
            "INSERT INTO threads VALUES('valid','/example','',3,0,0);
            INSERT INTO threads VALUES('invalid id','/example','',2,0,0);
            INSERT INTO threads VALUES('also-valid','/example','',1,0,0);",
        )
        .unwrap();
        let adapter = Codex {
            codex_home: root.path().to_owned(),
        };
        let report = adapter.discover_all().unwrap();
        assert_eq!(report.agents.len(), 2);
        let mcp = crate::mcp::discovery_json(None, &report);
        assert_eq!(mcp["complete"], false);
        assert_eq!(mcp["warnings"][0]["runtime"], "codex");
        assert_eq!(mcp["warnings"][0]["code"], "invalid_record");
        assert!(mcp["warnings"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("state_5.sqlite"));
        assert!(report
            .agents
            .iter()
            .all(|a| a.liveness == Liveness::Inferred));
    }

    #[test]
    fn expensive_database_work_is_interrupted_instead_of_hanging_discovery() {
        let root = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(root.path().join("state_5.sqlite")).unwrap();
        conn.execute_batch("CREATE VIEW threads AS WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000000)
            SELECT printf('t%d',x) AS id,'' AS cwd,'' AS agent_nickname,x AS updated_at_ms,0 AS updated_at,0 AS archived FROM n;").unwrap();
        let adapter = Codex {
            codex_home: root.path().to_owned(),
        };
        let start = std::time::Instant::now();
        let report = adapter.discover_all().unwrap();
        assert!(!report.complete);
        assert!(report
            .warnings
            .iter()
            .any(|w| matches!(w.code, Code::LimitReached)));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn concurrent_rollout_writes_and_disappearances_preserve_stable_records() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        rollout(&sessions.join("rollout-stable.jsonl"), "stable");
        fs::write(sessions.join("rollout-malformed.jsonl"), "{partial").unwrap();
        let moving = sessions.join("rollout-moving.jsonl");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let writer = std::thread::spawn(move || {
            ready.wait();
            for _ in 0..400 {
                rollout(&moving, "moving");
                fs::write(&moving, "{partial").unwrap();
                fs::remove_file(&moving).unwrap();
            }
        });
        let adapter = Codex {
            codex_home: root.path().to_owned(),
        };
        barrier.wait();
        for _ in 0..20 {
            let report = adapter.discover_all().unwrap();
            assert!(report.agents.iter().any(|a| a.addr == "codex:stable"));
            assert!(!report.complete);
            assert!(!report.warnings.is_empty());
        }
        writer.join().unwrap();
    }

    #[test]
    fn inaccessible_rollout_directory_does_not_discard_readable_siblings() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        rollout(&sessions.join("rollout-stable.jsonl"), "stable");
        let blocked = sessions.join("blocked");
        fs::create_dir(&blocked).unwrap();
        rollout(&blocked.join("rollout-hidden.jsonl"), "hidden");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let adapter = Codex {
            codex_home: root.path().to_owned(),
        };
        let result = adapter.discover_all();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        let report = result.unwrap();
        assert!(report.agents.iter().any(|a| a.addr == "codex:stable"));
        // Root can read mode-000 directories; ordinary CI users cannot.
        // SAFETY: geteuid has no arguments or caller-owned memory.
        if unsafe { libc::geteuid() } != 0 {
            assert!(!report.complete);
            assert!(report
                .warnings
                .iter()
                .any(|w| matches!(w.code, Code::Unreadable)));
        }
    }
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
        let report = adapter.discover_all().unwrap();
        assert!(report.agents.is_empty());
        assert!(!report.complete);
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
        let agents = adapter.discover_all().unwrap().agents;
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
        assert!(adapter.discover_since(2, None).unwrap().agents.is_empty());
        let registry = crate::registry::Registry::new(vec![Box::new(adapter)]);
        assert_eq!(
            registry
                .resolve(&mut Discovery::default(), "codex:old")
                .unwrap()
                .addr,
            "codex:old"
        );
        assert!(registry
            .resolve(&mut Discovery::default(), "codex:archived")
            .is_err());
    }
}
