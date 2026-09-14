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
//!
//! ## Why desktop-owned threads stay inferred
//!
//! Threads created in the ChatGPT desktop app are owned by an app-server the
//! app spawns as a child and talks to over stdio pipes, so there is no socket
//! for us to ask. The app's own `~/.codex/ipc/ipc.sock` is a different thing
//! entirely, and it was investigated and ruled out:
//!
//! - It is not newline-delimited JSON. Frames are a little-endian `u32` byte
//!   count followed by JSON, and the reader destroys any connection whose
//!   length is 0 or over 256MiB. A JSONL request opening with `{"id` is read
//!   as a length of 1,684,611,707, which is why a naive handshake gets an
//!   instant EOF rather than an error.
//! - More decisively, the router exposes no bridge to app-server thread
//!   status. Its `thread-owner-discovery` answers which *client* owns a
//!   conversation, not whether a thread is idle or active, so even a correct
//!   handshake would not yield the signal this module needs.
//! - Connecting is not free of side effects: registration broadcasts a
//!   client-status change to every other connected client.
//!
//! It is also a private, undocumented interface with no stability contract --
//! the published app-server documentation describes the WebSocket control
//! socket, not this router. So desktop-owned threads correctly remain
//! `Liveness::Inferred`.

use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
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

/// Kills the app-server on every exit path.
///
/// The server runs until its stdin closes, so an early return that merely
/// drops the handle leaves a process behind. A guard is used rather than a
/// tidy-up at the bottom of the function because several steps can fail.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn query(cli: &std::path::Path) -> Option<HashMap<String, ThreadStatus>> {
    let child = Command::new(cli)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut guard = ChildGuard(child);

    {
        let stdin = guard.0.stdin.as_mut()?;
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

    let stdout = guard.0.stdout.take()?;

    // `read_line` blocks with no timeout of its own, so reading on this thread
    // would let a server that never emits a newline hang every `telephone`
    // command indefinitely. Read on a worker instead and bound the wait here.
    //
    // The worker is deliberately detached: when this function returns, the
    // guard kills the child, the pipe closes, the pending read returns 0 and
    // the worker exits on its own.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // A closed receiver means the caller already gave up.
            if tx.send(line.clone()).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut out = HashMap::new();
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            crate::debug(|| "codex: app-server query timed out".to_string());
            break;
        };
        let Ok(line) = rx.recv_timeout(remaining) else { break };

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
