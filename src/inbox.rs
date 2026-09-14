//! The universal fallback transport: a directory per agent.
//!
//! Boring on purpose. A filesystem inbox survives daemon restarts, is
//! debuggable with `cat`, needs no coordination, and works for any runtime
//! whatsoever -- including ones with no inter-session protocol at all, which
//! is the whole point.

use crate::envelope::Envelope;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

pub fn root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".telephone"))
}

/// Addresses contain `:` and sometimes `/`; directory names shouldn't.
fn slug(addr: &str) -> String {
    addr.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

pub fn dir_for(addr: &str) -> Result<PathBuf> {
    Ok(root()?.join("inbox").join(slug(addr)))
}

/// Writes `env` into `addr`'s inbox and returns the file it landed in.
pub fn deposit(addr: &str, env: &Envelope) -> Result<PathBuf> {
    let dir = dir_for(addr)?;
    fs::create_dir_all(&dir).with_context(|| format!("creating inbox {}", dir.display()))?;

    // Sort key first so a plain directory listing is in delivery order.
    let path = dir.join(format!("{}-{}.json", env.sent_at, env.id));
    let body = serde_json::to_string_pretty(env)?;

    // Write to a temp file and rename, so a reader draining concurrently never
    // sees a half-written message.
    let tmp = dir.join(format!(".{}.tmp", env.id));
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[derive(Debug)]
pub struct Pending {
    pub path: PathBuf,
    pub env: Envelope,
}

/// Reads everything waiting for `addr`, oldest first.
pub fn peek(addr: &str) -> Result<Vec<Pending>> {
    let dir = dir_for(addr)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        match serde_json::from_str::<Envelope>(&raw) {
            Ok(env) => out.push(Pending { path, env }),
            // A malformed message shouldn't block every message behind it.
            Err(_) => continue,
        }
    }
    out.sort_by_key(|p| p.env.sent_at);
    Ok(out)
}

/// Reads and removes everything waiting for `addr`.
pub fn drain(addr: &str) -> Result<Vec<Envelope>> {
    let pending = peek(addr)?;
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        fs::remove_file(&p.path).ok();
        out.push(p.env);
    }
    Ok(out)
}
