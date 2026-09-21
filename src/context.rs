//! Opt-in Claude hook. Read a bounded transcript tail; emit only threshold reminders.
//! Transcript usage is best-effort telemetry, never a lifetime token sum.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 1024 * 1024;
const REMIND_AT: u64 = 250_000;
const COMPACT_AT: u64 = 300_000;

#[derive(Deserialize)]
struct Hook {
    session_id: String,
    transcript_path: PathBuf,
    hook_event_name: String,
}

pub fn run() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_BYTES {
        bail!("hook input exceeds size limit");
    }
    let hook: Hook = serde_json::from_slice(&input)?;
    if !matches!(
        hook.hook_event_name.as_str(),
        "PostToolUse" | "UserPromptSubmit"
    ) {
        return Ok(());
    }
    if hook.session_id.is_empty() || hook.session_id.len() > 256 {
        bail!("invalid hook session id");
    }
    let Some(tokens) = read_usage(&hook.transcript_path, &hook.session_id)? else {
        return Ok(());
    };
    // A subagent can share its parent's session id but has its own transcript.
    let path = hook.transcript_path.canonicalize()?;
    let key = format!(
        "{}:{:x}",
        hook.session_id,
        Sha256::digest(path.as_os_str().as_bytes())
    );
    let level = match tokens {
        COMPACT_AT.. => 2,
        REMIND_AT.. => 1,
        _ => 0,
    };
    if !crate::store::Store::open(&crate::inbox::root()?)?.claim_context_reminder(&key, level)? {
        return Ok(());
    }
    let action = if level == 2 {
        "Compact at the next safe boundary before starting more work."
    } else {
        "Prepare to compact before 300000 tokens."
    };
    let reminder = format!(
        "Context reminder: latest recorded context is approximately {tokens} tokens. {action} \
         Preserve a short checkpoint of the task, decisions, pending work and peer addresses/reply IDs. \
         Use the runtime's compaction facility if available; otherwise surface the need for /compact. \
         Keep peer updates concise."
    );
    let output = json!({"hookSpecificOutput": {
        "hookEventName":hook.hook_event_name,"additionalContext":reminder
    }});
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{output}")?;
    stdout.flush()?;
    Ok(())
}

fn read_usage(path: &Path, session_id: &str) -> Result<Option<u64>> {
    use std::os::unix::fs::MetadataExt;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .context("opening context transcript")?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid takes no arguments and accesses no caller-owned memory.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("transcript must be a regular file owned by this user");
    }
    let start = metadata.len().saturating_sub(MAX_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.take(MAX_BYTES).read_to_end(&mut tail)?;
    // Discard a possibly partial first line and any in-progress final write.
    let first = if start > 0 {
        match tail.iter().position(|b| *b == b'\n') {
            Some(index) => index + 1,
            None => return Ok(None),
        }
    } else {
        0
    };
    let Some(last) = tail.iter().rposition(|b| *b == b'\n') else {
        return Ok(None);
    };
    if first > last {
        return Ok(None);
    }
    Ok(latest_usage(&tail[first..last], session_id))
}

fn latest_usage(tail: &[u8], session_id: &str) -> Option<u64> {
    for line in tail.rsplit(|b| *b == b'\n') {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            // A complete but unparseable record could conceal a compaction.
            return None;
        };
        if record["sessionId"].as_str() != Some(session_id) {
            continue;
        }
        if record["type"] == "system" && record["subtype"] == "compact_boundary" {
            return Some(0);
        }
        if record["type"] != "assistant" {
            continue;
        }
        let usage = &record["message"]["usage"];
        if usage.is_null() {
            continue;
        }
        return usage_tokens(usage);
    }
    None
}

fn usage_tokens(usage: &Value) -> Option<u64> {
    // Newer Claude records can zero the aggregate counters while retaining
    // per-request iterations. Even with aggregates, summing several requests
    // would overestimate the current window: use only the last message iteration.
    let usage = match usage.get("iterations") {
        Some(iterations) => match iterations.as_array()?.last() {
            Some(last) if last["type"] == "message" => last,
            Some(_) => return None,
            None => usage,
        },
        None => usage,
    };
    // Cached input still occupies context; nested cache counters are breakdowns.
    let mut total = usage["input_tokens"].as_u64()?;
    for field in [
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ] {
        let value = match usage.get(field) {
            Some(value) => value.as_u64()?,
            None => 0,
        };
        total = total.checked_add(value)?;
    }
    // Synthetic/unfinished zero usage does not prove that context was compacted.
    (total > 0).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(usage: Value) -> Value {
        json!({"type":"assistant","sessionId":"session","message":{"usage":usage}})
    }

    #[test]
    fn counts_cached_context_once_and_uses_latest_request_only() {
        let earlier = record(json!({"input_tokens":900_000}));
        let latest = record(json!({
            "input_tokens":2,"cache_creation_input_tokens":1893,
            "cache_read_input_tokens":413044,"output_tokens":693,
            "cache_creation":{"ephemeral_5m_input_tokens":1893}
        }));
        let tail = format!("{earlier}\n{latest}");
        assert_eq!(latest_usage(tail.as_bytes(), "session"), Some(415_632));
        assert_eq!(latest_usage(tail.as_bytes(), "other-session"), None);
    }

    #[test]
    fn iteration_usage_survives_zero_aggregates_and_never_sums_requests() {
        for aggregate in [0, 900_000] {
            let usage = json!({
                "input_tokens":aggregate,"cache_read_input_tokens":0,"output_tokens":0,
                "iterations":[
                    {"type":"message","input_tokens":900_000},
                    {"type":"message","input_tokens":2,"cache_creation_input_tokens":1387,
                     "cache_read_input_tokens":503401,"output_tokens":675}
                ]
            });
            assert_eq!(usage_tokens(&usage), Some(505_465));
        }
        assert_eq!(
            usage_tokens(&json!({"input_tokens":0,"output_tokens":0})),
            None
        );
        assert_eq!(
            usage_tokens(&json!({"input_tokens":300_000,"iterations":[{"type":"unknown"}]})),
            None
        );
    }

    #[test]
    fn compaction_and_invalid_usage_cannot_reuse_old_high_counts() {
        let earlier = record(json!({"input_tokens":500_000}));
        let boundary = json!({"type":"system","subtype":"compact_boundary","sessionId":"session"});
        assert_eq!(
            latest_usage(format!("{earlier}\n{boundary}").as_bytes(), "session"),
            Some(0)
        );
        for usage in [
            json!({"input_tokens":-1}),
            json!({"input_tokens":1,"cache_read_input_tokens":"unknown"}),
            json!({"input_tokens":u64::MAX,"output_tokens":1}),
            json!({}),
        ] {
            assert_eq!(
                latest_usage(
                    format!("{earlier}\n{}", record(usage)).as_bytes(),
                    "session"
                ),
                None
            );
        }
        assert_eq!(
            latest_usage(format!("{earlier}\ninvalid").as_bytes(), "session"),
            None
        );
    }

    #[test]
    fn reads_bounded_tail_and_ignores_partial_writes_and_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let earlier = record(json!({"input_tokens":500_000}));
        let latest = record(json!({"input_tokens":12_000}));
        std::fs::write(
            &path,
            format!(
                "{earlier}\n{}\n{latest}\n{{\"type\":",
                "x".repeat(MAX_BYTES as usize)
            ),
        )
        .unwrap();
        assert_eq!(read_usage(&path, "session").unwrap(), Some(12_000));
        std::fs::write(
            &path,
            format!("{earlier}\n{}", "x".repeat(MAX_BYTES as usize)),
        )
        .unwrap();
        assert_eq!(read_usage(&path, "session").unwrap(), None);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_usage(&link, "session").is_err());
        assert!(read_usage(dir.path(), "session").is_err());
    }
}
