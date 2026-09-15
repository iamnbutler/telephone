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

fn assert_polling_text(text: &str, address: &str) {
    assert!(
        text.contains("Telephone will not wake this thread"),
        "{text}"
    );
    assert!(
        text.contains(&format!("check_inbox with {{\"address\":\"{address}\"}}")),
        "{text}"
    );
    assert!(text.contains(&format!("TELEPHONE_ADDR='{address}' telephone inbox")));
    assert!(text.contains("every 2 seconds for up to 30 seconds"));
    assert!(text.contains("otherwise report it pending"));
    assert!(text.contains("Do not resend because the inbox is empty"));
    assert!(text.contains("acknowledge acknowledgments"));
}

#[test]
fn cli_registration_teaches_polling_without_changing_address_only_stdout() {
    use serde_json::Value;
    let home = tempfile::tempdir().unwrap();
    let output = isolated(home.path(), None)
        .args(["register", "--runtime", "delta"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let address = stdout.trim();
    assert_eq!(stdout.lines().count(), 1);
    assert!(address.starts_with("delta:"));
    assert!(uuid::Uuid::parse_str(address.strip_prefix("delta:").unwrap()).is_ok());
    assert_polling_text(&String::from_utf8(output.stderr).unwrap(), address);

    let report: Value = serde_json::from_str(&success(
        home.path(),
        Some(address),
        &["register", "--json"],
    ))
    .unwrap();
    assert_eq!(report["address"], address);
    assert!(report["last_seen"].is_number());
    assert!(report["expires_at"].is_number());
    assert_eq!(report["receiving"]["check_inbox"]["tool"], "check_inbox");
    assert_eq!(
        report["receiving"]["check_inbox"]["arguments"]["address"],
        address
    );
    assert!(report["receiving"]["instructions"]
        .as_str()
        .unwrap()
        .contains("poll your own inbox"));
}

#[test]
fn doctor_reports_configured_routes_without_claiming_or_probing_reachability() {
    let home = tempfile::tempdir().unwrap();
    let registered = success(home.path(), None, &["register", "--runtime", "opencode"]);
    let address = registered.trim();
    // Seed persisted configuration, not a fake HTTP response. There is no server
    // accepting requests and no credential file; discovery must not require either.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let route = serde_json::json!({
        "endpoint": format!("http://{}", listener.local_addr().unwrap()),
        "session_id": "ses_doctor_test",
        "directory": home.path(),
        "credentials": home.path().join("absent-credentials.json"),
    });
    let db = rusqlite::Connection::open(home.path().join(".telephone/messages.sqlite")).unwrap();
    db.execute(
        "INSERT INTO opencode_routes(address,route) VALUES (?1,?2)",
        rusqlite::params![address, route.to_string()],
    )
    .unwrap();
    drop(db);

    let output = success(home.path(), Some(address), &["doctor"]);
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("opencode "))
        .unwrap();
    assert!(line.contains("1 native route(s) configured"), "{output}");
    assert!(line.contains("0 confirmed live"), "{output}");
    assert!(!output.contains("reachable natively"), "{output}");
    assert!(output.contains("not proof of reachability or receipt"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "doctor must not probe a configured OpenCode endpoint"
    );
}

#[test]
fn separate_cli_processes_exchange_replies_for_each_registered_runtime() {
    use serde_json::Value;
    let home = tempfile::tempdir().unwrap();
    let first = success(home.path(), None, &["register", "--runtime", "generic"]);
    let first = first.trim();
    for runtime in ["opencode", "zed", "delta"] {
        let second = success(home.path(), None, &["register", "--runtime", runtime]);
        let second = second.trim();
        assert!(second.starts_with(&format!("{runtime}:")));
        let out = success(
            home.path(),
            Some(first),
            &["send", second, "hello", "--kind", "request"],
        );
        assert!(out.contains("Queued in Telephone inbox"));
        assert_polling_text(&out, first);
        let inbox: Value =
            serde_json::from_str(&success(home.path(), Some(second), &["inbox", "--json"]))
                .unwrap();
        assert_eq!(inbox[0]["from"], first);
        assert_eq!(inbox[0]["body"], "hello");
        let id = inbox[0]["id"].as_str().unwrap();
        assert!(out.contains(&format!("in reply to'): {id}.")));
        success(
            home.path(),
            Some(second),
            &["send", first, "reply", "--kind", "reply", "--reply-to", id],
        );
        let reply: Value =
            serde_json::from_str(&success(home.path(), Some(first), &["inbox", "--json"])).unwrap();
        assert_eq!(reply[0]["reply_to"], id);
        assert_eq!(reply[0]["conversation"], inbox[0]["conversation"]);
        assert_eq!(reply[0]["hop_chain"].as_array().unwrap().len(), 2);
        assert_eq!(reply[0]["from"], second);
        assert_eq!(
            success(home.path(), Some(first), &["inbox", "--json"]).trim(),
            "[]"
        );
        success(home.path(), Some(second), &["unregister"]);
        let rejected = isolated(home.path(), Some(first))
            .args(["send", second, "after unregister"])
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("no agent matches"));
    }
    let rejected = isolated(home.path(), Some("delta:never-registered"))
        .args(["send", first, "one-way trap"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("not registered"));
}

struct McpProcess {
    child: std::process::Child,
    writer: std::os::unix::net::UnixStream,
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    next_id: u64,
}
impl McpProcess {
    fn start(home: &std::path::Path) -> Self {
        use std::{os::fd::OwnedFd, os::unix::net::UnixStream, time::Duration};
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let child = isolated(home, None)
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

#[test]
fn shared_mcp_server_keeps_thread_addresses_separate_and_cli_can_reply() {
    use serde_json::{json, Value};
    let home = tempfile::tempdir().unwrap();
    let mut mcp = McpProcess::start(home.path());
    let one: Value = serde_json::from_str(&mcp.tool(
        "register_agent",
        json!({"runtime":"zed","name":"same-name"}),
    ))
    .unwrap();
    // Execute the returned recipe against the actual MCP process, not a mock.
    let recipe = &one["receiving"]["check_inbox"];
    let empty: Value = serde_json::from_str(&mcp.tool(
        recipe["tool"].as_str().unwrap(),
        recipe["arguments"].clone(),
    ))
    .unwrap();
    assert_eq!(empty["messages"], json!([]));
    assert_eq!(recipe["arguments"]["address"], one["address"]);
    let two: Value = serde_json::from_str(&mcp.tool(
        "register_agent",
        json!({"runtime":"zed","name":"same-name"}),
    ))
    .unwrap();
    let one = one["address"].as_str().unwrap();
    let two = two["address"].as_str().unwrap();
    assert_ne!(one, two);
    let listed: Value =
        serde_json::from_str(&mcp.tool("list_agents", json!({"address":one}))).unwrap();
    assert_eq!(listed["you"], one);
    assert_eq!(listed["agents"].as_array().unwrap().len(), 1);
    assert_eq!(listed["agents"][0]["address"], two);
    assert_eq!(listed["agents"][0]["transport"], "inbox");
    let sent = mcp.tool(
        "send_message",
        json!({"from":one,"to":two,"body":"thread-specific request","kind":"request"}),
    );
    assert_polling_text(&sent, one);
    let inbox: Value =
        serde_json::from_str(&success(home.path(), Some(two), &["inbox", "--json"])).unwrap();
    let id = inbox[0]["id"].as_str().unwrap();
    success(
        home.path(),
        Some(two),
        &[
            "send",
            one,
            "thread-specific reply",
            "--kind",
            "reply",
            "--reply-to",
            id,
        ],
    );
    let response = mcp.tool("check_inbox", json!({"address":one}));
    assert!(response.contains("thread-specific reply"));
    assert!(response.contains(id));
    let empty: Value =
        serde_json::from_str(&mcp.tool("check_inbox", json!({"address":two}))).unwrap();
    assert_eq!(empty["messages"], json!([]));
    mcp.tool("unregister_agent", json!({"address":one}));
    let rejected = mcp.request(
        "tools/call",
        json!({"name":"send_message","arguments":{"from":one,"to":two,"body":"expired"}}),
    );
    assert_eq!(rejected["isError"], true);
    let rejected = mcp.request(
        "tools/call",
        json!({"name":"check_inbox","arguments":{"address":"claude:123"}}),
    );
    assert_eq!(rejected["isError"], true);
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
        json!({"pid":pid,"sessionId":"socket-fixture","messagingSocketPath":socket}).to_string(),
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
fn native_socket_then_registered_reply_and_safe_fallback_use_real_processes() {
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
    let sender = success(home.path(), None, &["register", "--runtime", "delta"]);
    let sender = sender.trim();
    let out = success(
        home.path(),
        Some(sender),
        // The real-client regression used inform despite asking for a reply.
        &["send", &claude, "native request", "--kind", "inform"],
    );
    assert!(out.contains("Written over uds"));
    assert_polling_text(&out, sender);
    let received = receiving.join().unwrap();
    let id = received["msg_id"].as_str().unwrap();
    assert!(out.contains(&format!("in reply to'): {id}.")));
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
    assert_polling_text(&out, sender);
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
    let sender = success(home.path(), None, &["register", "--runtime", "opencode"]);
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
    assert_polling_text(&error, sender.trim());
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
