//! Filesystem-backed local inbox API; transactional storage lives in store.rs.
use crate::{
    address::Address,
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
pub fn deposit(address: &Address, env: &Envelope) -> Result<PathBuf> {
    let mut store = Store::open(&root()?)?;
    store.deposit(address, env)?;
    Ok(store.path)
}
pub fn read(address: &Address, peek: bool) -> Result<InboxBatch> {
    let mut store = Store::open(&root()?)?;
    if address.inbox_runtime().is_some() {
        store.registered_identity(address, crate::envelope::now_millis())?;
    }
    store.inbox(address, peek)
}
