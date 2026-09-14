//! Real child servers, Unix sockets and SQLite locks. No adapter/reader mocks,
//! home-directory overrides, live-agent messages, or shared test environment.
use super::session;
use crate::{
    envelope::{Envelope, Kind},
    store::Store,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::{
        fd::AsFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
#[ignore = "subprocess helper; exercised by MCP socket tests"]
fn server_child() {
    let root = PathBuf::from(std::env::var_os("TELEPHONE_TEST_MCP_ROOT").unwrap());
    let stream =
        UnixStream::connect(std::env::var_os("TELEPHONE_TEST_MCP_SOCKET").unwrap()).unwrap();
    socket2::SockRef::from(&stream)
        .set_send_buffer_size(4096)
        .unwrap();
    let result = session::serve(stream.as_fd(), stream.as_fd(), root.clone());
    let text = result.map_or_else(|e| format!("{e:#}"), |_| "ok".into());
    fs::write(root.join("result"), text).unwrap();
}

struct Server {
    child: Child,
    stream: Option<UnixStream>,
    root: tempfile::TempDir,
    received: Vec<u8>,
    reaped: bool,
}
impl Server {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        drop(Store::open(root.path()).unwrap());
        let socket = root.path().join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mcp::integration::server_child", "--ignored"])
            .env("TELEPHONE_TEST_MCP_ROOT", root.path())
            .env("TELEPHONE_TEST_MCP_SOCKET", &socket)
            .env("TELEPHONE_ADDR", "test:receiver")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        && start.elapsed() < Duration::from_secs(5) =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) => {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("child did not connect: {e}");
                }
            }
        };
        stream.set_nonblocking(true).unwrap();
        let mut server = Self {
            child,
            stream: Some(stream),
            root,
            received: Vec::new(),
            reaped: false,
        };
        server.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"real-socket-test","version":"1"}}}));
        assert_eq!(server.reply()["id"], 0);
        server.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        server
    }
    fn socket(&self) -> &UnixStream {
        self.stream.as_ref().unwrap()
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.try_bytes(bytes).unwrap();
    }
    fn try_bytes(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        let start = Instant::now();
        while !bytes.is_empty() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "test input stalled"
            );
            match self.socket().write(bytes) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.bytes(&bytes);
    }
    fn ping(&mut self, id: i32) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"ping"}));
    }
    fn inbox(&mut self, id: i32) {
        self.send(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"check_inbox"}}),
        );
    }
    fn cancel(&mut self, id: i32) {
        self.send(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}),
        );
    }
    fn reply(&mut self) -> Value {
        self.reply_with_timeout(Duration::from_secs(2))
    }
    fn reply_with_timeout(&mut self, timeout: Duration) -> Value {
        let start = Instant::now();
        let mut scanned = 0;
        loop {
            // Scan each byte once, including when the socket temporarily has no
            // data. Re-scanning an accumulating bulk reply is quadratic work.
            if let Some(relative_end) = self.received[scanned..].iter().position(|&b| b == b'\n') {
                let end = scanned + relative_end;
                let bytes: Vec<_> = self.received.drain(..=end).collect();
                return serde_json::from_slice(&bytes).unwrap();
            }
            scanned = self.received.len();
            assert!(
                start.elapsed() < timeout,
                "MCP response exceeded {timeout:?}; buffered {} bytes",
                self.received.len()
            );
            let mut buffer = [0; 64 * 1024];
            match self.socket().read(&mut buffer) {
                Ok(0) => panic!("MCP socket closed before response"),
                Ok(n) => self.received.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("MCP output failed: {e}"),
            }
        }
    }
    fn seed(&self, count: usize, size: usize) {
        let mut store = Store::open(self.root.path()).unwrap();
        for _ in 0..count {
            let mut env = Envelope::new(
                "test:sender",
                "test:receiver",
                Kind::Inform,
                "x".repeat(size),
            )
            .unwrap();
            env.add_hop(env.from.clone()).unwrap();
            store.deposit(&env.to, &env).unwrap();
        }
    }
    fn connection(&self) -> Connection {
        let conn = Connection::open(self.root.path().join("messages.sqlite")).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        conn
    }
    fn unread(&self) -> i64 {
        self.connection()
            .query_row("SELECT COUNT(*) FROM inbox WHERE read=0", [], |r| r.get(0))
            .unwrap()
    }
    fn wait_for_inbox_lock(&self) {
        let conn = self.connection();
        let start = Instant::now();
        loop {
            match conn.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => conn.execute_batch("ROLLBACK").unwrap(),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy =>
                {
                    return
                }
                Err(e) => panic!("unexpected database failure: {e}"),
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "server never acquired inbox transaction"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn finish(&mut self) -> String {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reaped = true;
                assert!(status.success(), "test child failed: {status}");
                return fs::read_to_string(self.root.path().join("result")).unwrap();
            }
            assert!(
                start.elapsed() < Duration::from_secs(9),
                "MCP child did not exit"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if !self.reaped {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
}

#[test]
fn stalled_output_rolls_back_inbox_and_releases_the_writer_lock() {
    let mut server = Server::start();
    server.seed(20, 64 * 1024);
    server.inbox(1);
    server.wait_for_inbox_lock();
    // Deliberately never read the response, keeping the socket's receive end open.
    assert!(server.finish().contains("output deadline"));
    assert_eq!(server.unread(), 20);
    server
        .connection()
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
        .unwrap();
}

#[test]
fn partial_input_times_out_but_an_idle_session_does_not() {
    let mut idle = Server::start();
    let mut partial = Server::start();
    partial.bytes(b"{\"jsonrpc\":");
    assert!(partial.finish().contains("input deadline"));
    // The second session had no partial frame; it must remain usable after 5s.
    idle.ping(1);
    assert_eq!(idle.reply()["id"], 1);
    idle.socket().shutdown(Shutdown::Write).unwrap();
    assert_eq!(idle.finish(), "ok");
}

#[test]
fn ping_and_cancellation_work_while_a_real_sqlite_writer_blocks_the_tool() {
    let mut server = Server::start();
    server.seed(1, 16);
    let conn = server.connection();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    server.inbox(1);
    thread::sleep(Duration::from_millis(100));
    server.ping(2);
    assert_eq!(
        server.reply()["id"],
        2,
        "ping waited behind the locked inbox"
    );
    server.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"test:absent","body":"must not start"}}}));
    assert_eq!(server.reply()["error"]["code"], -32000);
    server.cancel(1);
    server.ping(4);
    assert_eq!(server.reply()["id"], 4);
    conn.execute_batch("ROLLBACK").unwrap();
    server.socket().shutdown(Shutdown::Write).unwrap();
    assert_eq!(server.finish(), "ok");
    assert_eq!(server.unread(), 1);
    // EOF without any further frame proves the cancelled response was suppressed.
    let mut bytes = [0; 16];
    assert_eq!(server.socket().read(&mut bytes).unwrap(), 0);
    assert!(server.received.is_empty());
}

#[test]
fn cancellation_during_partial_output_preserves_framing_but_not_the_receipt() {
    let mut server = Server::start();
    server.seed(10, 64 * 1024);
    server.inbox(1);
    server.wait_for_inbox_lock();
    // Wait for actual bytes, without draining the large response.
    let start = Instant::now();
    loop {
        let mut byte = [0];
        match server.socket().read(&mut byte) {
            Ok(1) => {
                server.received.push(byte[0]);
                break;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(2) =>
            {
                thread::sleep(Duration::from_millis(2))
            }
            other => panic!("did not observe a partial frame: {other:?}"),
        }
    }
    server.cancel(1);
    server.ping(2);
    // This is a bulk drain, not a control-latency assertion. The production
    // server still enforces its unchanged five-second output deadline. Allow
    // the debug-build client to drain the large frame or observe that failure.
    assert_eq!(server.reply_with_timeout(Duration::from_secs(6))["id"], 1);
    assert_eq!(server.reply()["id"], 2);
    assert_eq!(server.unread(), 10);
    server.socket().shutdown(Shutdown::Write).unwrap();
    assert_eq!(server.finish(), "ok");
}

#[test]
fn buffered_socket_replies_preserve_the_following_frame() {
    let mut server = Server::start();
    server.ping(1);
    server.ping(2);
    // Read both real server replies into the buffer before parsing either one.
    // This makes the coalesced-frame case deterministic without a reader mock.
    let start = Instant::now();
    while server.received.iter().filter(|&&b| b == b'\n').count() < 2 {
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut bytes = [0; 1024];
        match server.socket().read(&mut bytes) {
            Ok(0) => panic!("server closed before both ping replies"),
            Ok(n) => server.received.extend_from_slice(&bytes[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => panic!("reading ping replies failed: {e}"),
        }
    }
    assert_eq!(server.reply()["id"], 1);
    assert!(!server.received.is_empty());
    assert_eq!(server.reply()["id"], 2);
    assert!(server.received.is_empty());
    server.socket().shutdown(Shutdown::Write).unwrap();
    assert_eq!(server.finish(), "ok");
}

#[test]
fn broken_output_and_truncated_input_roll_back_pending_receipts() {
    for truncated in [false, true] {
        let mut server = Server::start();
        server.seed(10, 64 * 1024);
        server.inbox(1);
        server.wait_for_inbox_lock();
        if truncated {
            server.bytes(b"{\"jsonrpc\":");
            server.socket().shutdown(Shutdown::Write).unwrap();
        } else {
            drop(server.stream.take());
        }
        let result = server.finish();
        assert!(
            result.contains(if truncated {
                "midway"
            } else {
                "writing MCP response"
            }),
            "{result}"
        );
        assert_eq!(server.unread(), 10);
    }
}

#[test]
fn a_drained_response_commits_and_a_control_flood_has_a_bounded_queue() {
    let mut server = Server::start();
    server.seed(1, 16);
    server.inbox(1);
    assert_eq!(server.reply()["id"], 1);
    server.ping(2); // processed after commit, not merely after the bytes arrived
    assert_eq!(server.reply()["id"], 2);
    assert_eq!(server.unread(), 0);
    // Large echoed IDs fill the real socket before the queue cap is reached.
    for index in 0..40 {
        let mut bytes = serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":format!("{index}-{}", "x".repeat(4096)),"method":"ping"}),
        )
        .unwrap();
        bytes.push(b'\n');
        if let Err(e) = server.try_bytes(&bytes) {
            assert!(matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ));
            break;
        }
    }
    assert!(server.finish().contains("output queue limit"));
}

#[test]
fn process_death_during_output_keeps_inbox_messages_pending() {
    let mut server = Server::start();
    server.seed(10, 64 * 1024);
    server.inbox(1);
    server.wait_for_inbox_lock();
    server.child.kill().unwrap();
    assert!(!server.child.wait().unwrap().success());
    server.reaped = true;
    assert_eq!(server.unread(), 10);
    server
        .connection()
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
        .unwrap();
}

#[test]
fn malformed_and_unknown_cancellations_do_not_cancel_another_request() {
    let mut server = Server::start();
    server.seed(1, 16);
    let conn = server.connection();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    server.inbox(1);
    server.cancel(99);
    server.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":7}}));
    server.ping(2);
    assert_eq!(server.reply()["id"], 2);
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(server.reply()["id"], 1);
    server
        .stream
        .as_ref()
        .unwrap()
        .shutdown(Shutdown::Write)
        .unwrap();
    assert_eq!(server.finish(), "ok");
    assert_eq!(server.unread(), 0);
}
