//! Registered inboxes, with an optional explicitly bound OpenCode native route.
use crate::{
    discovery::{Code, Discovery},
    envelope::{now_millis, Envelope},
    registry::{Adapter, Agent, Delivered},
    store::Store,
};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct InboxOnly {
    pub runtime: &'static str,
    pub root: PathBuf,
}

impl InboxOnly {
    fn lookup(&self, exact: Option<&str>) -> Result<Discovery> {
        match std::fs::symlink_metadata(self.root.join("messages.sqlite")) {
            Ok(_) => {} // Store::open checks ownership, links and permissions.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Discovery::default()),
            Err(e) => return Err(e).context("checking registration journal"),
        }
        let store = Store::open(&self.root)?;
        let mut report = store.registrations(self.runtime, exact, now_millis())?;
        if self.runtime == "opencode" {
            let mut warnings = Vec::new();
            report.agents.retain_mut(|agent| {
                match agent
                    .addr
                    .parse()
                    .and_then(|address| store.opencode_route(&address))
                {
                    Ok(Some(route)) => {
                        agent
                            .transports
                            .insert(0, crate::registry::Transport::OpenCodeHttp);
                        agent.cwd = Some(route.directory.into());
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warnings.push(format!("{}: {error:#}", agent.addr));
                        return false;
                    }
                }
                true
            });
            for warning in warnings {
                report.warn(self.runtime, Code::SourceUnavailable, None, warning);
            }
        }
        Ok(report)
    }
}

pub fn adapters(root: &Path) -> Vec<Box<dyn Adapter>> {
    crate::store::registrations::RUNTIMES
        .into_iter()
        .map(|runtime| {
            Box::new(InboxOnly {
                runtime,
                root: root.to_owned(),
            }) as Box<dyn Adapter>
        })
        .collect()
}

impl Adapter for InboxOnly {
    fn runtime(&self) -> &'static str {
        self.runtime
    }
    fn discover(&self) -> Result<Discovery> {
        self.lookup(None)
    }
    fn find_exact(&self, address: &str) -> Result<Discovery> {
        let mut report = self.lookup(Some(address))?;
        if report.complete && report.agents.is_empty() {
            report.note(self.runtime, Code::SourceUnavailable, None,
                "no active inbox registration; the receiving thread must run telephone register or MCP register_agent");
        }
        Ok(report)
    }
    fn deliver(&self, agent: &Agent, env: &Envelope) -> Result<Delivered> {
        if agent.runtime != self.runtime || agent.addr != env.to.as_str() {
            bail!("registered inbox target mismatch");
        }
        let mut store = Store::open(&self.root)?;
        let mut native_note = String::new();
        if self.runtime == "opencode" {
            if let Some(route) = store.opencode_route(&env.to)? {
                match route.deliver(env)? {
                    super::opencode::Attempt::Delivered(delivered) => return Ok(delivered),
                    super::opencode::Attempt::Unavailable(note) => {
                        native_note = format!("{note}. ")
                    }
                }
            }
        }
        store.deposit_registered(&env.to, env, now_millis())?;
        Ok(Delivered::Queued {
            path: store.path,
            note: format!("{native_note}No native wake-up occurred. The recipient must call check_inbox or telephone inbox."),
        })
    }
}
