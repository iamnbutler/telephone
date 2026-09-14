//! Filesystem-backed local inbox API; transactional storage lives in store.rs.
use crate::{
    envelope::Envelope,
    store::{InboxBatch, Store},
};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn root() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home directory")?
        .join(".telephone"))
}
pub fn deposit(addr: &str, env: &Envelope) -> Result<PathBuf> {
    let address = addr.parse()?;
    let mut store = Store::open(&root()?)?;
    store.deposit(&address, env)?;
    Ok(store.path)
}
pub fn read(addr: &str, peek: bool) -> Result<InboxBatch> {
    Store::open(&root()?)?.inbox(&addr.parse()?, peek)
}
