//! Asking Codex what it actually thinks is running.
//!
//! Codex's on-disk records answer "when was this thread last written to",
//! which is not the same question as "is this thread running now". A thread
//! that exited seconds after its last write looks identical to one mid-turn.
//!
//! The app-server protocol does answer the real question: `thread/list`
//! returns a `status` documented as the thread's current runtime status, one
//! of `notLoaded`, `idle`, `active` or `systemError`.
//!
//! The catch, and the reason this is best-effort rather than authoritative:
//! **runtime status is only known to the daemon that owns the thread.** A
//! freshly spawned app-server reports `notLoaded` for every thread on the
//! machine, because it hasn't loaded any of them. So `notLoaded` means "this
//! daemon can't tell you", not "the thread is dead", and callers must fall
//! back to recency rather than treating it as an answer.
//!
//! The wire format is newline-delimited JSON with no `jsonrpc` field --
//! `{"id":N,"method":...,"params":...}` -- which is close enough to JSON-RPC
//! to be mistaken for it and different enough to fail silently if you assume.

use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawning an app-server costs a process and some config loading, so give it
/// a hard ceiling. Discovery must stay fast enough to run on every command.
const QUERY_TIMEOUT: Duration = Duration::from_secs(8);

/// How many threads to ask about. Discovery only cares about recent ones.
const PAGE_SIZE: u32 = 50;

/// The runtime status the owning daemon reports for a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    /// Running and waiting for input.
    Idle,
    /// Running a turn right now.
    Active,
    /// The daemon we asked has no runtime state for this thread. Says nothing
    /// about whether some *other* daemon is running it.
    NotLoaded,
    /// Loaded, but the daemon reported it as broken.
    SystemError,
}

impl ThreadStatus {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(ThreadStatus::Idle),
            "active" => Some(ThreadStatus::Active),
            "notLoaded" => Some(ThreadStatus::NotLoaded),
            "systemError" => Some(ThreadStatus::SystemError),
            _ => None,
        }
    }

    /// Whether this status actually tells us the thread is running.
    pub fn is_conclusive(&self) -> bool {
        matches!(self, ThreadStatus::Idle | ThreadStatus::Active)
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    id: Option<u32>,
    result: Option<ThreadListResult>,
}

#[derive(Debug, Deserialize)]
struct ThreadListResult {
    data: Vec<ThreadEntry>,
}

#[derive(Debug, Deserialize)]
struct ThreadEntry {
    id: String,
    status: StatusField,
}

#[derive(Debug, Deserialize)]
struct StatusField {
    #[serde(rename = "type")]
    kind: String,
}

/// Asks an app-server for the runtime status of every thread it knows about.
///
/// Returns an empty map on any failure. This is an enrichment pass: if it
/// doesn't work, discovery still functions on recency alone.
pub fn thread_statuses(cli: &std::path::Path) -> HashMap<String, ThreadStatus> {
    query(cli).unwrap_or_default()
}

fn query(cli: &std::path::Path) -> Option<HashMap<String, ThreadStatus>> {
    let mut child = Command::new(cli)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    {
        let stdin = child.stdin.as_mut()?;
        // Note the absent `jsonrpc` field: the schema defines requests as
        // exactly {id, method, params}, and the server ignores anything else.
        let init = serde_json::json!({
            "id": 1,
            "method": "initialize",
            "params": { "clientInfo": { "name": "telephone", "version": env!("CARGO_PKG_VERSION") } }
        });
        let list = serde_json::json!({
            "id": 2,
            "method": "thread/list",
            "params": { "limit": PAGE_SIZE }
        });
        writeln!(stdin, "{init}").ok()?;
        writeln!(stdin, "{list}").ok()?;
        stdin.flush().ok()?;
    }

    let stdout = child.stdout.take()?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut reader = BufReader::new(stdout);
    let mut out = HashMap::new();
    let mut line = String::new();

    loop {
        if Instant::now() > deadline {
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let Ok(response) = serde_json::from_str::<Response>(line.trim()) else { continue };
        if response.id != Some(2) {
            continue;
        }
        if let Some(result) = response.result {
            for entry in result.data {
                if let Some(status) = ThreadStatus::parse(&entry.status.kind) {
                    out.insert(entry.id, status);
                }
            }
        }
        break;
    }

    // The app-server runs until its stdin closes; don't leave it behind.
    let _ = child.kill();
    let _ = child.wait();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loaded_statuses_tell_us_anything() {
        assert!(ThreadStatus::Idle.is_conclusive());
        assert!(ThreadStatus::Active.is_conclusive());
        // The important one: a daemon that hasn't loaded a thread knows
        // nothing about it, which is not the same as the thread being dead.
        assert!(!ThreadStatus::NotLoaded.is_conclusive());
        assert!(!ThreadStatus::SystemError.is_conclusive());
    }

    #[test]
    fn status_names_match_the_protocol_schema() {
        assert_eq!(ThreadStatus::parse("notLoaded"), Some(ThreadStatus::NotLoaded));
        assert_eq!(ThreadStatus::parse("active"), Some(ThreadStatus::Active));
        assert_eq!(ThreadStatus::parse("idle"), Some(ThreadStatus::Idle));
        assert_eq!(ThreadStatus::parse("unrecognised"), None);
    }
}
