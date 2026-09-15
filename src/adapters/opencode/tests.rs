use super::*;
use crate::{envelope::Kind, registry::Adapter, store::Store};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    process::{Child, Command, Stdio},
    time::Instant,
};

fn route(root: &std::path::Path) -> Route {
    let credentials = root.join("credentials.json");
    fs::write(
        &credentials,
        r#"{"username":"opencode","password":"isolated-test-password"}"#,
    )
    .unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).unwrap();
    Route {
        endpoint: "http://127.0.0.1:12345".into(),
        session_id: "ses_test".into(),
        directory: root.to_str().unwrap().into(),
        credentials,
    }
}

fn no_model_context() -> PromptContext {
    PromptContext {
        agent: "plan".into(),
        provider: "telephone-test".into(),
        model: "no-model".into(),
        variant: None,
    }
}

fn wait_for_message(route: &Route, env: &Envelope) -> Value {
    let started = Instant::now();
    loop {
        let messages = route
            .client()
            .unwrap()
            .get(&format!("/session/{}/message", route.session_id))
            .unwrap();
        if let Some(message) = messages.as_array().unwrap().iter().find(|message| {
            message["parts"].as_array().unwrap().iter().any(|part| {
                part["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(&env.id.to_string()))
            })
        }) {
            return message.clone();
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn rejects_remote_ambiguous_and_injected_routes() {
    let root = tempfile::tempdir().unwrap();
    let valid = route(root.path());
    for endpoint in [
        "http://localhost:123",
        "https://127.0.0.1:123",
        "http://127.1:123",
        "http://0.0.0.0:123",
        "http://192.168.1.1:123",
        "http://127.0.0.1:0",
        "http://127.0.0.1:123/",
        "http://user@127.0.0.1:123",
        "http://127.0.0.1:123?remote=true",
        "http://[::ffff:127.0.0.1]:123",
    ] {
        let mut invalid = valid.clone();
        invalid.endpoint = endpoint.into();
        assert!(invalid.validate().is_err(), "{endpoint}");
    }
    assert!(valid.validate().is_ok());
    let mut ipv6 = valid.clone();
    ipv6.endpoint = "http://[::1]:12345".into();
    assert!(ipv6.validate().is_ok());
    for id in [
        "ses_",
        "ses_a/../../x",
        "ses_a?x=y",
        "ses_a\r\n",
        "opencode:thread",
    ] {
        let mut invalid = valid.clone();
        invalid.session_id = id.into();
        assert!(invalid.validate().is_err());
    }
    fs::set_permissions(&valid.credentials, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(valid.client().is_err());
    fs::set_permissions(&valid.credentials, fs::Permissions::from_mode(0o600)).unwrap();
    let mut symlink = valid.clone();
    symlink.credentials = root.path().join("link");
    std::os::unix::fs::symlink(&valid.credentials, &symlink.credentials).unwrap();
    assert!(symlink.client().is_err());
}

#[test]
fn binding_lifecycle_uses_real_journal_and_preserves_polling() {
    let root = tempfile::tempdir().unwrap();
    let route = route(root.path());
    let mut store = Store::open(&root.path().join("state")).unwrap();
    let address: Address = "opencode:registered".parse().unwrap();
    assert!(store.bind_opencode(&address, &route).is_err());
    store
        .register(&address, None, crate::envelope::now_millis())
        .unwrap();
    store.bind_opencode(&address, &route).unwrap();
    assert_eq!(
        store.opencode_route(&address).unwrap().unwrap().session_id,
        "ses_test"
    );
    store.unbind_opencode(&address).unwrap();
    assert!(store.opencode_route(&address).unwrap().is_none());
    assert!(store
        .registered_identity(&address, crate::envelope::now_millis())
        .is_ok());
    store.bind_opencode(&address, &route).unwrap();
    store.unregister(&address).unwrap();
    store
        .register(&address, None, crate::envelope::now_millis())
        .unwrap();
    assert!(
        store.opencode_route(&address).unwrap().is_none(),
        "unregister must revoke native binding"
    );
}

#[test]
fn post_disconnect_and_redirect_are_uncertain_without_retry() {
    for redirect in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut route = route(root.path());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        route.endpoint = format!("http://{}", listener.local_addr().unwrap());
        let client = route.client().unwrap();
        // Real TCP fault injection: drop after a request byte or return a redirect.
        let receiver = std::thread::spawn(move || {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(5));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            if redirect {
                stream.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:9/escape\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            drop(stream);
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
                "POST was retried"
            );
        });
        let env = Envelope::new(
            "delta:sender",
            "opencode:receiver",
            Kind::Request,
            "nonce".into(),
        )
        .unwrap();
        let error = client.prompt(&env, &no_model_context(), false).unwrap_err();
        assert!(error.to_string().contains("uncertain"));
        assert!(!format!("{error:#}").contains("isolated-test-password"));
        receiver.join().unwrap();
    }
}

#[test]
fn malformed_binding_does_not_hide_other_registered_threads() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    let mut store = Store::open(&state).unwrap();
    let bad: Address = "opencode:bad".parse().unwrap();
    let good: Address = "opencode:good".parse().unwrap();
    for address in [&bad, &good] {
        store
            .register(address, None, crate::envelope::now_millis())
            .unwrap();
    }
    let db = rusqlite::Connection::open(&store.path).unwrap();
    db.execute(
        "INSERT INTO opencode_routes(address,route) VALUES (?1,?2)",
        rusqlite::params![bad.as_str(), "invalid-json"],
    )
    .unwrap();
    let adapter = super::super::inbox_only::InboxOnly {
        runtime: "opencode",
        root: state,
    };
    let report = adapter.discover().unwrap();
    assert!(!report.complete);
    assert_eq!(report.agents.len(), 1);
    assert_eq!(report.agents[0].addr, good.as_str());
    assert!(!adapter.find_exact(bad.as_str()).unwrap().complete);
}

#[test]
fn http_response_headers_body_and_wait_are_bounded() {
    for fault in ["headers", "body", "stall"] {
        let root = tempfile::tempdir().unwrap();
        let mut route = route(root.path());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        route.endpoint = format!("http://{}", listener.local_addr().unwrap());
        let receiver = std::thread::spawn(move || {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(5));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream.read_exact(&mut [0]).unwrap();
            let response = match fault {
                "headers" => format!("HTTP/1.1 200 OK\r\nX-Padding: {}", "a".repeat(17 * 1024)),
                "body" => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    RESPONSE_LIMIT + 1,
                    "a".repeat(RESPONSE_LIMIT as usize + 1)
                ),
                _ => {
                    std::thread::sleep(Duration::from_millis(3300));
                    return;
                }
            };
            if let Err(error) = stream.write_all(response.as_bytes()) {
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ));
            }
        });
        let start = Instant::now();
        let error = route
            .client()
            .unwrap()
            .get("/session/ses_test")
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5));
        if fault == "stall" {
            assert!(format!("{error:#}").contains("timeout"));
        }
        receiver.join().unwrap();
    }
}

struct Server {
    child: Child,
    root: tempfile::TempDir,
    endpoint: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("stopping isolated OpenCode: {error:#}");
        }
    }
}
impl Server {
    fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            // SAFETY: this child was spawned in its own process group and has not been reaped.
            if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).context("killing test server group");
                }
            }
            self.child.wait().context("reaping test server")?;
        }
        Ok(())
    }
    fn start() -> Self {
        let bin = std::env::var("TELEPHONE_TEST_OPENCODE")
            .expect("set TELEPHONE_TEST_OPENCODE to an absolute OpenCode binary path");
        assert!(std::path::Path::new(&bin).is_absolute());
        let root = tempfile::tempdir().unwrap();
        let stdout = fs::File::create(root.path().join("stdout")).unwrap();
        let stderr = fs::File::create(root.path().join("stderr")).unwrap();
        let child = Command::new(bin).args(["serve", "--pure", "--hostname", "127.0.0.1", "--port", "0"])
            .env_clear().env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root.path()).env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data")).env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("OPENCODE_SERVER_PASSWORD", "isolated-test-password")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1").env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_PROJECT_CONFIG", "1").env("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1")
            .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "1")
            .env("OPENCODE_CONFIG_CONTENT", r#"{"plugin":[],"mcp":{},"enabled_providers":[],"model":"telephone-test/no-model"}"#)
            .current_dir(root.path()).process_group(0).stdin(Stdio::null()).stdout(stdout).stderr(stderr)
            .spawn().unwrap();
        let mut server = Self {
            child,
            root,
            endpoint: String::new(),
        };
        let start = Instant::now();
        loop {
            let output = private_fs::read_owned(&server.root.path().join("stdout"), 65536).unwrap();
            if let Some(endpoint) = output
                .split_whitespace()
                .find(|s| s.starts_with("http://127.0.0.1:"))
            {
                server.endpoint = endpoint.to_owned();
                return server;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "startup timeout");
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "OpenCode exited: {}",
                private_fs::read_owned(&server.root.path().join("stderr"), 65536).unwrap()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
#[ignore = "opt-in: real isolated OpenCode, no model calls; set TELEPHONE_TEST_OPENCODE"]
fn real_opencode_accepts_existing_session_and_falls_back_only_before_post() {
    let mut server = Server::start();
    let home = tempfile::tempdir().unwrap();
    let mut route = route(home.path());
    route.endpoint = server.endpoint.clone();
    route.directory = server
        .root
        .path()
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let client = route.client().unwrap();
    let mut response = client
        .agent
        .post(client.url("/session"))
        .header("Authorization", &client.auth)
        .query("directory", &route.directory)
        .header("Content-Type", "application/json")
        .send("{}")
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let session: Value =
        serde_json::from_str(&response.body_mut().read_to_string().unwrap()).unwrap();
    route.session_id = session["id"].as_str().unwrap().to_owned();
    route.verify().unwrap();
    let mut wrong = route.clone();
    wrong.session_id = "ses_missing".into();
    assert!(wrong.verify().is_err());
    let mut wrong = route.clone();
    wrong.directory.push_str("/other");
    assert!(wrong.verify().is_err());
    let state = home.path().join("telephone-state");
    let mut store = Store::open(&state).unwrap();
    let address: Address = "opencode:test".parse().unwrap();
    store
        .register(&address, None, crate::envelope::now_millis())
        .unwrap();
    store.bind_opencode(&address, &route).unwrap();
    let adapter = super::super::inbox_only::InboxOnly {
        runtime: "opencode",
        root: state.clone(),
    };
    let found = adapter.find_exact(address.as_str()).unwrap();
    let agent = &found.agents[0];
    assert_eq!(agent.transports[0].label(), "http");
    let mut env = Envelope::new(
        "delta:test",
        address.as_str(),
        Kind::Request,
        "native-nonce".into(),
    )
    .unwrap();
    env.add_hop(env.from.clone()).unwrap();
    // Same native endpoint/payload, but suppress model work in this compatibility check.
    route
        .client()
        .unwrap()
        .prompt(&env, &no_model_context(), true)
        .unwrap();
    wait_for_message(&route, &env);
    assert_eq!(
        route.client().unwrap().recorded_context().unwrap().agent,
        "plan"
    );
    // Exercise the production path too. Provider is intentionally nonexistent:
    // the prompt is stored but cannot invoke a model or charge an account.
    let mut next = Envelope::new(
        "delta:test",
        address.as_str(),
        Kind::Request,
        "second native nonce".into(),
    )
    .unwrap();
    next.add_hop(next.from.clone()).unwrap();
    assert!(matches!(
        adapter.deliver(agent, &next).unwrap(),
        Delivered::Accepted { .. }
    ));
    let message = wait_for_message(&route, &next);
    assert_eq!(message["info"]["agent"], "plan");
    assert_eq!(message["info"]["model"]["providerID"], "telephone-test");
    let batch = Store::open(&state).unwrap().inbox(&address, true).unwrap();
    assert!(batch.messages.is_empty());
    batch.acknowledge().unwrap();
    server.stop().unwrap();
    let mut pending = Envelope::new(
        "delta:test",
        address.as_str(),
        Kind::Request,
        "new message after shutdown".into(),
    )
    .unwrap();
    pending.add_hop(pending.from.clone()).unwrap();
    let fallback = adapter.deliver(agent, &pending).unwrap();
    assert!(matches!(fallback, Delivered::Queued { .. }));
    let batch = Store::open(&state).unwrap().inbox(&address, true).unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].id, pending.id);
    batch.acknowledge().unwrap();
}
