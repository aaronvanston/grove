//! Probe readings in the store: where each machine's probe is, how far
//! its ring has been read, and the last hour of readings at full
//! resolution.

use rusqlite::{OptionalExtension, params};

use super::{Result, Store};

/// How long full-resolution readings are kept.
pub const LIVE_WINDOW_MS: i64 = 3_600_000;

impl Store {
    /// Records (or forgets, with None) where a machine's probe lives. A new
    /// place starts its ring position over.
    pub fn set_probe(&self, machine: &str, dir: Option<&str>) -> Result<()> {
        self.db.execute(
            "UPDATE machines SET probe_dir = ?, probe_ring_id = NULL, probe_seq = 0 WHERE name = ?",
            params![dir, machine],
        )?;
        Ok(())
    }

    pub fn set_probe_position(
        &self,
        machine: &str,
        ring_id: i64,
        seq: i64,
        clock_offset_ms: i64,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE machines SET probe_ring_id = ?, probe_seq = ?, clock_offset_ms = ? WHERE name = ?",
            params![ring_id, seq, clock_offset_ms, machine],
        )?;
        Ok(())
    }

    pub fn insert_live(
        &self,
        machine: &str,
        seq: i64,
        taken_at: i64,
        received_at: i64,
        reading: &[u8],
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO live_readings (machine, seq, taken_at, received_at, reading)
             VALUES (?, ?, ?, ?, ?)",
            params![machine, seq, taken_at, received_at, reading],
        )?;
        Ok(())
    }

    /// Deletes live readings taken or received before `before`: the time
    /// they arrived is this clock's own, whatever the machine dated them.
    pub fn prune_live(&self, before: i64) -> Result<usize> {
        Ok(self.db.execute(
            "DELETE FROM live_readings WHERE taken_at < ?1 OR received_at < ?1",
            [before],
        )?)
    }

    /// The newest live reading: when it was taken (on this clock) and its
    /// bytes.
    pub fn latest_live(&self, machine: &str) -> Result<Option<(i64, Vec<u8>)>> {
        Ok(self
            .db
            .query_row(
                "SELECT taken_at, reading FROM live_readings WHERE machine = ? ORDER BY seq DESC LIMIT 1",
                [machine],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Live readings taken at or after `since`, oldest first.
    pub fn live_since(&self, machine: &str, since: i64) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut statement = self.db.prepare(
            "SELECT taken_at, reading FROM live_readings WHERE machine = ? AND taken_at >= ? ORDER BY seq",
        )?;
        let rows = statement.query_map(params![machine, since], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
