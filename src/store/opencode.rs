use super::Store;
use crate::{adapters::opencode::Route, address::Address, envelope::now_millis};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

impl Store {
    pub fn bind_opencode(&mut self, address: &Address, route: &Route) -> Result<()> {
        crate::adapters::opencode::address(address)?;
        let encoded = serde_json::to_string(route).context("encoding OpenCode binding")?;
        if encoded.len() > 16384 {
            bail!("OpenCode binding exceeds size limit");
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("locking OpenCode binding")?;
        let now: i64 = now_millis()
            .try_into()
            .context("OpenCode binding timestamp exceeds SQLite range")?;
        let active: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM registrations WHERE address=?1 AND expires_at>?2)",
                params![address.as_str(), now],
                |r| r.get(0),
            )
            .context("checking OpenCode registration")?;
        if !active {
            bail!("register this OpenCode address before binding its native session");
        }
        tx.execute("INSERT INTO opencode_routes(address,route) VALUES (?1,?2) ON CONFLICT(address) DO UPDATE SET route=excluded.route",
            params![address.as_str(), encoded]).context("writing OpenCode binding")?;
        tx.commit()
            .context("committing OpenCode binding; it may have been recorded")
    }

    pub fn opencode_route(&self, address: &Address) -> Result<Option<Route>> {
        let now: i64 = now_millis()
            .try_into()
            .context("OpenCode binding timestamp exceeds SQLite range")?;
        let raw: Option<Option<String>> = self.conn.query_row(
            "SELECT CASE WHEN length(CAST(route AS BLOB))<=16384 THEN route END FROM opencode_routes JOIN registrations USING(address) WHERE address=?1 AND expires_at>?2",
            params![address.as_str(), now], |r| r.get(0))
            .optional().context("reading OpenCode binding")?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let raw = raw.context("OpenCode binding exceeds size limit")?;
        let route: Route = serde_json::from_str(&raw)
            .map_err(|_| anyhow::anyhow!("invalid OpenCode binding JSON"))?;
        Ok(Some(route))
    }

    pub fn unbind_opencode(&self, address: &Address) -> Result<()> {
        crate::adapters::opencode::address(address)?;
        if self
            .conn
            .execute(
                "DELETE FROM opencode_routes WHERE address=?1",
                [address.as_str()],
            )
            .context("removing OpenCode binding")?
            != 1
        {
            bail!("no OpenCode binding exists for this address");
        }
        Ok(())
    }
}
