//! Readings in the store: stored samples, each machine's last reading and
//! counters, contact times, facts, config, and how long samples are kept.

use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;

use super::{ConfigState, Facts, Result, Store};
use crate::reading::Sample;

/// Samples are kept this long unless `retention` says otherwise.
pub const DEFAULT_RETENTION_MS: i64 = 90 * 86_400_000;

/// What the last reading of a machine left behind for the next one.
#[derive(Debug, Clone, PartialEq)]
pub struct Latest {
    pub taken_at: i64,
    pub reading: Value,
    pub net_rx_bytes: f64,
    pub net_tx_bytes: f64,
    pub jiffies: Option<(i64, i64)>,
}

pub struct SampleSummary {
    pub count: i64,
    pub first_at: Option<i64>,
    pub last_at: Option<i64>,
}

pub(super) fn sample_from_row(row: &Row) -> rusqlite::Result<Sample> {
    Ok(Sample {
        machine: row.get("machine")?,
        taken_at: row.get("taken_at")?,
        hostname: row.get("hostname")?,
        os: row.get("os")?,
        arch: row.get("arch")?,
        cores: row.get("cores")?,
        load1: row.get("load1")?,
        load5: row.get("load5")?,
        load15: row.get("load15")?,
        cpu_pct: row.get("cpu_pct")?,
        mem_total_kb: row.get("mem_total_kb")?,
        mem_available_kb: row.get("mem_available_kb")?,
        disk_total_kb: row.get("disk_total_kb")?,
        disk_used_kb: row.get("disk_used_kb")?,
        net_rx_bytes: row.get("net_rx_bytes")?,
        net_tx_bytes: row.get("net_tx_bytes")?,
        uptime_s: row.get("uptime_s")?,
        swap_total_kb: row.get("swap_total_kb")?,
        swap_used_kb: row.get("swap_used_kb")?,
        cpu_temp_c: row.get("cpu_temp_c")?,
        gpu_temp_c: row.get("gpu_temp_c")?,
        battery_pct: row.get("battery_pct")?,
        battery_state: row.get("battery_state")?,
        agent_sessions: row.get("agent_sessions")?,
        claude_sessions: row.get("claude_sessions")?,
        codex_sessions: row.get("codex_sessions")?,
        gpu_util_pct: row.get("gpu_util_pct")?,
        gpu_mem_used_mb: row.get("gpu_mem_used_mb")?,
        latency_ms: row.get("latency_ms")?,
        agent_cpu_pct: row.get("agent_cpu_pct")?,
        agent_rss_mb: row.get("agent_rss_mb")?,
        ping_target: None,
        address: None,
    })
}

impl Store {
    /// Runs `write` inside `BEGIN IMMEDIATE`, so two samplers sharing the
    /// store take turns rather than interleaving.
    pub fn write<T>(&self, write: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match write(self) {
            Ok(value) => {
                self.db.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Facts are refreshed on every sample. A probe that came back empty
    /// leaves the last known value, so one failed read doesn't erase the
    /// model of a machine that is still the same machine.
    pub fn update_facts(&self, machine: &str, facts: &Facts, at: i64) -> Result<()> {
        self.db.execute(
            "UPDATE machines SET
               ip = COALESCE(?, ip),
               model = COALESCE(?, model),
               chip = COALESCE(?, chip),
               os_version = COALESCE(?, os_version),
               product_name = COALESCE(?, product_name),
               gpu = COALESCE(?, gpu),
               gpu_mem_total_mb = COALESCE(?, gpu_mem_total_mb),
               updated_at = ?
             WHERE name = ?",
            params![
                facts.ip,
                facts.model,
                facts.chip,
                facts.os_version,
                facts.product_name,
                facts.gpu,
                facts.gpu_mem_total_mb,
                at,
                machine
            ],
        )?;
        Ok(())
    }

    /// Config is overwritten whole: a machine that stopped reporting a
    /// commit no longer applies the one it used to.
    pub fn update_config(&self, machine: &str, config: &ConfigState) -> Result<()> {
        self.db.execute(
            "UPDATE machines SET config_commit = ?, config_verify = ?, config_checked_at = ?
             WHERE name = ?",
            params![config.commit, config.verify, config.checked_at, machine],
        )?;
        Ok(())
    }

    pub fn insert_sample(&self, sample: &Sample) -> Result<()> {
        self.db.execute(
            "INSERT INTO samples (
               machine, taken_at, hostname, os, arch, cores, load1, load5, load15,
               cpu_pct, mem_total_kb, mem_available_kb, disk_total_kb, disk_used_kb,
               net_rx_bytes, net_tx_bytes, uptime_s, swap_total_kb, swap_used_kb,
               cpu_temp_c, gpu_temp_c, battery_pct, battery_state, agent_sessions,
               claude_sessions, codex_sessions, gpu_util_pct, gpu_mem_used_mb, latency_ms,
               agent_cpu_pct, agent_rss_mb
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                sample.machine,
                sample.taken_at,
                sample.hostname,
                sample.os,
                sample.arch,
                sample.cores,
                sample.load1,
                sample.load5,
                sample.load15,
                sample.cpu_pct,
                sample.mem_total_kb,
                sample.mem_available_kb,
                sample.disk_total_kb,
                sample.disk_used_kb,
                sample.net_rx_bytes,
                sample.net_tx_bytes,
                sample.uptime_s,
                sample.swap_total_kb,
                sample.swap_used_kb,
                sample.cpu_temp_c,
                sample.gpu_temp_c,
                sample.battery_pct,
                sample.battery_state,
                sample.agent_sessions,
                sample.claude_sessions,
                sample.codex_sessions,
                sample.gpu_util_pct,
                sample.gpu_mem_used_mb,
                sample.latency_ms,
                sample.agent_cpu_pct,
                sample.agent_rss_mb
            ],
        )?;
        Ok(())
    }

    pub fn latest(&self, machine: &str) -> Result<Option<Latest>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM machine_latest WHERE machine = ?",
                [machine],
                |row| {
                    let total: Option<i64> = row.get("cpu_total_jiffies")?;
                    let idle: Option<i64> = row.get("cpu_idle_jiffies")?;
                    let reading: String = row.get("reading")?;
                    Ok(Latest {
                        taken_at: row.get("taken_at")?,
                        reading: serde_json::from_str(&reading).unwrap_or(Value::Null),
                        net_rx_bytes: row.get("net_rx_bytes")?,
                        net_tx_bytes: row.get("net_tx_bytes")?,
                        jiffies: total.zip(idle),
                    })
                },
            )
            .optional()?)
    }

    pub fn set_latest(&self, machine: &str, latest: &Latest) -> Result<()> {
        let (total, idle) = latest.jiffies.unzip();
        self.db.execute(
            "INSERT INTO machine_latest
               (machine, taken_at, reading, net_rx_bytes, net_tx_bytes, cpu_total_jiffies, cpu_idle_jiffies)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (machine) DO UPDATE SET
               taken_at = excluded.taken_at,
               reading = excluded.reading,
               net_rx_bytes = excluded.net_rx_bytes,
               net_tx_bytes = excluded.net_tx_bytes,
               cpu_total_jiffies = excluded.cpu_total_jiffies,
               cpu_idle_jiffies = excluded.cpu_idle_jiffies",
            params![
                machine,
                latest.taken_at,
                latest.reading.to_string(),
                latest.net_rx_bytes,
                latest.net_tx_bytes,
                total,
                idle
            ],
        )?;
        Ok(())
    }

    /// Newest first, at most `limit`.
    pub fn list_samples(&self, machine: &str, limit: i64) -> Result<Vec<Sample>> {
        let mut statement = self
            .db
            .prepare("SELECT * FROM samples WHERE machine = ? ORDER BY taken_at DESC LIMIT ?")?;
        let rows = statement.query_map(params![machine, limit], sample_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn sample_summary(&self, machine: &str) -> Result<SampleSummary> {
        Ok(self.db.query_row(
            "SELECT COUNT(*), MIN(taken_at), MAX(taken_at) FROM samples WHERE machine = ?",
            [machine],
            |row| {
                Ok(SampleSummary {
                    count: row.get(0)?,
                    first_at: row.get(1)?,
                    last_at: row.get(2)?,
                })
            },
        )?)
    }

    pub fn total_samples(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))?)
    }

    /// When the newest stored sample was taken.
    pub fn last_sample_at(&self, machine: &str) -> Result<Option<i64>> {
        Ok(self.db.query_row(
            "SELECT MAX(taken_at) FROM samples WHERE machine = ?",
            [machine],
            |row| row.get(0),
        )?)
    }

    /// Deletes samples taken before `before`, for one machine or all.
    pub fn prune(&self, machine: Option<&str>, before: i64) -> Result<usize> {
        Ok(match machine {
            Some(machine) => self.db.execute(
                "DELETE FROM samples WHERE machine = ? AND taken_at < ?",
                params![machine, before],
            )?,
            None => self
                .db
                .execute("DELETE FROM samples WHERE taken_at < ?", [before])?,
        })
    }

    /// How long samples are kept; None keeps them forever.
    pub fn retention(&self) -> Result<Option<i64>> {
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM settings WHERE key = 'retention_ms'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match value.as_deref() {
            None => Some(DEFAULT_RETENTION_MS),
            Some("off") => None,
            Some(text) => Some(text.parse().unwrap_or(DEFAULT_RETENTION_MS)),
        })
    }
    pub fn set_retention(&self, retention: Option<i64>) -> Result<()> {
        let value = retention.map_or_else(|| "off".to_owned(), |ms| ms.to_string());
        self.db.execute(
            "INSERT INTO settings (key, value) VALUES ('retention_ms', ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [value],
        )?;
        Ok(())
    }
}
