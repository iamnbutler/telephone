//! At most one reminder per threshold crossing, including concurrent hook calls.
use super::Store;
use anyhow::Result;
use rusqlite::params;

impl Store {
    pub fn claim_context_reminder(&self, session: &str, level: u8) -> Result<bool> {
        if level == 0 {
            self.conn
                .execute("DELETE FROM context_reminders WHERE session=?1", [session])?;
            return Ok(false);
        }
        Ok(self.conn.execute(
            "INSERT INTO context_reminders(session,level) VALUES (?1,?2)
             ON CONFLICT(session) DO UPDATE SET level=excluded.level
             WHERE context_reminders.level < excluded.level",
            params![session, level],
        )? == 1)
    }
}
