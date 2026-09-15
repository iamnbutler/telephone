//! Known runtimes without a verified native route. An explicit lease makes them pollable.
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
        Store::open(&self.root)?.registrations(self.runtime, exact, now_millis())
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
        store.deposit_registered(&env.to, env, now_millis())?;
        Ok(Delivered::Queued {
            path: store.path,
            note: "No native wake-up is available for this route. The recipient must call check_inbox or telephone inbox.".into(),
        })
    }
}
