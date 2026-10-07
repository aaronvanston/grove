//! Table definitions and the ordered migrations that bring a database to
//! the current `user_version`. Steps are append-only once released: a
//! change to the shape is a new step at the end, never an edit.

use rusqlite::Connection;

/// Step 1: machines, samples, alerts, hooks, policy, each machine's last
/// reading whether or not it was stored, its probe's live readings, and
/// settings.
fn create_tables(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "
        CREATE TABLE machines (
          name TEXT PRIMARY KEY,
          endpoint TEXT NOT NULL,
          port INTEGER NOT NULL,
          added_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL,
          ip TEXT,
          model TEXT,
          chip TEXT,
          os_version TEXT,
          label_trust TEXT,
          label_privacy TEXT,
          label_power TEXT,
          label_locality TEXT,
          config_commit TEXT,
          config_verify INTEGER,
          config_checked_at INTEGER,
          product_name TEXT,
          gpu TEXT,
          gpu_mem_total_mb INTEGER,
          probe_dir TEXT,
          probe_ring_id INTEGER,
          probe_seq INTEGER,
          clock_offset_ms INTEGER
        );
        CREATE TABLE samples (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          machine TEXT NOT NULL,
          taken_at INTEGER NOT NULL,
          hostname TEXT NOT NULL,
          os TEXT NOT NULL,
          arch TEXT NOT NULL,
          cores INTEGER NOT NULL,
          load1 REAL NOT NULL,
          load5 REAL NOT NULL,
          load15 REAL NOT NULL,
          cpu_pct REAL NOT NULL,
          mem_total_kb INTEGER NOT NULL,
          mem_available_kb INTEGER NOT NULL,
          disk_total_kb INTEGER NOT NULL,
          disk_used_kb INTEGER NOT NULL,
          net_rx_bytes INTEGER NOT NULL,
          net_tx_bytes INTEGER NOT NULL,
          uptime_s INTEGER,
          swap_total_kb INTEGER,
          swap_used_kb INTEGER,
          cpu_temp_c REAL,
          gpu_temp_c REAL,
          battery_pct INTEGER,
          battery_state TEXT,
          agent_sessions INTEGER,
          claude_sessions INTEGER,
          codex_sessions INTEGER,
          gpu_util_pct REAL,
          gpu_mem_used_mb INTEGER,
          latency_ms REAL,
          agent_cpu_pct REAL,
          agent_rss_mb INTEGER
        );
        CREATE INDEX samples_by_machine ON samples (machine, taken_at DESC);
        CREATE TABLE alert_rules (
          metric TEXT NOT NULL,
          machine TEXT NOT NULL DEFAULT '',
          threshold REAL,
          window_ms INTEGER NOT NULL,
          created_at INTEGER NOT NULL,
          PRIMARY KEY (metric, machine)
        );
        CREATE TABLE alert_states (
          machine TEXT NOT NULL,
          metric TEXT NOT NULL,
          triggered INTEGER NOT NULL,
          since INTEGER,
          last_value REAL,
          updated_at INTEGER NOT NULL,
          PRIMARY KEY (machine, metric)
        );
        CREATE TABLE alert_events (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          machine TEXT NOT NULL,
          metric TEXT NOT NULL,
          kind TEXT NOT NULL,
          value REAL,
          threshold REAL,
          window_ms INTEGER NOT NULL,
          at INTEGER NOT NULL
        );
        CREATE TABLE machine_contact (
          machine TEXT PRIMARY KEY,
          last_ok_at INTEGER,
          down_since INTEGER
        );
        CREATE TABLE hooks (
          name TEXT PRIMARY KEY,
          command TEXT NOT NULL,
          machine TEXT NOT NULL DEFAULT '',
          on_trigger TEXT NOT NULL DEFAULT 'both',
          created_at INTEGER NOT NULL
        );
        CREATE TABLE hook_runs (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          hook TEXT NOT NULL,
          machine TEXT NOT NULL,
          metric TEXT NOT NULL,
          event TEXT NOT NULL,
          exit_code INTEGER NOT NULL,
          stderr TEXT NOT NULL,
          at INTEGER NOT NULL
        );
        CREATE TABLE policy (
          machine TEXT PRIMARY KEY,
          max_sessions INTEGER,
          quiet_start_min INTEGER,
          quiet_end_min INTEGER,
          warn_sessions INTEGER,
          updated_at INTEGER NOT NULL
        );
        CREATE TABLE machine_latest (
          machine TEXT PRIMARY KEY,
          taken_at INTEGER NOT NULL,
          reading TEXT NOT NULL,
          net_rx_bytes INTEGER NOT NULL,
          net_tx_bytes INTEGER NOT NULL,
          cpu_total_jiffies INTEGER,
          cpu_idle_jiffies INTEGER
        );
        CREATE TABLE live_readings (
          machine TEXT NOT NULL,
          seq INTEGER NOT NULL,
          taken_at INTEGER NOT NULL,
          received_at INTEGER NOT NULL,
          reading BLOB NOT NULL,
          PRIMARY KEY (machine, seq)
        );
        CREATE INDEX live_readings_by_time ON live_readings (machine, taken_at);
        CREATE TABLE settings (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        ",
    )
}

/// The steps, in order; the schema version is how many there are.
pub const MIGRATIONS: [fn(&Connection) -> rusqlite::Result<()>; 1] = [create_tables];

pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// The tables that hold a machine's rows in their `machine` column, which
/// `rm` clears before the machine itself.
pub const MACHINE_TABLES: [&str; 10] = [
    "samples",
    "alert_rules",
    "alert_states",
    "alert_events",
    "machine_contact",
    "hooks",
    "hook_runs",
    "policy",
    "machine_latest",
    "live_readings",
];
