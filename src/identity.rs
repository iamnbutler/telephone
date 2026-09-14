//! Working out which agent *we* are.
//!
//! Sending is easy; knowing your own return address is the awkward part. Each
//! runtime leaks a different amount of self-knowledge into the environment of
//! the processes it spawns, so this is a ladder of decreasing confidence.

use crate::registry::Adapter;

#[derive(Debug, Default)]
pub struct Me {
    pub addr: Option<String>,
    pub runtime: Option<&'static str>,
    pub name: Option<String>,
    /// How we worked it out, so `telephone whoami` can be honest about
    /// whether this is known or guessed.
    pub source: &'static str,
}

pub fn whoami() -> Me {
    // 1. Explicit override. Always wins, and is the escape hatch for any
    //    runtime this doesn't handle.
    if let Ok(addr) = std::env::var("TELEPHONE_ADDR") {
        if !addr.trim().is_empty() {
            return Me {
                runtime: addr.split_once(':').map(|(r, _)| leak(r)),
                name: std::env::var("TELEPHONE_NAME").ok(),
                addr: Some(addr),
                source: "TELEPHONE_ADDR",
            };
        }
    }

    // 2. Claude Code hands its children the pid, the socket and the token.
    if let Some(addr) = crate::adapters::claude_code::ClaudeCode::self_address() {
        let name = crate::adapters::claude_code::ClaudeCode::new()
            .ok()
            .and_then(|a| a.discover().ok())
            .and_then(|agents| {
                agents.into_iter().find(|x| x.addr == addr).map(|x| x.name)
            });
        return Me {
            addr: Some(addr),
            runtime: Some("claude"),
            name,
            source: "CLAUDE_PID",
        };
    }

    // 3. Codex tells a spawned MCP server nothing about which thread it is, so
    //    fall back to matching the most recently written rollout for this cwd.
    //    This is a guess and is labelled as one.
    if std::env::var("CODEX_HOME").is_ok() {
        if let Some(agent) = codex_self_guess() {
            return Me {
                addr: Some(agent.addr),
                runtime: Some("codex"),
                name: Some(agent.name),
                source: "codex rollout (inferred from cwd)",
            };
        }
    }

    Me {
        source: "unknown",
        ..Default::default()
    }
}

fn codex_self_guess() -> Option<crate::registry::Agent> {
    let cwd = std::env::current_dir().ok()?;
    let adapter = crate::adapters::codex::Codex::new().ok()?;
    let mut agents = adapter.discover().ok()?;
    agents.retain(|a| a.cwd.as_ref() == Some(&cwd));
    agents.sort_by_key(|a| a.last_seen);
    agents.pop()
}

/// Runtime names are `&'static str` throughout; an override can name a runtime
/// we don't have a compiled-in adapter for, so it gets leaked once at startup.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}
