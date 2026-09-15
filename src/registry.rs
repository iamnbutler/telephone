//! Discovery: who exists, what can they speak, are they alive.
//!
//! Every runtime keeps its own idea of "a live session" in its own format.
//! An [`Adapter`] reads that native registry and normalizes it into [`Agent`]s
//! addressed in one namespace, so the rest of telephone never has to care
//! which runtime something came from.

use crate::discovery::{Code, Discovery};
use crate::envelope::Envelope;
use crate::{address::Address, runtime::Runtime};
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

/// How much we actually know about whether an agent is still running.
///
/// This is a first-class field rather than a footnote because the two cases
/// are genuinely different and were previously presented identically. A
/// registry that says "here are your agents" while quietly mixing live
/// sessions with ones that exited minutes ago is worse than one that admits
/// which is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// We asked something authoritative and it said this session is running:
    /// a live pid whose start time matches, or a runtime status from the
    /// daemon that owns the session.
    Verified,
    /// Nobody can currently tell us. The session was active recently, which
    /// is not the same as being active now -- it may have exited seconds after
    /// its last write.
    Inferred,
}

impl Liveness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Liveness::Verified => "live",
            Liveness::Inferred => "recent?",
        }
    }
}

/// How to actually reach an agent, most faithful first.
#[derive(Clone)]
pub enum Transport {
    /// Claude Code's native per-session Unix socket. Delivers into the live
    /// session as a real turn.
    ClaudeUds {
        socket: PathBuf,
        session_id: String,
        token: Option<String>,
    },
    /// Codex's own CLI, which can push a message into a thread's queue even
    /// when that thread isn't currently open.
    CodexQueue,
    /// An explicitly bound existing OpenCode session; no network discovery.
    OpenCodeHttp,
    /// The universal fallback: a filesystem inbox the agent drains itself,
    /// via the telephone MCP server or a shell hook.
    Inbox,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeUds {
                socket, session_id, ..
            } => f
                .debug_struct("ClaudeUds")
                .field("socket", socket)
                .field("session_id", session_id)
                .field("token", &"[redacted]")
                .finish(),
            Self::CodexQueue => f.write_str("CodexQueue"),
            Self::OpenCodeHttp => f.write_str("OpenCodeHttp"),
            Self::Inbox => f.write_str("Inbox"),
        }
    }
}

impl Transport {
    pub fn label(&self) -> &'static str {
        match self {
            Transport::ClaudeUds { .. } => "uds",
            Transport::CodexQueue => "queue",
            Transport::OpenCodeHttp => "http",
            Transport::Inbox => "inbox",
        }
    }
}

/// A messageable agent, in telephone's namespace.
#[derive(Debug, Clone)]
pub struct Agent {
    /// Canonical address: `<runtime>:<local-id>`, e.g. `claude:83487`.
    addr: Address,
    runtime: Runtime,
    /// Display alias. Convenient to type, but never authoritative: names
    /// collide and get reused, addresses don't.
    pub name: String,
    pub cwd: Option<PathBuf>,
    pub status: Status,
    /// Whether [`Agent::status`] is something we confirmed or merely inferred.
    pub liveness: Liveness,
    /// Epoch millis of the last activity the runtime reported.
    pub last_seen: u64,
    /// Ordered best-first. Delivery walks this and takes the first that works.
    pub transports: Vec<Transport>,
}

impl Agent {
    pub fn new(
        addr: Address,
        name: String,
        cwd: Option<PathBuf>,
        status: Status,
        liveness: Liveness,
        last_seen: u64,
        transports: Vec<Transport>,
    ) -> Result<Self> {
        let runtime = addr.runtime()?;
        Ok(Self {
            addr,
            runtime,
            name,
            cwd,
            status,
            liveness,
            last_seen,
            transports,
        })
    }

    pub fn address(&self) -> &Address {
        &self.addr
    }
    pub fn addr(&self) -> &str {
        self.addr.as_str()
    }
    pub fn runtime(&self) -> Runtime {
        self.runtime
    }

    /// True if `q` names this agent: exact address, exact name, or the
    /// local-id half of the address.
    pub fn matches(&self, q: &str) -> bool {
        if self.addr() == q || self.name == q {
            return true;
        }
        // Allow `83487` as shorthand for `claude:83487`, but only when
        // unambiguous -- the caller is responsible for rejecting multiple hits.
        self.addr.local_id() == q
    }
}

/// What happened when we tried to deliver.
#[derive(Debug)]
pub enum Delivered {
    /// Bytes were written, but the runtime has not confirmed acceptance.
    Unconfirmed { via: &'static str },
    /// The runtime's queue command accepted the message, not necessarily read it.
    Accepted { via: &'static str },
    /// Left in an inbox. `note` explains how the agent will see it, because
    /// "queued" without that is indistinguishable from "lost".
    Queued { path: PathBuf, note: String },
}

/// One runtime's integration: how to find its sessions and how to talk to them.
pub trait Adapter {
    fn runtime(&self) -> Runtime;

    /// Enumerate live sessions. Should return `Ok(vec![])` rather than erroring
    /// when the runtime simply isn't installed.
    fn discover(&self) -> Result<Discovery>;

    fn discover_all(&self) -> Result<Discovery> {
        self.discover()
    }

    fn find_exact(&self, address: &Address) -> Result<Discovery>;

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
    pub fn discover(&self) -> Discovery {
        self.discover_with(false)
    }

    pub fn discover_with(&self, all: bool) -> Discovery {
        let mut report = Discovery::default();
        for adapter in &self.adapters {
            match if all {
                adapter.discover_all()
            } else {
                adapter.discover()
            } {
                Ok(found) => report.merge(found),
                Err(e) => report.warn(
                    adapter.runtime(),
                    Code::SourceUnavailable,
                    None,
                    format!("{e:#}"),
                ),
            }
        }
        // Most recently active first.
        report
            .agents
            .sort_by_key(|a| std::cmp::Reverse(a.last_seen));
        report
    }

    pub fn adapter_for(&self, agent: &Agent) -> Option<&dyn Adapter> {
        self.adapters
            .iter()
            .find(|a| a.runtime() == agent.runtime())
            .map(|a| a.as_ref())
    }

    /// Resolve a user-typed name or address to exactly one agent.
    pub fn resolve(&self, report: &mut Discovery, q: &str) -> Result<Agent> {
        if q.contains(':') {
            let address: crate::address::Address = q.parse()?;
            if let Some(agent) = report.agents.iter().find(|a| a.addr() == address.as_str()) {
                return Ok(agent.clone());
            }
            let runtime = address.runtime()?;
            let adapter = self
                .adapters
                .iter()
                .find(|a| a.runtime() == runtime)
                .ok_or_else(|| anyhow::anyhow!("unsupported address runtime"))?;
            let exact = adapter.find_exact(&address)?;
            let complete = exact.complete;
            let agent = exact.agents.iter().find(|a| a.addr() == q).cloned();
            report.merge(exact);
            return agent.ok_or_else(|| if complete {
                anyhow::anyhow!("no agent matches this exact address")
            } else {
                anyhow::anyhow!("exact-address lookup was incomplete; absence is not confirmed (inspect discovery warnings)")
            });
        }
        if !report.complete {
            anyhow::bail!("discovery is incomplete; use an exact address instead of a potentially ambiguous name");
        }
        let hits: Vec<&Agent> = report.agents.iter().filter(|a| a.matches(q)).collect();
        match hits.len() {
            0 => anyhow::bail!("no agent matches '{q}' (try `telephone list`)"),
            1 => Ok(hits[0].clone()),
            _ => {
                let names: Vec<&str> = hits.iter().map(|a| a.addr()).collect();
                anyhow::bail!("'{q}' is ambiguous: {}", names.join(", "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(addr: &str, name: &str) -> Agent {
        Agent::new(
            addr.parse().unwrap(),
            name.into(),
            None,
            Status::Idle,
            Liveness::Verified,
            0,
            vec![Transport::Inbox],
        )
        .unwrap()
    }

    #[test]
    fn agent_runtime_comes_from_its_address_and_unknown_runtimes_cannot_route() {
        assert_eq!(agent("claude:1", "a").runtime(), Runtime::Claude);
        assert_eq!(agent("codex:1", "a").runtime(), Runtime::Codex);
        assert!(Agent::new(
            "unknown:1".parse().unwrap(),
            "a".into(),
            None,
            Status::Unknown,
            Liveness::Inferred,
            0,
            vec![Transport::Inbox]
        )
        .is_err());
        let registry = Registry::new(vec![]);
        let error = registry
            .resolve(&mut Discovery::default(), "unknown:1")
            .unwrap_err();
        assert!(error
            .downcast_ref::<crate::runtime::UnsupportedRuntime>()
            .is_some());
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
        let mut report = Discovery {
            agents: vec![agent("claude:1", "dup"), agent("codex:2", "dup")],
            ..Discovery::default()
        };
        let err = registry
            .resolve(&mut report, "dup")
            .expect_err("should refuse");
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn resolving_an_unknown_name_points_at_list() {
        let registry = Registry::new(vec![]);
        let err = registry
            .resolve(&mut Discovery::default(), "nobody")
            .expect_err("should fail");
        assert!(err.to_string().contains("telephone list"));
    }
}
