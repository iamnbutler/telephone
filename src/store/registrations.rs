//! Explicit same-user routing leases. Neither registration nor polling authenticates an agent.
use super::{insert_inbox, Store};
use crate::{
    address::Address,
    discovery::{Code, Discovery, MAX_AGENTS},
    envelope::Envelope,
    registry::{Agent, Liveness, Status, Transport},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;

pub const RUNTIMES: [&str; 4] = ["opencode", "zed", "delta", "generic"];
pub const LEASE_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_REGISTRATIONS: i64 = 1024;

pub fn runtime(address: &str) -> Option<&'static str> {
    let (prefix, _) = address.split_once(':')?;
    RUNTIMES.into_iter().find(|r| *r == prefix)
}

pub fn registration_address(runtime: Option<&str>, address: Option<&str>) -> Result<Address> {
    match (runtime, address) {
        (Some(prefix), None) if RUNTIMES.contains(&prefix) => {
            format!("{prefix}:{}", uuid::Uuid::new_v4()).parse()
        }
        (None, Some(address)) => {
            let parsed: Address = address.parse()?;
            self::runtime(address).context("use an opencode:, zed:, delta: or generic: address")?;
            Ok(parsed)
        }
        _ => bail!("provide exactly one of runtime (opencode, zed, delta, generic) or address"),
    }
}

#[derive(Debug, Serialize)]
pub struct Registration {
    pub address: Address,
    pub name: String,
    pub last_seen: u64,
    pub expires_at: u64,
}

impl Registration {
    fn validate(&self, now: u64) -> Result<&'static str> {
        // `now` may have been sampled before waiting for SQLite's writer lock.
        // Do not mistake a concurrent renewal for a timestamp from the future.
        let now = now.max(crate::envelope::now_millis());
        let runtime = runtime(self.address.as_str()).context("unsupported inbox runtime")?;
        if self.name.is_empty() || self.name.len() > 256 || self.name.chars().any(char::is_control)
        {
            bail!("registration name must contain 1..=256 bytes without control characters");
        }
        if self.last_seen > now || self.last_seen.checked_add(LEASE_MS) != Some(self.expires_at) {
            bail!("invalid registration timestamps (or local clock moved backwards)");
        }
        if self.expires_at <= now {
            bail!("registration expired; register this thread again");
        }
        Ok(runtime)
    }

    fn agent(&self, runtime: &'static str) -> Agent {
        Agent {
            addr: self.address.to_string(),
            runtime,
            name: self.name.clone(),
            cwd: None,
            status: Status::Unknown,
            liveness: Liveness::Inferred,
            last_seen: self.last_seen,
            transports: vec![Transport::Inbox],
        }
    }
}

// Bound text before copying it out of SQLite, including on a damaged local DB.
const COLUMNS: &str = "CASE WHEN length(CAST(address AS BLOB))<=145 THEN address END,
    CASE WHEN length(CAST(name AS BLOB))<=256 THEN name END, last_seen, expires_at";

fn sql_time(value: u64) -> Result<i64> {
    value
        .try_into()
        .context("registration timestamp exceeds SQLite integer range")
}
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, i64, i64)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}
fn decode(raw: (String, String, i64, i64)) -> Result<Registration> {
    Ok(Registration {
        address: raw.0.parse().context("invalid registered address")?,
        name: raw.1,
        last_seen: raw
            .2
            .try_into()
            .context("negative registration timestamp")?,
        expires_at: raw.3.try_into().context("negative registration expiry")?,
    })
}

impl Store {
    /// Register or explicitly renew a thread. Never called on behalf of a message recipient.
    pub fn register(
        &mut self,
        address: &Address,
        name: Option<&str>,
        now: u64,
    ) -> Result<Registration> {
        let mut registration = Registration {
            address: address.clone(),
            name: name.unwrap_or(address.as_str()).to_owned(),
            last_seen: now,
            expires_at: now
                .checked_add(LEASE_MS)
                .context("registration time overflow")?,
        };
        let runtime = registration.validate(now)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("locking registration journal")?;
        tx.execute(
            "DELETE FROM registrations WHERE expires_at<=?1",
            [sql_time(now)?],
        )
        .context("pruning expired registrations")?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM registrations WHERE address=?1)",
                [address.as_str()],
                |r| r.get(0),
            )
            .context("checking existing registration")?;
        if exists && name.is_none() {
            registration.name = tx.query_row("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name END FROM registrations WHERE address=?1",
                [address.as_str()], |r| r.get(0)).context("preserving registration name")?;
            registration.validate(now)?;
        }
        let count: i64 = tx
            .query_row("SELECT count(*) FROM registrations", [], |r| r.get(0))
            .context("checking registration quota")?;
        if !exists && count >= MAX_REGISTRATIONS {
            bail!("local registration limit reached (1024); unregister unused threads");
        }
        tx.execute("INSERT INTO registrations(address,runtime,name,last_seen,expires_at) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(address) DO UPDATE SET name=excluded.name,last_seen=excluded.last_seen,expires_at=excluded.expires_at",
            params![address.as_str(), runtime, registration.name, sql_time(now)?, sql_time(registration.expires_at)?])
            .context("recording registration")?;
        tx.commit()
            .context("committing registration; it may have been recorded")?;
        Ok(registration)
    }

    pub fn unregister(&self, address: &Address) -> Result<()> {
        runtime(address.as_str()).context("only inbox runtimes can be unregistered")?;
        let changed = self
            .conn
            .execute(
                "DELETE FROM registrations WHERE address=?1",
                [address.as_str()],
            )
            .context("removing registration")?;
        if changed != 1 {
            bail!("address is not registered");
        }
        // Pending messages deliberately survive unregister/expiry. No message deletion here.
        Ok(())
    }

    /// Refresh only an existing, unexpired lease. Sending/polling cannot resurrect an address.
    pub fn registered_identity(
        &mut self,
        address: &Address,
        now: u64,
    ) -> Result<crate::identity::Me> {
        let expected_runtime = runtime(address.as_str()).context("unsupported inbox runtime")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("locking registration renewal")?;
        let now = now.max(crate::envelope::now_millis());
        let raw = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM registrations WHERE address=?1 AND runtime=?2"),
                params![address.as_str(), expected_runtime],
                row,
            )
            .optional()
            .context("reading sender registration")?
            .context(
                "address is not registered; use telephone register or MCP register_agent first",
            )?;
        let registration = decode(raw)?;
        let runtime = registration.validate(now)?;
        let changed = tx.execute("UPDATE registrations SET last_seen=max(last_seen,?2),expires_at=max(expires_at,?3) WHERE address=?1 AND expires_at>?2",
            params![address.as_str(), sql_time(now)?, sql_time(now.checked_add(LEASE_MS).context("registration time overflow")?)?])
            .context("renewing registration")?;
        if changed != 1 {
            bail!("registration disappeared or expired during renewal; register again");
        }
        tx.commit().context("committing registration renewal")?;
        Ok(crate::identity::Me {
            addr: Some(address.to_string()),
            runtime: Some(runtime.into()),
            name: Some(registration.name),
            source: "local registration",
        })
    }

    pub fn registrations(
        &self,
        runtime: &'static str,
        exact: Option<&str>,
        now: u64,
    ) -> Result<Discovery> {
        let mut report = Discovery::default();
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM registrations
            WHERE runtime=?1 AND expires_at>?2 AND (?3 IS NULL OR address=?3)
            ORDER BY expires_at DESC, address LIMIT ?4"
            ))
            .context("preparing registration lookup")?;
        let rows = stmt
            .query_map(
                params![runtime, sql_time(now)?, exact, (MAX_AGENTS + 1) as i64],
                row,
            )
            .context("querying registrations")?;
        for raw in rows {
            let validated = (|| {
                let registration = decode(raw.context("reading registration row")?)?;
                if registration.validate(now)? != runtime {
                    bail!("registration runtime mismatch");
                }
                Ok(registration.agent(runtime))
            })();
            match validated {
                Ok(agent) => {
                    if !report.push(agent) {
                        break;
                    }
                }
                Err(e) => report.warn(
                    runtime,
                    Code::InvalidRecord,
                    Some(&self.path),
                    format!("{e:#}"),
                ),
            }
        }
        Ok(report)
    }

    pub fn deposit_registered(
        &mut self,
        address: &Address,
        env: &Envelope,
        now: u64,
    ) -> Result<()> {
        env.validate()?;
        if &env.to != address {
            bail!("inbox recipient does not match envelope");
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("locking registered inbox deposit")?;
        let expected_runtime = runtime(address.as_str()).context("unsupported inbox runtime")?;
        let raw = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM registrations WHERE address=?1 AND runtime=?2"),
                params![address.as_str(), expected_runtime],
                row,
            )
            .optional()
            .context("checking recipient registration")?
            .context("recipient is no longer registered; nothing queued")?;
        decode(raw)?.validate(now)?;
        // Lease check and deposit share the writer lock, so unregister cannot race this send.
        insert_inbox(&tx, address, env)?;
        tx.commit()
            .context("committing registered inbox deposit; inspect before retrying")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapters::inbox_only,
        envelope::{now_millis, Kind},
        registry::Registry,
    };

    #[test]
    fn leases_expire_and_unregister_blocks_a_previously_discovered_route() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let address: Address = "delta:thread".parse().unwrap();
        let now = now_millis();
        store.register(&address, Some("worker"), now).unwrap();
        let registry = Registry::new(inbox_only::adapters(dir.path()));
        let mut found = registry.discover();
        let agent = registry.resolve(&mut found, address.as_str()).unwrap();
        assert_eq!(agent.liveness, Liveness::Inferred);
        assert_eq!(agent.status, Status::Unknown);
        let env = store
            .prepare(
                Envelope::new("claude:1", address.as_str(), Kind::Request, "hello".into()).unwrap(),
                None,
            )
            .unwrap();
        store.deposit_registered(&address, &env, now).unwrap();
        assert!(store
            .registrations("delta", None, now + LEASE_MS)
            .unwrap()
            .agents
            .is_empty());
        assert!(store.registered_identity(&address, now + LEASE_MS).is_err());
        assert!(store
            .deposit_registered(&address, &env, now + LEASE_MS)
            .is_err());
        store.unregister(&address).unwrap();
        assert!(registry
            .adapter_for(&agent)
            .unwrap()
            .deliver(&agent, &env)
            .is_err());
        // Unregister does not destroy queued messages; explicit re-registration can retrieve them.
        store.register(&address, None, now).unwrap();
        let batch = Store::open(dir.path())
            .unwrap()
            .inbox(&address, false)
            .unwrap();
        assert_eq!(batch.messages.len(), 1);
        batch.acknowledge().unwrap();
    }

    #[test]
    fn polling_renews_only_existing_leases_and_identity_is_not_authentication() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let now = now_millis();
        let address = registration_address(Some("zed"), None).unwrap();
        assert!(store.registered_identity(&address, now).is_err());
        store.register(&address, Some("reviewer"), now).unwrap();
        let me = store
            .registered_identity(&address, now + LEASE_MS - 1)
            .unwrap();
        assert_eq!(me.name.as_deref(), Some("reviewer"));
        assert_eq!(
            store
                .registrations("zed", None, now + LEASE_MS)
                .unwrap()
                .agents
                .len(),
            1
        );
        for value in ["claude:123", "unknown:thread", "delta:../escape"] {
            assert!(registration_address(None, Some(value)).is_err());
        }
        assert!(registration_address(Some("zed"), Some("zed:thread")).is_err());
        assert!(store.register(&address, Some("bad\nname"), now).is_err());
        assert!(store.register(&address, Some(""), now).is_err());
        assert!(store.register(&address, None, u64::MAX).is_err());
    }

    #[test]
    fn quotas_exact_lookup_and_duplicate_names_use_the_real_registry() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let now = now_millis();
        for i in 0..MAX_REGISTRATIONS {
            store
                .register(
                    &format!("opencode:{i:04}").parse().unwrap(),
                    Some("duplicate"),
                    now,
                )
                .unwrap();
        }
        assert!(store
            .register(&"zed:over-quota".parse().unwrap(), None, now)
            .is_err());
        store
            .register(&"opencode:0000".parse().unwrap(), None, now)
            .unwrap();
        let registry = Registry::new(inbox_only::adapters(dir.path()));
        let mut found = registry.discover();
        assert!(!found.complete);
        assert_eq!(found.agents.len(), MAX_AGENTS);
        assert!(registry.resolve(&mut found, "duplicate").is_err());
        assert_eq!(
            registry.resolve(&mut found, "opencode:1023").unwrap().addr,
            "opencode:1023"
        );
        // Expired records are pruned on the next registration without deleting journal messages.
        store
            .register(&"zed:new".parse().unwrap(), None, now + LEASE_MS)
            .unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM registrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        store
            .register(
                &"delta:other".parse().unwrap(),
                Some("same"),
                now + LEASE_MS,
            )
            .unwrap();
        store
            .register(&"zed:new".parse().unwrap(), Some("same"), now + LEASE_MS)
            .unwrap();
        let mut found = store.registrations("delta", None, now + LEASE_MS).unwrap();
        found.merge(store.registrations("zed", None, now + LEASE_MS).unwrap());
        assert!(registry
            .resolve(&mut found, "same")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
    }

    #[test]
    fn malformed_registration_rows_do_not_hide_valid_neighbors() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let now = now_millis();
        store
            .register(&"delta:good".parse().unwrap(), None, now)
            .unwrap();
        for (address, name, seen) in [
            ("delta:bad;", "bad".into(), sql_time(now).unwrap()),
            ("zed:wrong-prefix", "wrong".into(), sql_time(now).unwrap()),
            (
                "delta:huge-name",
                "x".repeat(100_000),
                sql_time(now).unwrap(),
            ),
            ("delta:negative-time", "negative".into(), -1),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO registrations VALUES (?1,'delta',?2,?3,?4)",
                    params![address, name, seen, sql_time(now + LEASE_MS).unwrap()],
                )
                .unwrap();
        }
        let found = store.registrations("delta", None, now).unwrap();
        assert!(!found.complete);
        assert_eq!(found.warnings.len(), 4);
        assert_eq!(found.agents.len(), 1);
        assert_eq!(found.agents[0].addr, "delta:good");
    }
}
