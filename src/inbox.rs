//! Filesystem-backed local inbox API; transactional storage lives in store.rs.
use crate::{
    address::Address,
    envelope::Envelope,
    store::{InboxBatch, Store},
};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("TELEPHONE_STATE_DIR") {
        let path = PathBuf::from(path);
        anyhow::ensure!(path.is_absolute(), "TELEPHONE_STATE_DIR must be absolute");
        return Ok(path);
    }
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
    address.runtime()?;
    crate::receiving::advertise(&root()?, address)?;
    Store::open(&root()?)?.inbox(address, peek)
}
