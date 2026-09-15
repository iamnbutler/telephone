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
    let runtime = address.runtime().ok();
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
    if let Some(address) = crate::adapters::claude_code::ClaudeCode::parent_address()? {
        return identified(address, None, "Claude parent session");
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

/// Per-call identities let a shared MCP server serve distinct threads. These are
/// same-user routing hints, not credentials; never accept them from a network.
pub fn for_call(explicit: Option<&str>, root: &std::path::Path) -> Result<Me> {
    let me = match explicit {
        Some(address) => identified(address.to_owned(), None, "explicit local inbox")?,
        None => whoami()?,
    };
    if let Some(address) = &me.addr {
        if address.inbox_runtime().is_some() {
            return crate::store::Store::open(root)?
                .registered_identity(address, crate::envelope::now_millis());
        }
    }
    if explicit.is_some() {
        bail!("explicit MCP identity must be an opencode:, zed:, delta: or generic: registration");
    }
    Ok(me)
}
