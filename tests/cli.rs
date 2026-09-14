use std::io::Write;
use std::process::{Command, Stdio};

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
