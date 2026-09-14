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
//! adapter falls back before sending, but never retries a possibly partial send.

use crate::envelope::Envelope;
use crate::inbox;
use crate::registry::{Adapter, Agent, Delivered, Liveness, Status, Transport};
use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::os::unix::{
    ffi::OsStrExt,
    fs::{FileTypeExt, MetadataExt},
};
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

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
    #[serde(rename = "messagingSocketPath")]
    messaging_socket_path: Option<String>,
    #[serde(rename = "statusUpdatedAt")]
    status_updated_at: Option<u64>,
    #[serde(rename = "startedAt")]
    started_at: Option<u64>,
}

#[derive(Deserialize)]
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

    /// The key hash binds the token to this exact socket, not merely the PID.
    fn token_for(&self, pid: u32, socket: &std::path::Path) -> Result<String> {
        if !socket.is_absolute() {
            anyhow::bail!("Claude socket path is not absolute");
        }
        let hash = Sha256::digest(socket.as_os_str().as_bytes());
        let path = self.sessions_dir.join(format!("{pid}.{hash:x}.key"));
        let raw = crate::private_fs::read_secret(&path, 4096)?;
        let key: KeyFile = serde_json::from_str(&raw).context("invalid Claude peer key file")?;
        if key.peer_token.len() != 32 || !key.peer_token.bytes().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("invalid Claude peer token format");
        }
        Ok(key.peer_token)
    }

    /// MCP children do not always receive CLAUDE_PID. Check the direct
    /// parent's session record before trusting an inherited Codex thread id.
    pub fn parent_address() -> Result<Option<String>> {
        let pid = crate::proc::parent_pid();
        let adapter = Self::new()?;
        let path = adapter.sessions_dir.join(format!("{pid}.json"));
        if !path
            .try_exists()
            .context("checking parent session record")?
        {
            return Ok(None);
        }
        let raw = crate::private_fs::read_owned(&path, 64 * 1024)?;
        let record: SessionRecord =
            serde_json::from_str(&raw).context("invalid parent session record")?;
        let started = crate::proc::start_times(&[pid]);
        Ok((record.pid == pid
            && crate::proc::start_time_verified(record.started_at, started.get(&pid)))
        .then(|| format!("{RUNTIME}:{pid}")))
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
        if !self
            .sessions_dir
            .try_exists()
            .context("checking Claude registry")?
        {
            return Ok(Vec::new());
        }
        // Read every record first, then resolve liveness for the whole set in
        // one `ps` call rather than spawning a process per session.
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir).context("reading Claude registry")? {
            let path = entry.context("reading Claude registry entry")?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = match crate::private_fs::read_owned(&path, 64 * 1024) {
                Ok(raw) => raw,
                Err(e) => {
                    crate::warn(format!("skipping Claude record {path:?}: {e:#}"));
                    continue;
                }
            };
            let rec = match serde_json::from_str::<SessionRecord>(&raw) {
                Ok(rec) => rec,
                Err(e) => {
                    crate::warn(format!("skipping malformed Claude record {path:?}: {e}"));
                    continue;
                }
            };
            if crate::proc::is_alive(rec.pid) {
                records.push(rec);
            }
        }

        let pids: Vec<u32> = records.iter().map(|r| r.pid).collect();
        let started = crate::proc::start_times(&pids);

        let mut agents = Vec::new();
        for rec in records {
            // A registry entry outlives the process it describes, and pids get
            // recycled. Without this check a message meant for a session that
            // exited hours ago would be delivered to whatever now holds its pid.
            if !crate::proc::start_time_matches(rec.started_at, started.get(&rec.pid)) {
                crate::debug(|| {
                    format!(
                        "claude: skipping pid {} -- session recorded start {:?}, \
                         but the process holding that pid started {:?}",
                        rec.pid,
                        rec.started_at,
                        started.get(&rec.pid)
                    )
                });
                continue;
            }

            let socket = rec.messaging_socket_path.as_ref().map(PathBuf::from);
            let mut transports = Vec::new();
            if let Some(sock) = socket {
                if sock.try_exists().context("checking Claude socket path")? {
                    let token = match self.token_for(rec.pid, &sock) {
                        Ok(token) => Some(token),
                        Err(e) => {
                            crate::debug(|| format!("Claude native auth unavailable: {e:#}"));
                            None
                        }
                    };
                    if let Some(token) = token {
                        transports.push(Transport::ClaudeUds {
                            socket: sock,
                            session_id: rec.session_id.clone(),
                            token: Some(token),
                        });
                    }
                }
            }
            transports.push(Transport::Inbox);

            agents.push(Agent {
                addr: format!("{RUNTIME}:{}", rec.pid),
                runtime: RUNTIME,
                name: rec.name.unwrap_or_else(|| format!("claude-{}", rec.pid)),
                cwd: rec.cwd.map(PathBuf::from),
                status: status_from(rec.status.as_deref()),
                // Missing evidence keeps the record visible, never verified.
                liveness: if crate::proc::positively_alive(rec.pid)
                    && crate::proc::start_time_verified(rec.started_at, started.get(&rec.pid))
                {
                    Liveness::Verified
                } else {
                    Liveness::Inferred
                },
                last_seen: rec.status_updated_at.or(rec.started_at).unwrap_or(0),
                transports,
            });
        }
        Ok(agents)
    }

    fn deliver(&self, agent: &Agent, env: &Envelope) -> Result<Delivered> {
        env.validate()?;
        if env.to.as_str() != agent.addr {
            anyhow::bail!("Claude target does not match envelope recipient");
        }
        for transport in &agent.transports {
            if let Transport::ClaudeUds {
                socket,
                session_id,
                token,
            } = transport
            {
                let Some(token) = token else { continue };
                let stream = match connect_socket(socket) {
                    Ok(stream) => stream,
                    Err(e) => {
                        crate::debug(|| format!("Claude socket unavailable before send: {e:#}"));
                        continue;
                    }
                };
                // From this point, partial delivery is possible. Never fall back
                // automatically: that could perform the peer's action twice.
                send_uds(stream, session_id, token, env)
                    .context("Claude delivery is uncertain; no fallback or retry was attempted")?;
                return Ok(Delivered::Unconfirmed { via: "uds" });
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
fn connect_socket(path: &std::path::Path) -> Result<UnixStream> {
    let metadata = fs::symlink_metadata(path).context("inspecting Claude socket")?;
    // SAFETY: geteuid has no arguments or memory preconditions.
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("Claude endpoint must be a socket owned by this user");
    }
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket
        .connect_timeout(&socket2::SockAddr::unix(path)?, Duration::from_secs(2))
        .context("connecting to Claude socket")?;
    let stream = UnixStream::from(std::os::fd::OwnedFd::from(socket));
    verify_peer_user(&stream)
        .context("checking connected Claude socket owner before disclosing token")?;
    Ok(stream)
}

fn verify_peer_user(stream: &UnixStream) -> Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    let uid = {
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: stream owns a live fd; both output pointers are valid.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error()).context("reading socket peer credentials");
        }
        uid
    };
    #[cfg(target_os = "linux")]
    let uid = {
        // SAFETY: zeroed ucred is valid output storage for getsockopt.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: stream owns a live fd; cred and len are correctly sized outputs.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::from_mut(&mut cred).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error()).context("reading socket peer credentials");
        }
        if len as usize != std::mem::size_of::<libc::ucred>() {
            anyhow::bail!("invalid peer credential size");
        }
        cred.uid
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    compile_error!("native socket peer verification requires macOS or Linux");
    // SAFETY: geteuid has no arguments or caller-owned memory.
    if uid != unsafe { libc::geteuid() } {
        anyhow::bail!("socket peer belongs to a different user");
    }
    Ok(())
}

fn send_uds(mut stream: UnixStream, session_id: &str, token: &str, env: &Envelope) -> Result<()> {
    let auth = serde_json::json!({ "type": "auth", "token": token });

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
    let payload = format!("{auth}\n{msg}\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = payload.as_bytes();
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Claude write deadline exceeded")?;
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(bytes) {
            Ok(0) => anyhow::bail!("Claude socket closed during write"),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("writing Claude message"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_records_accept_present_or_absent_proc_start() {
        for proc_start in [None, Some("Mon Sep 14 12:00:00 2026")] {
            let mut value = serde_json::json!({
                "pid": 12345,
                "sessionId": "example-session",
                "startedAt": 1_789_395_543_000u64,
            });
            if let Some(start) = proc_start {
                value["procStart"] = serde_json::json!(start);
            }

            let record: SessionRecord = serde_json::from_value(value).unwrap();
            assert_eq!(record.pid, 12345);
            assert_eq!(record.session_id, "example-session");
            assert_eq!(record.started_at, Some(1_789_395_543_000));
        }
    }

    #[test]
    fn a_live_pid_without_start_evidence_is_only_inferred() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("session.json"),
            serde_json::json!({"pid":std::process::id(),"sessionId":"fixture"}).to_string(),
        )
        .unwrap();
        let adapter = ClaudeCode {
            sessions_dir: temp.path().to_owned(),
        };
        let agents = adapter.discover().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].liveness, Liveness::Inferred);
    }

    #[test]
    fn real_socket_delivery_reports_unconfirmed_and_frames_are_valid_json() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("peer.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let receiver = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut frames = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                frames.push(serde_json::from_str::<serde_json::Value>(&line).unwrap());
            }
            frames
        });
        let mut env = Envelope::new(
            "codex:sender",
            "claude:receiver",
            crate::envelope::Kind::Request,
            "fixture only".into(),
        )
        .unwrap();
        env.add_hop(env.from.clone()).unwrap();
        let agent = Agent {
            addr: env.to.to_string(),
            name: "fixture".into(),
            runtime: "claude",
            cwd: None,
            status: Status::Unknown,
            liveness: Liveness::Inferred,
            last_seen: 0,
            transports: vec![Transport::ClaudeUds {
                socket: path,
                session_id: "fixture-session".into(),
                token: Some("0".repeat(32)),
            }],
        };
        let adapter = ClaudeCode {
            sessions_dir: temp.path().to_owned(),
        };
        assert!(matches!(
            adapter.deliver(&agent, &env).unwrap(),
            Delivered::Unconfirmed { .. }
        ));
        let frames = receiver.join().unwrap();
        assert_eq!(frames[0]["type"], "auth");
        assert_eq!(frames[1]["msg_id"], env.id.to_string());
        assert!(frames[1]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("not authenticated"));
    }

    #[test]
    fn key_selection_is_bound_to_socket_hash_and_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let adapter = ClaudeCode {
            sessions_dir: temp.path().to_owned(),
        };
        let socket = temp.path().join("fixture.sock");
        let hash = Sha256::digest(socket.as_os_str().as_bytes());
        let key = temp.path().join(format!("123.{hash:x}.key"));
        fs::write(
            &key,
            serde_json::json!({"peerToken":"a".repeat(32)}).to_string(),
        )
        .unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(adapter.token_for(123, &socket).is_err());
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(adapter.token_for(123, &socket).unwrap(), "a".repeat(32));
        assert!(adapter
            .token_for(123, &temp.path().join("different.sock"))
            .is_err());
    }
}
