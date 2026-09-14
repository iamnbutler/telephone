//! Claude Code adapter.
//!
//! Claude Code is the best case: it already has a real peer-messaging layer,
//! so telephone speaks it natively rather than falling back to an inbox.
//!
//! The mechanism, which is undocumented and was determined by inspection:
//!
//! - **Discovery** is a directory of world-readable JSON, one file per live
//!   session, at `~/.claude/sessions/<pid>.json`.
//! - **Auth** is a 16-byte hex token in a sibling `0600` file,
//!   `~/.claude/sessions/<pid>.<sha256>.key`. Discovery is public but
//!   reachability is gated: you can see a session you may not message.
//! - **Transport** is a Unix socket at `/tmp/cc-socks/<pid>.sock`, carrying
//!   newline-delimited JSON. The first frame authenticates, subsequent frames
//!   are messages.
//!
//! Because this is undocumented it can break on any Claude Code release. The
//! adapter degrades to the inbox transport rather than failing hard.

use crate::envelope::Envelope;
use crate::inbox;
use crate::registry::{Adapter, Agent, Delivered, Status, Transport};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const RUNTIME: &str = "claude";

/// A `~/.claude/sessions/<pid>.json` record. Extra fields are ignored so a
/// Claude Code upgrade that adds fields doesn't break discovery.
#[derive(Debug, Deserialize)]
struct SessionRecord {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: Option<String>,
    name: Option<String>,
    status: Option<String>,
    kind: Option<String>,
    #[serde(rename = "messagingSocketPath")]
    messaging_socket_path: Option<String>,
    #[serde(rename = "statusUpdatedAt")]
    status_updated_at: Option<u64>,
    #[serde(rename = "startedAt")]
    started_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct KeyFile {
    #[serde(rename = "peerToken")]
    peer_token: String,
}

pub struct ClaudeCode {
    sessions_dir: PathBuf,
}

impl ClaudeCode {
    pub fn new() -> Result<Self> {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(ClaudeCode {
            sessions_dir: home.join(".claude").join("sessions"),
        })
    }

    /// Reads the peer token for `pid`. The filename embeds a hash, so we glob
    /// by prefix rather than reconstructing it.
    fn token_for(&self, pid: u32) -> Option<String> {
        let entries = fs::read_dir(&self.sessions_dir).ok()?;
        let prefix = format!("{pid}.");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.starts_with(&prefix) && name.ends_with(".key") {
                let raw = fs::read_to_string(entry.path()).ok()?;
                let key: KeyFile = serde_json::from_str(&raw).ok()?;
                return Some(key.peer_token);
            }
        }
        None
    }

    /// The address of *this* process's session, if we're running inside one.
    ///
    /// Claude Code puts the socket and token straight into the environment of
    /// everything it spawns, which is what lets a subprocess send as itself.
    pub fn self_address() -> Option<String> {
        std::env::var("CLAUDE_PID").ok().map(|p| format!("{RUNTIME}:{p}"))
    }

    pub fn self_socket() -> Option<String> {
        std::env::var("CLAUDE_CODE_MESSAGING_SOCKET").ok()
    }
}

fn status_from(s: Option<&str>) -> Status {
    match s {
        Some("idle") => Status::Idle,
        Some("busy") | Some("shell") | Some("waiting") => Status::Busy,
        _ => Status::Unknown,
    }
}

impl Adapter for ClaudeCode {
    fn runtime(&self) -> &'static str {
        RUNTIME
    }

    fn discover(&self) -> Result<Vec<Agent>> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let mut agents = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else { continue };
            let Ok(rec) = serde_json::from_str::<SessionRecord>(&raw) else { continue };

            // A registry entry outlives the process it describes. Checking
            // liveness here keeps dead sessions out of `list`.
            if !crate::proc::is_alive(rec.pid) {
                continue;
            }

            let socket = rec.messaging_socket_path.as_ref().map(PathBuf::from);
            let mut transports = Vec::new();
            if let Some(sock) = socket {
                if sock.exists() {
                    transports.push(Transport::ClaudeUds {
                        socket: sock,
                        session_id: rec.session_id.clone(),
                        token: self.token_for(rec.pid),
                    });
                }
            }
            transports.push(Transport::Inbox);

            agents.push(Agent {
                addr: format!("{RUNTIME}:{}", rec.pid),
                runtime: RUNTIME,
                name: rec.name.unwrap_or_else(|| format!("claude-{}", rec.pid)),
                cwd: rec.cwd.map(PathBuf::from),
                status: status_from(rec.status.as_deref()),
                last_seen: rec.status_updated_at.or(rec.started_at).unwrap_or(0),
                transports,
            });

            let _ = rec.kind;
        }
        Ok(agents)
    }

    fn deliver(&self, agent: &Agent, env: &Envelope) -> Result<Delivered> {
        for transport in &agent.transports {
            if let Transport::ClaudeUds { socket, session_id, token } = transport {
                let Some(token) = token else { continue };
                match send_uds(socket, session_id, token, env) {
                    Ok(()) => return Ok(Delivered::Native { via: "uds" }),
                    // Fall through to the inbox rather than failing: a session
                    // that just exited shouldn't lose the message.
                    Err(_) => continue,
                }
            }
        }
        let path = inbox::deposit(&agent.addr, env)?;
        Ok(Delivered::Queued {
            path,
            note: "native socket unavailable; the session will see this when it \
                   drains its telephone inbox"
                .into(),
        })
    }
}

/// Speaks Claude Code's peer protocol: auth frame, then a user message.
fn send_uds(socket: &PathBuf, session_id: &str, token: &str, env: &Envelope) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let auth = serde_json::json!({ "type": "auth", "token": token });
    writeln!(stream, "{auth}")?;

    // `from` is self-asserted; the receiver independently verifies the pid of
    // whoever actually opened the socket. We send our own session's socket when
    // we have one so replies can route back.
    let from = ClaudeCode::self_socket()
        .map(|s| format!("uds:{s}"))
        .unwrap_or_else(|| format!("telephone:{}", env.from));

    let msg = serde_json::json!({
        "type": "user",
        "uuid": crate::envelope::new_id(),
        "msg_id": env.id,
        "session_id": session_id,
        "from": from,
        "message": { "content": crate::adapters::format_for_delivery(env) },
    });
    writeln!(stream, "{msg}")?;
    stream.flush()?;
    Ok(())
}
