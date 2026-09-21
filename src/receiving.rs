//! Fresh receiver observations guide future sends; never replay accepted work.
use crate::{address::Address, registry::Adapter, runtime::Runtime, store::Store};
use anyhow::Result;
use std::path::Path;

pub fn session_key(address: &Address) -> Result<Option<String>> {
    match address.runtime()? {
        Runtime::Codex => Ok(Some(address.to_string())),
        Runtime::Claude => {
            let report = crate::adapters::claude_code::ClaudeCode::new()?.find_exact(address)?;
            Ok(report
                .agents
                .into_iter()
                .find(|a| a.address() == address)
                .and_then(|a| a.session_key))
        }
    }
}

pub fn advertise(root: &Path, address: &Address) -> Result<()> {
    if let Some(key) = session_key(address)? {
        Store::open(root)?.record_poll(address, &key, crate::envelope::now_millis())?;
    }
    Ok(())
}
