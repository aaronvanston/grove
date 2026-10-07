//! The SQLite store at `$GROVE_HOME/grove.db`, shared by every caller:
//! the registry, stored samples, the last reading of each machine, and
//! settings. WAL and a busy timeout let a scheduled sampler and an
//! interactive command use it at the same time.

mod alerts;
mod live;
mod samples;
mod schema;

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::errors::{AppError, exit};
use crate::output::now_ms;

pub use alerts::{Hook, HookRun, PolicyPatch, PolicyRow, Resolved};
pub use live::LIVE_WINDOW_MS;
pub use samples::Latest;
pub use schema::SCHEMA_VERSION;

pub type Result<T> = std::result::Result<T, AppError>;

/// The four label keys, in the order flags and errors list them.
pub const LABEL_KEYS: [&str; 4] = ["trust", "privacy", "power", "locality"];

/// One value per key in `LABEL_KEYS`; `None` is unset.
pub type Labels = [Option<String>; 4];

/// A label change: `None` leaves a key alone, `Some(None)` clears it.
pub type LabelPatch = [Option<Option<String>>; 4];

/// What config management last reported about the machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigState {
    pub commit: Option<String>,
    pub verify: Option<i64>,
    pub checked_at: Option<i64>,
}

/// Facts about the machine itself, re-read on every sample and kept with
/// no history.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub ip: Option<String>,
    pub model: Option<String>,
    pub chip: Option<String>,
    pub os_version: Option<String>,
    pub product_name: Option<String>,
    pub gpu: Option<String>,
    pub gpu_mem_total_mb: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Machine {
    pub name: String,
    pub endpoint: String,
    pub port: i64,
    pub added_at: i64,
    pub labels: Labels,
    pub config: ConfigState,
    pub facts: Facts,
    /// Where the machine's probe lives, when one is installed.
    pub probe: Option<Probe>,
}

/// A machine's probe and how far grove has read its ring.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub dir: String,
    pub ring_id: Option<i64>,
    pub seq: i64,
    /// The machine's clock minus this one's, in milliseconds.
    pub clock_offset_ms: i64,
}

/// A label value is stored trimmed, and an empty value clears the label,
/// so "unset" has one representation.
pub fn normalize_label(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The user's home folder: HOME when set and not empty, else the account's.
pub fn user_home() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => account_home().unwrap_or_else(|| PathBuf::from("/")),
    }
}

fn account_home() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer into static storage or null; it is
    // read at once and copied.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr((*entry).pw_dir);
        Some(PathBuf::from(
            std::ffi::OsStr::from_encoded_bytes_unchecked(dir.to_bytes()),
        ))
    }
}

/// `$GROVE_HOME`, or `~/.grove`.
pub fn resolve_home() -> PathBuf {
    match std::env::var_os("GROVE_HOME") {
        Some(home) if !home.is_empty() => {
            let home = PathBuf::from(home);
            if home.is_absolute() {
                home
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("/"))
                    .join(home)
            }
        }
        _ => user_home().join(".grove"),
    }
}

fn machine_from_row(row: &Row) -> rusqlite::Result<Machine> {
    Ok(Machine {
        name: row.get("name")?,
        endpoint: row.get("endpoint")?,
        port: row.get("port")?,
        added_at: row.get("added_at")?,
        labels: [
            row.get("label_trust")?,
            row.get("label_privacy")?,
            row.get("label_power")?,
            row.get("label_locality")?,
        ],
        config: ConfigState {
            commit: row.get("config_commit")?,
            verify: row.get("config_verify")?,
            checked_at: row.get("config_checked_at")?,
        },
        facts: Facts {
            ip: row.get("ip")?,
            model: row.get("model")?,
            chip: row.get("chip")?,
            os_version: row.get("os_version")?,
            product_name: row.get("product_name")?,
            gpu: row.get("gpu")?,
            gpu_mem_total_mb: row.get("gpu_mem_total_mb")?,
        },
        probe: row
            .get::<_, Option<String>>("probe_dir")?
            .map(|dir| -> rusqlite::Result<Probe> {
                Ok(Probe {
                    dir,
                    ring_id: row.get("probe_ring_id")?,
                    seq: row.get::<_, Option<i64>>("probe_seq")?.unwrap_or(0),
                    clock_offset_ms: row.get::<_, Option<i64>>("clock_offset_ms")?.unwrap_or(0),
                })
            })
            .transpose()?,
    })
}

pub struct Store {
    pub home: PathBuf,
    db: Connection,
}

impl Store {
    /// Opens (creating when missing) the store in `home` and brings it to
    /// the current schema.
    pub fn open(home: &Path) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(home)
            .map_err(|error| {
                AppError::new(
                    "store_unavailable",
                    format!("Could not create {}: {error}.", home.display()),
                )
                .hint("Set GROVE_HOME to a writable directory.")
                .exit(exit::CONFIG)
            })?;
        let path = home.join("grove.db");
        let db = Connection::open(&path)?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        db.busy_timeout(std::time::Duration::from_millis(5000))?;
        db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        let store = Self {
            home: home.to_path_buf(),
            db,
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version = self.user_version()?;
        if version > SCHEMA_VERSION {
            return Err(AppError::new(
                "store_too_new",
                format!(
                    "{} was written by a newer grove (schema {version}; this build reads {SCHEMA_VERSION}).",
                    self.home.join("grove.db").display()
                ),
            )
            .hint("Upgrade grove.")
            .exit(exit::CONFIG));
        }
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let applied = (|| -> Result<()> {
            // Another process may have migrated while this one waited for
            // the write lock.
            let current = self.user_version()?;
            for step in schema::MIGRATIONS.iter().skip(current.max(0) as usize) {
                step(&self.db)?;
            }
            self.db
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            Ok(())
        })();
        match applied {
            Ok(()) => Ok(self.db.execute_batch("COMMIT")?),
            Err(error) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn user_version(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    pub fn list(&self) -> Result<Vec<Machine>> {
        let mut statement = self.db.prepare("SELECT * FROM machines ORDER BY name")?;
        let rows = statement.query_map([], machine_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, name: &str) -> Result<Option<Machine>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM machines WHERE name = ?",
                [name],
                machine_from_row,
            )
            .optional()?)
    }

    pub fn add(&self, name: &str, endpoint: &str, port: i64, labels: &Labels) -> Result<Machine> {
        let now = now_ms();
        self.db.execute(
            "INSERT INTO machines (
               name, endpoint, port, added_at, updated_at,
               label_trust, label_privacy, label_power, label_locality
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                name, endpoint, port, now, now, labels[0], labels[1], labels[2], labels[3]
            ],
        )?;
        Ok(Machine {
            name: name.to_owned(),
            endpoint: endpoint.to_owned(),
            port,
            added_at: now,
            labels: labels.clone(),
            config: ConfigState::default(),
            facts: Facts::default(),
            probe: None,
        })
    }

    /// Writes only the keys the patch names, so setting one label never
    /// clears another.
    pub fn set_labels(&self, name: &str, patch: &LabelPatch) -> Result<()> {
        for (key, value) in LABEL_KEYS.iter().zip(patch) {
            if let Some(value) = value {
                self.db.execute(
                    &format!("UPDATE machines SET label_{key} = ?, updated_at = ? WHERE name = ?"),
                    params![value, now_ms(), name],
                )?;
            }
        }
        Ok(())
    }

    /// Removes the machine and every row that names it, in one transaction.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let transaction = self.db.transaction()?;
        for table in schema::MACHINE_TABLES {
            transaction.execute(&format!("DELETE FROM {table} WHERE machine = ?"), [name])?;
        }
        let removed = transaction.execute("DELETE FROM machines WHERE name = ?", [name])? > 0;
        transaction.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A store in a fresh folder under the system temp directory, removed
    /// when dropped.
    pub(crate) struct TempStore {
        pub(crate) store: Store,
        pub(crate) home: PathBuf,
    }

    impl TempStore {
        pub(crate) fn new() -> Self {
            let home = Self::folder();
            Self {
                store: Store::open(&home).expect("opens"),
                home,
            }
        }

        /// A fresh folder of its own, for tests that shape a file first.
        pub(crate) fn folder() -> PathBuf {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::temp_dir().canonicalize().expect("temp dir");
            let home = base.join(format!(
                "grove-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            assert!(home.starts_with(&base));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).expect("temp home");
            home
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn a_new_store_is_at_the_current_schema_and_a_newer_one_is_refused() {
        let temp = TempStore::new();
        assert_eq!(temp.store.user_version().unwrap(), SCHEMA_VERSION);
        // Opening again is a no-op.
        drop(Store::open(&temp.home).expect("reopens"));
        temp.store
            .db
            .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();
        let error = Store::open(&temp.home).err().expect("refused");
        assert_eq!(
            (error.code.as_str(), error.exit_code),
            ("store_too_new", 78)
        );
    }

    /// Removing a machine takes every row that names it and leaves other
    /// machines' and fleet-wide rows alone.
    #[test]
    fn removing_a_machine_clears_its_rows_everywhere() {
        let mut temp = TempStore::new();
        let none: Labels = Default::default();
        temp.store
            .add("cam-mbp", "cam-mbp.local", 22, &none)
            .unwrap();
        temp.store.add("cedar-01", "cedar-01", 22, &none).unwrap();
        temp.store
            .db
            .execute_batch(
                "INSERT INTO machine_contact VALUES ('cam-mbp', 1, NULL), ('cedar-01', 1, NULL);
                 INSERT INTO policy VALUES ('cam-mbp', 2, NULL, NULL, NULL, 1), ('', 6, NULL, NULL, 10, 1);
                 INSERT INTO hooks VALUES ('desk', 'true', 'cam-mbp', 'both', 1);",
            )
            .unwrap();
        assert!(temp.store.remove("cam-mbp").unwrap());
        assert!(!temp.store.remove("cam-mbp").unwrap());
        let count =
            |sql: &str| -> i64 { temp.store.db.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM machines"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM machine_contact"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM policy WHERE machine = ''"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM hooks"), 0);
    }
}
