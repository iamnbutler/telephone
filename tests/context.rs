use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

fn hook(home: &Path, input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_telephone"))
        .arg("context-hook")
        .env("HOME", home)
        .env_remove("TELEPHONE_DEBUG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap()
}

fn usage(path: &Path, tokens: u64) {
    let line = json!({"type":"assistant","sessionId":"session","message":{
        "usage":{"input_tokens":1,"cache_read_input_tokens":tokens-1}
    }});
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(format!("{line}\n").as_bytes())
        .unwrap();
}

#[test]
fn reminders_cross_thresholds_once_and_rearm_after_compaction() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("transcript.jsonl");
    let input =
        json!({"session_id":"session","transcript_path":path,"hook_event_name":"PostToolUse"})
            .to_string();
    for (tokens, expected) in [
        (249_999, None),
        (250_000, Some("Prepare to compact")),
        (280_000, None),
        (300_000, Some("Compact at the next safe boundary")),
        (505_465, None),
        (270_000, None),
        (20_000, None),
        (250_000, Some("Prepare to compact")),
    ] {
        usage(&path, tokens);
        let output = hook(home.path(), &input);
        match expected {
            None => assert!(output.is_empty(), "{output}"),
            Some(expected) => {
                let value: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PostToolUse");
                assert!(value["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap()
                    .contains(expected));
                assert!(value.get("decision").is_none());
                assert!(output.contains(&tokens.to_string()));
            }
        }
    }
    let boundary = json!({"type":"system","sessionId":"session","subtype":"compact_boundary"});
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(format!("{boundary}\n").as_bytes())
        .unwrap();
    assert!(hook(home.path(), &input).is_empty());
    usage(&path, 310_000);
    assert!(hook(home.path(), &input).contains("Compact at the next safe boundary"));

    // Same parent session id, different subagent transcript: independent budget.
    let subagent = home.path().join("subagent.jsonl");
    usage(&subagent, 260_000);
    let subinput = json!({"session_id":"session","transcript_path":subagent,"hook_event_name":"UserPromptSubmit"});
    let output = hook(home.path(), &subinput.to_string());
    assert!(output.contains("Prepare to compact"));
    assert!(output.contains("UserPromptSubmit"));
}

#[test]
fn parallel_hook_processes_emit_one_reminder_and_failures_stay_quiet() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("transcript.jsonl");
    usage(&path, 20_000);
    let input =
        json!({"session_id":"session","transcript_path":path,"hook_event_name":"PostToolUse"})
            .to_string();
    // Initialize schema before competing connections, as in a running session.
    assert!(hook(home.path(), &input).is_empty());
    usage(&path, 500_000);
    let outputs = std::thread::scope(|scope| {
        let children: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| hook(home.path(), &input)))
            .collect();
        children
            .into_iter()
            .map(|child| child.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        outputs.iter().filter(|output| !output.is_empty()).count(),
        1
    );
    assert!(hook(home.path(), "invalid JSON").is_empty());
    assert!(hook(home.path(), &"x".repeat(1024 * 1024 + 1)).is_empty());
    let stop = input.replace("PostToolUse", "Stop");
    assert!(hook(home.path(), &stop).is_empty());
    fs::remove_file(path).unwrap();
    assert!(hook(home.path(), &input).is_empty());
    // No message was sent or inbox consumed by these hook invocations.
    let db = rusqlite::Connection::open(home.path().join(".telephone/messages.sqlite")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}
