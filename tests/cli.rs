use std::io::Write;
use std::process::{Command, Stdio};

fn isolated(home: &std::path::Path, identity: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_telephone"));
    // Child-only test home: never mutate the test runner's environment or user's journal.
    command
        .env("HOME", home)
        .env_remove("CODEX_HOME")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_PID")
        .env_remove("TELEPHONE_ADDR")
        .env_remove("TELEPHONE_NAME")
        .env_remove("TELEPHONE_STATE_DIR")
        .env_remove("CLAUDE_CODE_MESSAGING_SOCKET");
    if let Some(identity) = identity {
        command.env("TELEPHONE_ADDR", identity);
    }
    command
}

fn success(home: &std::path::Path, identity: Option<&str>, args: &[&str]) -> String {
    let out = isolated(home, identity).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

struct McpProcess {
    child: std::process::Child,
    writer: std::os::unix::net::UnixStream,
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    next_id: u64,
}
impl McpProcess {
    fn from_command(mut command: Command) -> Self {
        use std::{os::fd::OwnedFd, os::unix::net::UnixStream, time::Duration};
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let child = command
            .arg("mcp")
            .stdin(Stdio::from(OwnedFd::from(server.try_clone().unwrap())))
            .stdout(Stdio::from(OwnedFd::from(server)))
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut process = Self {
            child,
            reader: std::io::BufReader::new(client.try_clone().unwrap()),
            writer: client,
            next_id: 1,
        };
        process.request("initialize", serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"real-process-test","version":"1"}}));
        writeln!(
            process.writer,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        process
    }
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use std::io::BufRead;
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.writer,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id);
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> String {
        let result = self.request(
            "tools/call",
            serde_json::json!({"name":name,"arguments":arguments}),
        );
        assert_ne!(result["isError"], true, "{result}");
        result["content"][0]["text"].as_str().unwrap().to_owned()
    }
}
impl Drop for McpProcess {
    fn drop(&mut self) {
        // The guard also cleans up on test assertion failure; no abandoned MCP children.
        if let Err(error) = self.child.kill() {
            eprintln!("test child kill: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("test child wait: {error}");
        }
    }
}

fn native_fixture(home: &std::path::Path) -> (std::os::unix::net::UnixListener, String) {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        os::unix::{ffi::OsStrExt, fs::PermissionsExt, net::UnixListener},
    };
    let sessions = home.join(".claude/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let socket = home.join("cc.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let pid = std::process::id();
    let claude = format!("claude:{pid}");
    fs::write(
        sessions.join(format!("{pid}.json")),
        json!({"pid":pid,"sessionId":"socket-fixture","startedAt":now(),"messagingSocketPath":socket}).to_string(),
    )
    .unwrap();
    let hash = Sha256::digest(socket.as_os_str().as_bytes());
    let key = sessions.join(format!("{pid}.{hash:x}.key"));
    fs::write(&key, json!({"peerToken":"a".repeat(32)}).to_string()).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    (listener, claude)
}

fn accept_peer(listener: &std::os::unix::net::UnixListener) -> std::os::unix::net::UnixStream {
    use std::time::{Duration, Instant};
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(10));
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true).unwrap();
                return stream;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
}

#[test]
fn native_socket_then_automatic_reply_and_safe_fallback_use_real_processes() {
    use serde_json::Value;
    use std::{
        io::Read,
        time::{Duration, Instant},
    };
    let home = tempfile::tempdir().unwrap();
    let (listener, claude) = native_fixture(home.path());
    let receiving = std::thread::spawn(move || {
        let started = Instant::now();
        let mut stream = accept_peer(&listener);
        let mut bytes = Vec::new();
        while bytes.iter().filter(|b| **b == b'\n').count() < 2 {
            assert!(started.elapsed() < Duration::from_secs(10));
            let mut buffer = [0; 4096];
            match stream.read(&mut buffer) {
                Ok(0) => panic!("peer closed before both frames"),
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("read: {e}"),
            }
        }
        let frames: Vec<Value> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(frames[0]["type"], "auth");
        frames[1].clone()
        // Dropping the listener leaves the pathname in place: future connect gets refused.
    });
    let sender = seed_codex(home.path(), "sender");
    let sender = sender.as_str();
    let out = success(
        home.path(),
        Some(sender),
        &["send", &claude, "native request", "--kind", "request"],
    );
    assert!(out.contains("Written over uds"));
    assert!(out.contains("every 2 seconds"));
    let received = receiving.join().unwrap();
    let id = received["msg_id"].as_str().unwrap();
    assert!(out.contains(id));
    assert!(received["message"]["content"]
        .as_str()
        .unwrap()
        .contains(sender));
    let db = rusqlite::Connection::open(home.path().join(".telephone/messages.sqlite")).unwrap();
    let queued: i64 = db
        .query_row("SELECT count(*) FROM inbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(queued, 0, "native sends must not also be queued");
    let native_reply = success(
        home.path(),
        Some(&claude),
        &[
            "send",
            sender,
            "native recipient replies",
            "--kind",
            "reply",
            "--reply-to",
            id,
        ],
    );
    assert!(!native_reply.contains("Receiving replies:"));
    assert!(!native_reply.contains("Telephone will not wake this thread"));
    let reply: Value =
        serde_json::from_str(&success(home.path(), Some(sender), &["inbox", "--json"])).unwrap();
    assert_eq!(reply[0]["reply_to"], id);
    let out = success(
        home.path(),
        Some(sender),
        &["send", &claude, "safe fallback"],
    );
    assert!(out.contains("Queued in Telephone inbox"));
    let inbox: Value =
        serde_json::from_str(&success(home.path(), Some(&claude), &["inbox", "--json"])).unwrap();
    assert_eq!(inbox.as_array().unwrap().len(), 1);
    assert_eq!(inbox[0]["body"], "safe fallback");
}

#[test]
fn native_failure_after_connection_never_queues_a_duplicate() {
    use std::{
        io::Read,
        net::Shutdown,
        time::{Duration, Instant},
    };
    let home = tempfile::tempdir().unwrap();
    let (listener, claude) = native_fixture(home.path());
    let receiving = std::thread::spawn(move || {
        let mut stream = accept_peer(&listener);
        let started = Instant::now();
        // Read a byte to establish that sending has started, then break the connection.
        loop {
            assert!(started.elapsed() < Duration::from_secs(10));
            match stream.read(&mut [0; 1]) {
                Ok(1) => break,
                Ok(_) => panic!("peer closed before any bytes"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => panic!("read: {e}"),
            }
        }
        stream.shutdown(Shutdown::Both).unwrap();
    });
    let sender = "codex:sender";
    // JSON escaping makes the real payload larger than the socket's buffer.
    let out = isolated(home.path(), Some(sender.trim()))
        .args(["send", &claude, &"\u{1}".repeat(65536)])
        .output()
        .unwrap();
    receiving.join().unwrap();
    assert!(!out.status.success());
    // CLI errors are JSON-escaped to keep terminal control characters inert.
    let stderr = String::from_utf8(out.stderr).unwrap();
    let error: String =
        serde_json::from_str(stderr.trim().strip_prefix("error: ").unwrap()).unwrap();
    assert!(error.contains("uncertain"));
    assert!(error.contains("inspect before retrying"));
    let db = rusqlite::Connection::open(home.path().join(".telephone/messages.sqlite")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM inbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let outcome: String = db
        .query_row("SELECT outcome FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(outcome, "failed-or-uncertain");
}

#[test]
fn cli_rejects_unsafe_identity_without_a_send() {
    let out = Command::new(env!("CARGO_BIN_EXE_telephone"))
        .arg("whoami")
        .env("TELEPHONE_ADDR", "codex:bad; printf injected")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid identity"));
}

#[test]
fn packaged_stdio_protocol_initializes_and_answers_ping() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_telephone"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input,"{}",serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"ping"})
    )
    .unwrap();
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let values: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(values.len(), 2);
    assert_eq!(values[1]["result"], serde_json::json!({}));
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn seed_codex(home: &std::path::Path, thread: &str) -> String {
    let root = home.join(".codex");
    std::fs::create_dir_all(&root).unwrap();
    let conn = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS threads(id TEXT PRIMARY KEY,cwd TEXT,agent_nickname TEXT,updated_at_ms INTEGER,updated_at INTEGER,archived INTEGER);").unwrap();
    conn.execute(
        "INSERT INTO threads VALUES (?1,?2,'same-name',?3,?4,0)",
        rusqlite::params![
            thread,
            home.to_str().unwrap(),
            now() as i64,
            (now() / 1000) as i64
        ],
    )
    .unwrap();
    format!("codex:{thread}")
}
fn fake_codex(home: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let cli = home.join("codex-cli");
    std::fs::write(
        &cli,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$QUEUE_LOG\"\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli
}
fn journal(home: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(home.join(".telephone/messages.sqlite")).unwrap()
}

#[test]
fn polling_routes_automatically_expires_and_never_replays_native_messages() {
    use serde_json::{json, Value};
    let home = tempfile::tempdir().unwrap();
    let sender = seed_codex(home.path(), "sender");
    let recipient = seed_codex(home.path(), "recipient");
    let cli = fake_codex(home.path());
    let log = home.path().join("queue.log");
    let mut command = isolated(home.path(), Some(&sender));
    command.env("CODEX_CLI_PATH", &cli).env("QUEUE_LOG", &log);
    let mut mcp = McpProcess::from_command(command);
    let native = mcp.tool("send_message", json!({"to":recipient,"body":"native"}));
    assert!(native.contains("Accepted by codex queue; not a read receipt"));
    assert!(native.contains("bypasses telephone inbox/check_inbox"));
    let native_log = std::fs::read(&log).unwrap();
    // CLI inbox advertises its native thread and surfaces separate native history once.
    let empty = isolated(home.path(), Some(&recipient))
        .args(["inbox", "--json"])
        .output()
        .unwrap();
    assert!(empty.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&empty.stdout).unwrap(),
        json!([])
    );
    assert!(String::from_utf8_lossy(&empty.stderr).contains("Native read status is unknown"));
    let quiet = isolated(home.path(), Some(&recipient))
        .args(["inbox", "--json"])
        .output()
        .unwrap();
    assert!(quiet.stderr.is_empty());
    // Same MCP process, same environment, different route based on receiving evidence.
    let sent = mcp.tool("send_message", json!({"to":recipient,"body":"automatic"}));
    assert!(sent.contains("selected polling automatically"));
    assert_eq!(std::fs::read(&log).unwrap(), native_log);
    let received: Value = serde_json::from_str(&success(
        home.path(),
        Some(&recipient),
        &["inbox", "--json"],
    ))
    .unwrap();
    assert_eq!(received.as_array().unwrap().len(), 1);
    assert_eq!(received[0]["body"], "automatic");
    let doctor: Value = serde_json::from_str(&success(
        home.path(),
        None,
        &["doctor", &recipient, "--json"],
    ))
    .unwrap();
    assert_eq!(doctor["preferred_transport"], "inbox");
    assert_eq!(doctor["native_transport"], "queue");
    assert!(doctor["polling"]["expires_at"].as_u64().unwrap() > now());
    let conn = journal(home.path());
    conn.execute("UPDATE receiving SET checked_at=1,expires_at=2", [])
        .unwrap();
    let doctor: Value = serde_json::from_str(&success(
        home.path(),
        None,
        &["doctor", &recipient, "--json"],
    ))
    .unwrap();
    assert_eq!(doctor["preferred_transport"], "queue");
    assert!(doctor["polling"].is_null());
    assert_eq!(
        std::fs::read(&log).unwrap(),
        native_log,
        "doctor never probes or refreshes polling"
    );
    assert!(mcp
        .tool("send_message", json!({"to":recipient,"body":"expired"}))
        .contains("Accepted by codex queue"));
    assert_ne!(std::fs::read(&log).unwrap(), native_log);
    assert!(mcp
        .tool(
            "send_message",
            json!({"to":recipient,"body":"forced","delivery":"inbox"})
        )
        .contains("no native wake-up attempted"));
    let mut receiver = McpProcess::from_command(isolated(home.path(), Some(&recipient)));
    let received: Value = serde_json::from_str(&receiver.tool("check_inbox", json!({}))).unwrap();
    assert_eq!(received["messages"].as_array().unwrap().len(), 1);
    assert!(received["messages"][0].as_str().unwrap().contains("forced"));
    assert_eq!(received["notices"].as_array().unwrap().len(), 0);
    let quiet: Value = serde_json::from_str(&receiver.tool("check_inbox", json!({}))).unwrap();
    assert_eq!(quiet["messages"], json!([]));
    assert_eq!(quiet["notices"].as_array().unwrap().len(), 1);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM inbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "accepted native sends are not mirrored or replayed"
    );
}

#[test]
fn separate_native_mcp_threads_exchange_replies_without_sender_arguments() {
    use serde_json::{json, Value};
    let home = tempfile::tempdir().unwrap();
    let one = seed_codex(home.path(), "one");
    let two = seed_codex(home.path(), "two");
    let mut first = McpProcess::from_command(isolated(home.path(), Some(&one)));
    let mut second = McpProcess::from_command(isolated(home.path(), Some(&two)));
    let listed: Value = serde_json::from_str(&first.tool("list_agents", json!({}))).unwrap();
    assert_eq!(listed["you"], one);
    assert_eq!(listed["agents"].as_array().unwrap().len(), 1);
    assert_eq!(listed["agents"][0]["address"], two);
    second.tool("check_inbox", json!({}));
    let sent = first.tool(
        "send_message",
        json!({"to":two,"body":"request","kind":"request"}),
    );
    assert!(sent.contains("selected polling automatically"));
    assert!(sent.contains("every 2 seconds"));
    // Use CLI in the receiving thread; same native identity and journal as MCP.
    let received: Value =
        serde_json::from_str(&success(home.path(), Some(&two), &["inbox", "--json"])).unwrap();
    let id = received[0]["id"].as_str().unwrap();
    let sent = second.tool(
        "send_message",
        json!({"to":one,"body":"reply","kind":"reply","reply_to":id}),
    );
    assert!(sent.contains("selected polling automatically"));
    assert!(!sent.contains("every 2 seconds"));
    assert!(first.tool("check_inbox", json!({})).contains(id));
    let schema = first.request("tools/list", json!({}));
    let tools = schema["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    for tool in tools {
        let props = tool["inputSchema"]["properties"].as_object().unwrap();
        assert!(!props.contains_key("address"));
        assert!(!props.contains_key("from"));
    }
}

#[test]
fn codex_metadata_keeps_a_shared_mcp_process_bound_to_each_call() {
    use serde_json::{json, Value};
    let home = tempfile::tempdir().unwrap();
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    let one = seed_codex(home.path(), &first);
    let two = seed_codex(home.path(), &second);
    let mut command = isolated(home.path(), None);
    command.env("CODEX_THREAD_ID", uuid::Uuid::new_v4().to_string());
    let mut mcp = McpProcess::from_command(command);
    for (id, address) in [(&first, &one), (&second, &two), (&first, &one)] {
        let response = mcp.request(
            "tools/call",
            json!({"name":"list_agents","arguments":{},"_meta":{"threadId":id}}),
        );
        assert_ne!(response["isError"], true, "{response}");
        let listed: Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(listed["you"], *address);
        assert_eq!(listed["agents"].as_array().unwrap().len(), 1);
    }
    let response = mcp.request("tools/call", json!({"name":"list_agents","arguments":{}}));
    assert_eq!(
        response["isError"], true,
        "missing metadata cannot adopt the inherited outer thread"
    );
    let response = mcp.request(
        "tools/call",
        json!({"name":"check_inbox","arguments":{},"_meta":{"threadId":"bad;id"}}),
    );
    assert_eq!(response["isError"], true);
}

#[test]
fn verified_claude_ancestor_wins_over_inherited_codex_and_stale_pid_variables() {
    let home = tempfile::tempdir().unwrap();
    let (_listener, claude) = native_fixture(home.path());
    let mut command = isolated(home.path(), None);
    command
        .env("CODEX_THREAD_ID", uuid::Uuid::new_v4().to_string())
        .env("CLAUDE_PID", "123");
    let out = command.arg("whoami").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("address: {claude}")));
    let mut command = isolated(home.path(), None);
    command.env("CODEX_THREAD_ID", uuid::Uuid::new_v4().to_string());
    let mut mcp = McpProcess::from_command(command);
    let listed: serde_json::Value =
        serde_json::from_str(&mcp.tool("list_agents", serde_json::json!({}))).unwrap();
    assert_eq!(listed["you"], claude);
}
