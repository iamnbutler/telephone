//! Validate, journal, then deliver. An uncertain send must not be retried blindly.
use crate::{
    envelope::{Envelope, Kind},
    identity,
    registry::Delivered,
    store::Store,
};
use anyhow::{Context, Result};

pub fn send(to: &str, body: &str, kind: Kind, reply_to: Option<String>) -> Result<String> {
    let me = identity::whoami()?;
    let from = me
        .addr
        .context("cannot identify sender; set TELEPHONE_ADDR explicitly")?;
    let reply_to = reply_to
        .map(|id| uuid::Uuid::parse_str(&id).context("reply_to must be a UUID"))
        .transpose()?;
    if kind == Kind::Reply && reply_to.is_none() {
        anyhow::bail!("a reply requires --reply-to");
    }
    let registry = crate::default_registry()?;
    let mut discovery = registry.discover();
    let target = registry.resolve(&mut discovery, to).with_context(|| {
        format!(
            "recipient lookup: discovery complete={}, warnings={} ({} omitted)",
            discovery.complete,
            serde_json::json!(discovery.warnings),
            discovery.warnings_omitted
        )
    })?;
    let mut draft = Envelope::new(&from, &target.addr, kind, body.to_owned())?;
    draft.from_name = me.name;
    let adapter = registry
        .adapter_for(&target)
        .context("no adapter for target runtime")?;
    let mut store = Store::open(&crate::inbox::root()?)?;
    let env = store.prepare(draft, reply_to)?;
    let outcome = match adapter.deliver(&target, &env) {
        Ok(outcome) => outcome,
        Err(e) => {
            if let Err(journal) = store.outcome(env.id, "failed-or-uncertain") {
                return Err(e).context(format!("delivery failed or is uncertain; journal update also failed: {journal:#}; message {}", env.id));
            }
            return Err(e).context(format!("message {}; inspect before retrying", env.id));
        }
    };
    let (state, description) = match outcome {
        Delivered::Unconfirmed { via } => (
            "unconfirmed",
            format!(
                "Written over {via}; acceptance and reading are unconfirmed. Do not retry blindly."
            ),
        ),
        Delivered::Accepted { via } => (
            "accepted",
            format!("Accepted by {via}; not a read receipt."),
        ),
        Delivered::Queued { path, note } => (
            "queued",
            format!(
                "Queued in Telephone inbox. {note}\nJournal: {}",
                path.display()
            ),
        ),
    };
    let mut report = format!(
        "{description}\nTo: {} ({})\nMessage id: {}",
        serde_json::json!(target.name),
        target.addr,
        env.id
    );
    // Delivery has already happened: a bookkeeping error must not masquerade as
    // a safe-to-retry failure. Preserve the known outcome in the response.
    if let Err(e) = store.outcome(env.id, state) {
        report.push_str(&format!(
            "\nWARNING: outcome journal update failed: {e:#}; do not resend blindly."
        ));
    }
    for warning in discovery.warnings {
        report.push_str(&format!("\nwarning: {}", serde_json::json!(warning)));
    }
    if discovery.warnings_omitted > 0 {
        report.push_str(&format!(
            "\n{} additional discovery warnings omitted.",
            discovery.warnings_omitted
        ));
    }
    Ok(report)
}
