use super::Store;
use crate::address::Address;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

pub const POLLING_TTL_MS: u64 = 15_000;
#[derive(Debug, Serialize)]
pub struct Receiving {
    pub checked_at: u64,
    pub expires_at: u64,
}

impl Store {
    pub fn record_poll(&self, address: &Address, session_key: &str, now: u64) -> Result<()> {
        let at = i64::try_from(now)?;
        let expires = i64::try_from(now.saturating_add(POLLING_TTL_MS))?;
        self.conn.execute("INSERT INTO receiving(address,session_key,checked_at,expires_at) VALUES (?1,?2,?3,?4)
            ON CONFLICT(address) DO UPDATE SET session_key=excluded.session_key,checked_at=excluded.checked_at,expires_at=excluded.expires_at",
            params![address.as_str(), session_key, at, expires])?;
        Ok(())
    }
    pub fn receiving(
        &self,
        address: &Address,
        session_key: &str,
        now: u64,
    ) -> Result<Option<Receiving>> {
        let row = self
            .conn
            .query_row(
                "SELECT checked_at,expires_at FROM receiving
            WHERE address=?1 AND session_key=?2 AND checked_at<=?3 AND expires_at>?3",
                params![address.as_str(), session_key, i64::try_from(now)?],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        row.map(|(at, expires)| {
            Ok(Receiving {
                checked_at: at.try_into()?,
                expires_at: expires.try_into()?,
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn poll_evidence_expires_and_is_bound_to_the_session_not_just_pid() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let address = "claude:123".parse().unwrap();
        store.record_poll(&address, "session-a", 100).unwrap();
        assert!(store
            .receiving(&address, "session-a", 100)
            .unwrap()
            .is_some());
        assert!(store
            .receiving(&address, "session-a", 99)
            .unwrap()
            .is_none());
        assert!(store
            .receiving(&address, "session-a", 100 + POLLING_TTL_MS)
            .unwrap()
            .is_none());
        assert!(store
            .receiving(&address, "session-b", 101)
            .unwrap()
            .is_none());
        store.record_poll(&address, "session-b", 200).unwrap();
        assert!(store
            .receiving(&address, "session-a", 201)
            .unwrap()
            .is_none());
        assert!(store
            .receiving(&address, "session-b", 201)
            .unwrap()
            .is_some());
    }
}
