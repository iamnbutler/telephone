//! Discovery: who exists, what can they speak, are they alive.
//!
//! Every runtime keeps its own idea of "a live session" in its own format.
//! An [`Adapter`] reads that native registry and normalizes it into [`Agent`]s
//! addressed in one namespace, so the rest of telephone never has to care
//! which runtime something came from.

use crate::envelope::Envelope;
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Busy,
    Unknown,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Busy => "busy",
            Status::Unknown => "unknown",
        }
    }
}

/// How to actually reach an agent, most faithful first.
#[derive(Debug, Clone)]
pub enum Transport {
    /// Claude Code's native per-session Unix socket. Delivers into the live
    /// session as a real turn.
    ClaudeUds {
        socket: PathBuf,
        session_id: String,
        token: Option<String>,
    },
    /// The universal fallback: a filesystem inbox the agent drains itself,
    /// via the telephone MCP server or a shell hook.
    Inbox,
}

impl Transport {
    pub fn label(&self) -> &'static str {
        match self {
            Transport::ClaudeUds { .. } => "uds",
            Transport::Inbox => "inbox",
        }
    }
}

/// A messageable agent, in telephone's namespace.
#[derive(Debug, Clone)]
pub struct Agent {
    /// Canonical address: `<runtime>:<local-id>`, e.g. `claude:83487`.
    pub addr: String,
    pub runtime: &'static str,
    /// Display alias. Convenient to type, but never authoritative: names
    /// collide and get reused, addresses don't.
    pub name: String,
    pub cwd: Option<PathBuf>,
    pub status: Status,
    /// Epoch millis of the last activity the runtime reported.
    pub last_seen: u64,
    /// Ordered best-first. Delivery walks this and takes the first that works.
    pub transports: Vec<Transport>,
}

impl Agent {
    /// True if `q` names this agent: exact address, exact name, or the
    /// local-id half of the address.
    pub fn matches(&self, q: &str) -> bool {
        if self.addr == q || self.name == q {
            return true;
        }
        // Allow `83487` as shorthand for `claude:83487`, but only when
        // unambiguous -- the caller is responsible for rejecting multiple hits.
        self.addr.split_once(':').map(|(_, id)| id) == Some(q)
    }
}

/// What happened when we tried to deliver.
#[derive(Debug)]
pub enum Delivered {
    /// Went into the live session over its own protocol.
    Native { via: &'static str },
    /// Left in an inbox. `note` explains how the agent will see it, because
    /// "queued" without that is indistinguishable from "lost".
    Queued { path: PathBuf, note: String },
}

/// One runtime's integration: how to find its sessions and how to talk to them.
pub trait Adapter {
    fn runtime(&self) -> &'static str;

    /// Enumerate live sessions. Should return `Ok(vec![])` rather than erroring
    /// when the runtime simply isn't installed.
    fn discover(&self) -> Result<Vec<Agent>>;

    /// Deliver `env` to `agent`, which this adapter produced.
    fn deliver(&self, agent: &Agent, env: &Envelope) -> Result<Delivered>;
}

/// Collects agents across every adapter.
pub struct Registry {
    pub adapters: Vec<Box<dyn Adapter>>,
}

impl Registry {
    pub fn new(adapters: Vec<Box<dyn Adapter>>) -> Self {
        Registry { adapters }
    }

    /// Discovery is best-effort per adapter: one broken runtime shouldn't
    /// hide every other agent on the box.
    pub fn discover(&self) -> (Vec<Agent>, Vec<String>) {
        let mut agents = Vec::new();
        let mut warnings = Vec::new();
        for adapter in &self.adapters {
            match adapter.discover() {
                Ok(found) => agents.extend(found),
                Err(e) => warnings.push(format!("{}: {e}", adapter.runtime())),
            }
        }
        agents.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        (agents, warnings)
    }

    pub fn adapter_for(&self, agent: &Agent) -> Option<&dyn Adapter> {
        self.adapters
            .iter()
            .find(|a| a.runtime() == agent.runtime)
            .map(|a| a.as_ref())
    }

    /// Resolve a user-typed name or address to exactly one agent.
    pub fn resolve(&self, agents: &[Agent], q: &str) -> Result<Agent> {
        let hits: Vec<&Agent> = agents.iter().filter(|a| a.matches(q)).collect();
        match hits.len() {
            0 => anyhow::bail!("no agent matches '{q}' (try `telephone list`)"),
            1 => Ok(hits[0].clone()),
            _ => {
                let names: Vec<&str> = hits.iter().map(|a| a.addr.as_str()).collect();
                anyhow::bail!("'{q}' is ambiguous: {}", names.join(", "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(addr: &str, name: &str) -> Agent {
        Agent {
            addr: addr.into(),
            runtime: "claude",
            name: name.into(),
            cwd: None,
            status: Status::Idle,
            last_seen: 0,
            transports: vec![Transport::Inbox],
        }
    }

    #[test]
    fn an_agent_answers_to_its_address_name_and_bare_id() {
        let a = agent("claude:83487", "luau-4c");
        assert!(a.matches("claude:83487"));
        assert!(a.matches("luau-4c"));
        assert!(a.matches("83487"));
        assert!(!a.matches("claude:99999"));
        assert!(!a.matches("other-name"));
    }

    #[test]
    fn ambiguous_names_are_an_error_rather_than_a_guess() {
        // Sending to the wrong agent is worse than refusing to send.
        let registry = Registry::new(vec![]);
        let agents = vec![agent("claude:1", "dup"), agent("codex:2", "dup")];
        let err = registry.resolve(&agents, "dup").expect_err("should refuse");
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn resolving_an_unknown_name_points_at_list() {
        let registry = Registry::new(vec![]);
        let err = registry.resolve(&[], "nobody").expect_err("should fail");
        assert!(err.to_string().contains("telephone list"));
    }
}
