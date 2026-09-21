//! Identity is a local routing hint, not authentication. Never guess from cwd.
use crate::{address::Address, runtime::Runtime};
use anyhow::{bail, Context, Result};

#[derive(Debug, Default)]
pub struct Me {
    pub addr: Option<Address>,
    pub runtime: Option<Runtime>,
    pub name: Option<String>,
    pub source: &'static str,
}
fn variable(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("{name} is not valid Unicode")),
    }
}
fn identified(value: String, name: Option<String>, source: &'static str) -> Result<Me> {
    let address: Address = value
        .parse()
        .with_context(|| format!("invalid identity from {source}"))?;
    if name
        .as_ref()
        .is_some_and(|n| n.len() > 256 || n.chars().any(char::is_control))
    {
        bail!("invalid TELEPHONE_NAME");
    }
    let runtime = Some(address.runtime()?);
    Ok(Me {
        addr: Some(address),
        runtime,
        name,
        source,
    })
}
pub fn whoami() -> Result<Me> {
    if let Some(value) = variable("TELEPHONE_ADDR")? {
        return identified(value, variable("TELEPHONE_NAME")?, "TELEPHONE_ADDR");
    }
    for (pid, command) in crate::proc::ancestors()? {
        if matches!(command.as_str(), "codex" | "codex-cli") {
            let id = variable("CODEX_THREAD_ID")?.context("Codex host has no thread identity; use Telephone CLI inside the native thread or MCP with per-call threadId (telephone install)")?;
            return identified(
                format!("codex:{id}"),
                None,
                "Codex ancestor / CODEX_THREAD_ID",
            );
        }
        if let Some(address) = crate::adapters::claude_code::ClaudeCode::address_for_pid(pid)? {
            return identified(address, None, "verified Claude ancestor");
        }
        if command == "claude" {
            bail!("Claude host session identity is unavailable or stale; restart its Telephone MCP process or set TELEPHONE_ADDR explicitly");
        }
    }
    let claude = variable("CLAUDE_PID")?;
    let codex = variable("CODEX_THREAD_ID")?;
    // Both can be inherited when runtimes launch one another. Guessing directs
    // replies to the wrong agent; an explicit override is safer.
    if claude.is_some() && codex.is_some() {
        bail!("conflicting CLAUDE_PID and CODEX_THREAD_ID; set TELEPHONE_ADDR explicitly");
    }
    if let Some(pid) = claude {
        let pid: u32 = pid.parse().context("invalid CLAUDE_PID")?;
        if pid == 0 || pid > i32::MAX as u32 {
            bail!("invalid CLAUDE_PID");
        }
        return identified(format!("claude:{pid}"), None, "CLAUDE_PID");
    }
    if let Some(id) = codex {
        return identified(format!("codex:{id}"), None, "CODEX_THREAD_ID");
    }
    // CODEX_SESSION_ID can describe a session-tree root rather than this thread.
    // It is deliberately not accepted as a thread return address.
    Ok(Me {
        source: "unknown; set TELEPHONE_ADDR",
        ..Me::default()
    })
}

/// Codex supplies the exact thread in tool-call metadata. Claude binds to the
/// verified parent session. Never use an inherited outer Codex ID for MCP.
pub fn for_call(metadata: Option<&serde_json::Map<String, serde_json::Value>>) -> Result<Me> {
    let thread = metadata.and_then(|m| m.get("threadId"));
    let me = if let Some(thread) = thread {
        let thread = thread.as_str().context("MCP threadId must be a string")?;
        uuid::Uuid::parse_str(thread).context("MCP threadId must be a UUID")?;
        let me = identified(format!("codex:{thread}"), None, "Codex MCP threadId")?;
        if let Some(value) = variable("TELEPHONE_ADDR")? {
            let fixed = identified(value, variable("TELEPHONE_NAME")?, "TELEPHONE_ADDR")?;
            if fixed.addr != me.addr {
                bail!("MCP threadId conflicts with TELEPHONE_ADDR; remove the fixed override for a shared Codex host");
            }
        }
        for (pid, command) in crate::proc::ancestors()? {
            if matches!(command.as_str(), "codex" | "codex-cli") {
                break;
            }
            if crate::adapters::claude_code::ClaudeCode::address_for_pid(pid)?.is_some() {
                bail!("Codex MCP threadId conflicts with the verified Claude host");
            }
        }
        me
    } else {
        let me = whoami()?;
        if me.runtime == Some(Runtime::Codex) && me.source != "TELEPHONE_ADDR" {
            bail!("Codex MCP host did not supply per-call threadId. Upgrade the host or use Telephone CLI from this thread's shell. A shared MCP process cannot infer identity from inherited CODEX_THREAD_ID.");
        }
        me
    };
    Ok(me)
}
