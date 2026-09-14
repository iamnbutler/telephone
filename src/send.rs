//! Addressing, loop protection, and handing off to the right adapter.

use crate::envelope::{Envelope, Kind};
use crate::identity;
use anyhow::Result;

/// Resolves `to`, builds an envelope, and delivers it.
///
/// Returns a human-readable account of what happened, because "sent" and
/// "queued in a file nobody will read" are very different outcomes and the
/// caller deserves to know which one it got.
pub fn send(to: &str, body: &str, kind: Kind, reply_to: Option<String>) -> Result<String> {
    let me = identity::whoami();
    let registry = crate::default_registry()?;
    let (agents, warnings) = registry.discover();
    let target = registry.resolve(&agents, to)?;

    let from = me.addr.clone().unwrap_or_else(|| "unknown:local".into());
    if from == target.addr {
        anyhow::bail!("that's you ({from}) -- refusing to send to self");
    }

    let mut env = Envelope::new(from, target.addr.clone(), kind, body.to_string());
    env.from_name = me.name.clone();
    env.reply_to = reply_to;
    // Record ourselves as the first hop so a message that gets relayed onward
    // carries its full path and trips the loop guard before it runs away.
    env.add_hop(&env.from.clone())?;

    let adapter = registry
        .adapter_for(&target)
        .ok_or_else(|| anyhow::anyhow!("no adapter for runtime '{}'", target.runtime))?;

    let outcome = adapter.deliver(&target, &env)?;

    let mut report = match outcome {
        crate::registry::Delivered::Native { via } => format!(
            "Delivered to {} ({}) over {via}. Message id: {}",
            target.name, target.addr, env.id
        ),
        crate::registry::Delivered::Queued { path, note } => format!(
            "Queued for {} ({}). {note}\nMessage id: {}\nInbox file: {}",
            target.name,
            target.addr,
            env.id,
            path.display()
        ),
    };
    for w in warnings {
        report.push_str(&format!("\nwarning: {w}"));
    }
    Ok(report)
}
