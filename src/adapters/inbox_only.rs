//! Discovery and delivery for explicitly registered local inboxes.
use crate::{
    address::Address,
    discovery::{Code, Discovery},
    envelope::{now_millis, Envelope},
    registry::{Adapter, Agent, Delivered},
    runtime::{InboxRuntime, Runtime},
    store::Store,
};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub struct InboxOnly {
    pub runtime: InboxRuntime,
    pub root: PathBuf,
}

impl InboxOnly {
    fn lookup(&self, exact: Option<&Address>) -> Result<Discovery> {
        match std::fs::symlink_metadata(self.root.join("messages.sqlite")) {
            Ok(_) => {} // Store::open checks ownership, links and permissions.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Discovery::default()),
            Err(e) => return Err(e).context("checking registration journal"),
        }
        Store::open(&self.root)?.registrations(
            self.runtime,
            exact.map(Address::as_str),
            now_millis(),
        )
    }

    pub(super) fn check_target(&self, agent: &Agent, env: &Envelope) -> Result<()> {
        env.validate()?;
        if agent.runtime() != self.runtime.runtime() || agent.address() != &env.to {
            bail!("registered inbox target mismatch");
        }
        Ok(())
    }
}

impl Adapter for InboxOnly {
    fn runtime(&self) -> Runtime {
        self.runtime.runtime()
    }
    fn discover(&self) -> Result<Discovery> {
        self.lookup(None)
    }
    fn find_exact(&self, address: &Address) -> Result<Discovery> {
        if address.runtime()? != self.runtime() {
            bail!("wrong runtime for inbox adapter");
        }
        let mut report = self.lookup(Some(address))?;
        if report.complete && report.agents.is_empty() {
            report.note(self.runtime(), Code::SourceUnavailable, None,
                "no active inbox registration; the receiving thread must run telephone register or MCP register_agent");
        }
        Ok(report)
    }
    fn deliver(&self, agent: &Agent, env: &Envelope) -> Result<Delivered> {
        self.check_target(agent, env)?;
        let mut store = Store::open(&self.root)?;
        store.deposit_registered(&env.to, env, now_millis())?;
        Ok(Delivered::Queued {
            path: store.path,
            note: "No native wake-up occurred. The recipient must call check_inbox or telephone inbox.".into(),
        })
    }
}
