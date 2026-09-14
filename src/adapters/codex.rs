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

use crate::envelope::Envelope;
use crate::inbox;
use crate::registry::{Adapter, Agent, Delivered, Status, Transport};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
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

/// Widens the liveness window, in minutes. `telephone list --all` sets this so
/// you can see threads that have gone quiet without losing the default's
/// bias toward "currently running".
const WINDOW_ENV: &str = "TELEPHONE_WINDOW_MINS";

fn live_window_millis() -> u64 {
    match std::env::var(WINDOW_ENV).ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(mins) => mins.saturating_mul(60_000),
        None => LIVE_WINDOW_MILLIS,
    }
}

pub struct Codex {
    codex_home: PathBuf,
}

impl Codex {
    pub fn new() -> Result<Self> {
        // Respect CODEX_HOME so this works for non-default installs.
        let codex_home = match std::env::var("CODEX_HOME") {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => dirs::home_dir().context("no home directory")?.join(".codex"),
        };
        Ok(Codex { codex_home })
    }

    /// Codex versions its state database (`state_5.sqlite`, and so on), so
    /// pick the highest version present rather than pinning to one.
    fn state_db(&self) -> Option<PathBuf> {
        let mut best: Option<(u32, PathBuf)> = None;
        for entry in fs::read_dir(&self.codex_home).ok()?.flatten() {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let Some(rest) = name.strip_prefix("state_") else { continue };
            let Some(version) = rest.strip_suffix(".sqlite") else { continue };
            let Ok(version) = version.parse::<u32>() else { continue };
            if best.as_ref().is_none_or(|(b, _)| version > *b) {
                best = Some((version, path));
            }
        }
        best.map(|(_, p)| p)
    }

    fn discover_via_sqlite(&self, cutoff: u64) -> Result<Vec<Agent>> {
        let db = self.state_db().context("no state database")?;

        // Codex holds this open in WAL mode. Read-only is both correct and
        // the only safe thing to do to another process's live database.
        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", db.display()))?;

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
        for row in rows {
            let (id, cwd, label, updated) = row?;
            agents.push(build_agent(&id, cwd, &label, updated.max(0) as u64));
        }
        Ok(agents)
    }

    /// Fallback for when the state database can't be read: every live thread
    /// appends to a rollout log whose first line is a `session_meta` record.
    fn discover_via_rollouts(&self, cutoff: u64) -> Result<Vec<Agent>> {
        let sessions = self.codex_home.join("sessions");
        if !sessions.exists() {
            return Ok(Vec::new());
        }

        let mut agents = Vec::new();
        let mut stack = vec![sessions];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let is_rollout = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
                if !is_rollout {
                    continue;
                }
                let modified = mtime_millis(&path);
                if modified < cutoff {
                    continue;
                }
                // The meta record is the first line; don't read a whole
                // transcript just to learn a session id.
                let Ok(file) = fs::File::open(&path) else { continue };
                let mut first = String::new();
                {
                    use std::io::BufRead;
                    let mut reader = std::io::BufReader::new(file);
                    if reader.read_line(&mut first).is_err() {
                        continue;
                    }
                }
                let Ok(line) = serde_json::from_str::<RolloutLine>(&first) else { continue };
                if line.kind != "session_meta" {
                    continue;
                }
                let Ok(meta) = serde_json::from_value::<SessionMeta>(line.payload) else { continue };
                agents.push(build_agent(&meta.session_id, meta.cwd, "", modified));
            }
        }
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
    session_id: String,
    cwd: Option<String>,
}

fn build_agent(id: &str, cwd: Option<String>, label: &str, last_seen: u64) -> Agent {
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
    Agent {
        addr: format!("{RUNTIME}:{id}"),
        runtime: RUNTIME,
        name,
        cwd: cwd.map(PathBuf::from),
        status: Status::Unknown,
        last_seen,
        transports: if codex_cli().is_some() {
            vec![Transport::CodexQueue, Transport::Inbox]
        } else {
            vec![Transport::Inbox]
        },
    }
}

/// Nicknames are free text. Names are for typing at a shell, so keep them
/// short and shell-safe.
fn slugify(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    cleaned
        .split('-')
        .filter(|p| !p.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
}

fn mtime_millis(path: &PathBuf) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        if !self.codex_home.exists() {
            return Ok(Vec::new());
        }
        let cutoff = now_millis().saturating_sub(live_window_millis());

        // The state database is authoritative; rollouts are the safety net.
        match self.discover_via_sqlite(cutoff) {
            Ok(agents) if !agents.is_empty() => Ok(agents),
            Ok(_) => self.discover_via_rollouts(cutoff),
            Err(e) => {
                crate::debug(|| format!("codex: state db unreadable ({e}); using rollouts"));
                self.discover_via_rollouts(cutoff)
            }
        }
    }

    fn deliver(&self, agent: &Agent, env: &Envelope) -> Result<Delivered> {
        let thread = agent
            .addr
            .split_once(':')
            .map(|(_, id)| id)
            .unwrap_or(&agent.addr);

        if let Some(cli) = codex_cli() {
            match queue_message(&cli, thread, env) {
                Ok(()) => return Ok(Delivered::Native { via: "codex queue" }),
                Err(e) => crate::debug(|| format!("codex: queue failed ({e}); using inbox")),
            }
        }

        let path = inbox::deposit(&agent.addr, env)?;
        Ok(Delivered::Queued {
            path,
            note: "the codex CLI wasn't usable, so this is waiting in the inbox; \
                   Codex will see it when it calls the telephone MCP server's \
                   check_inbox tool"
                .into(),
        })
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
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(out) = Command::new("sh").arg("-c").arg("command -v codex").output() {
        let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !found.is_empty() {
            return Some(PathBuf::from(found));
        }
    }
    let bundled = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
    bundled.is_file().then_some(bundled)
}

/// Pushes a message into a Codex thread's queue.
fn queue_message(cli: &PathBuf, thread: &str, env: &Envelope) -> Result<()> {
    let out = Command::new(cli)
        .arg("queue")
        .arg("--thread")
        .arg(thread)
        .arg("--message")
        .arg(crate::adapters::format_for_delivery(env))
        .output()
        .context("running `codex queue`")?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("codex queue failed: {}", err.trim());
    }
    Ok(())
}
