//! From the sample script's `key=value` lines to a sample, and from a
//! sample to the record every command prints. Readings a machine may not
//! have stay null all the way out: a missing sensor is never zero.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::cli::js_number;
use crate::health;
use crate::output::{iso_ms, num, opt_num, round1};
use crate::store::{ConfigState, Facts};

/// How long a reading describes now. Past this it is history, and its
/// counters are too old to take a rate against.
pub const STALE_AFTER_MS: i64 = 600_000;

/// How far ahead of this clock a reading may be dated, once the machine's
/// clock offset is taken out, and still be believed.
pub const MAX_AHEAD_MS: i64 = 60_000;

/// Whether a reading taken at `at` still describes `now`: no older than
/// `STALE_AFTER_MS`, and not dated more than `MAX_AHEAD_MS` ahead, so a
/// reading from the future can't stay fresh forever.
pub fn fresh(now: i64, at: i64) -> bool {
    (-MAX_AHEAD_MS..=STALE_AFTER_MS).contains(&(now - at))
}

/// One reading of one machine, as stored and as printed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub machine: String,
    pub taken_at: i64,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub cores: f64,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub cpu_pct: f64,
    pub mem_total_kb: f64,
    pub mem_available_kb: f64,
    pub disk_total_kb: f64,
    pub disk_used_kb: f64,
    pub net_rx_bytes: f64,
    pub net_tx_bytes: f64,
    pub uptime_s: Option<f64>,
    pub swap_total_kb: Option<f64>,
    pub swap_used_kb: Option<f64>,
    pub cpu_temp_c: Option<f64>,
    pub gpu_temp_c: Option<f64>,
    pub battery_pct: Option<f64>,
    pub battery_state: Option<String>,
    pub agent_sessions: Option<f64>,
    pub claude_sessions: Option<f64>,
    pub codex_sessions: Option<f64>,
    pub gpu_util_pct: Option<f64>,
    pub gpu_mem_used_mb: Option<f64>,
    pub latency_ms: Option<f64>,
    /// What the agent sessions use together: CPU as a percent of one
    /// core, and resident memory.
    pub agent_cpu_pct: Option<f64>,
    pub agent_rss_mb: Option<f64>,
    /// Where pings went and the address they reached; carried by the last
    /// reading, not stored with samples.
    pub ping_target: Option<String>,
    pub address: Option<String>,
}

/// Everything one run of the script reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub sample: Sample,
    pub facts: Facts,
    pub config: ConfigState,
    /// /proc/stat's total and idle jiffies, on Linux.
    pub jiffies: Option<(i64, i64)>,
}

/// Mandatory readings: their key, whether they must be whole, and their
/// least value. A machine that can't produce one has no usable sample.
const REQUIRED: [(&str, bool, f64); 14] = [
    ("cores", true, 1.0),
    ("load1", false, 0.0),
    ("load5", false, 0.0),
    ("load15", false, 0.0),
    ("cpu_pct", false, 0.0),
    ("mem_total_kb", true, 1.0),
    ("mem_available_kb", true, 0.0),
    ("disk_total_kb", true, 0.0),
    ("disk_used_kb", true, 0.0),
    ("net_rx_bytes", true, 0.0),
    ("net_tx_bytes", true, 0.0),
    ("hostname", false, f64::NAN),
    ("os", false, f64::NAN),
    ("arch", false, f64::NAN),
];

/// Reads the script's output. The error names the first reading that was
/// missing or unusable.
pub fn parse(stdout: &str, machine: &str, taken_at: i64) -> Result<Parsed, String> {
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in stdout.lines() {
        if let Some(separator) = line.find('=').filter(|&at| at > 0) {
            fields.insert(&line[..separator], line[separator + 1..].trim());
        }
    }
    let mut numbers: HashMap<&str, f64> = HashMap::new();
    for (key, whole, least) in REQUIRED {
        let Some(&text) = fields.get(key) else {
            return Err(key.to_owned());
        };
        if least.is_nan() {
            if text.is_empty() {
                return Err(key.to_owned());
            }
            continue;
        }
        let value = js_number(text);
        if !value.is_finite() || value < least || (whole && value.fract() != 0.0) {
            return Err(key.to_owned());
        }
        numbers.insert(key, value);
    }
    let text = |key: &str| {
        fields
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned())
    };
    let number = |key: &str| {
        fields
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| js_number(value))
            .filter(|value| value.is_finite())
    };
    // A count is a whole number, never negative; anything else is unknown.
    let count = |key: &str| number(key).filter(|value| *value >= 0.0 && value.fract() == 0.0);
    let required = |key: &str| numbers.get(key).copied().unwrap_or_default();
    let sample = Sample {
        machine: machine.to_owned(),
        taken_at,
        hostname: text("hostname").unwrap_or_default(),
        os: text("os").unwrap_or_default(),
        arch: text("arch").unwrap_or_default(),
        cores: required("cores"),
        load1: required("load1"),
        load5: required("load5"),
        load15: required("load15"),
        cpu_pct: required("cpu_pct"),
        mem_total_kb: required("mem_total_kb"),
        mem_available_kb: required("mem_available_kb"),
        disk_total_kb: required("disk_total_kb"),
        disk_used_kb: required("disk_used_kb"),
        net_rx_bytes: required("net_rx_bytes"),
        net_tx_bytes: required("net_tx_bytes"),
        uptime_s: number("uptime_s"),
        swap_total_kb: number("swap_total_kb"),
        swap_used_kb: number("swap_used_kb"),
        cpu_temp_c: number("cpu_temp_c"),
        gpu_temp_c: number("gpu_temp_c"),
        battery_pct: number("battery_pct"),
        battery_state: text("battery_state").map(|state| state.to_lowercase()),
        agent_sessions: count("agent_sessions"),
        claude_sessions: count("claude_sessions"),
        codex_sessions: count("codex_sessions"),
        gpu_util_pct: number("gpu_util_pct"),
        gpu_mem_used_mb: number("gpu_mem_used_mb"),
        latency_ms: None,
        agent_cpu_pct: None,
        agent_rss_mb: None,
        ping_target: None,
        address: None,
    };
    let jiffies = match (number("cpu_total_jiffies"), number("cpu_idle_jiffies")) {
        (Some(total), Some(idle)) => Some((total as i64, idle as i64)),
        _ => None,
    };
    Ok(Parsed {
        sample,
        facts: Facts {
            ip: text("ip"),
            model: text("model"),
            chip: text("chip"),
            os_version: text("os_version"),
            product_name: text("product_name"),
            gpu: text("gpu_name"),
            gpu_mem_total_mb: number("gpu_mem_total_mb"),
        },
        config: ConfigState {
            commit: text("config_commit"),
            verify: number("config_verify").map(|value| value as i64),
            checked_at: Some(taken_at),
        },
        jiffies,
    })
}

/// The busy share of the CPU between two /proc/stat readings, when the
/// counters moved forward.
pub fn cpu_between(older: (i64, i64), newer: (i64, i64)) -> Option<f64> {
    let (total, idle) = (newer.0 - older.0, newer.1 - older.1);
    if total <= 0 || idle < 0 {
        return None;
    }
    let busy = (total - idle).max(0) as f64;
    Some(round1(busy / total as f64 * 100.0).clamp(0.0, 100.0))
}

/// A rate from two cumulative counters. A reboot resets the counters,
/// which shows as a negative delta and reads as unknown.
pub fn rate(newer: f64, older: f64, seconds: f64) -> Option<f64> {
    let delta = newer - older;
    (delta >= 0.0 && seconds > 0.0).then(|| (delta / seconds).round())
}

impl Sample {
    pub fn mem_used_pct(&self) -> f64 {
        round1((1.0 - self.mem_available_kb / self.mem_total_kb) * 100.0)
    }

    pub fn disk_used_pct(&self) -> f64 {
        if self.disk_total_kb == 0.0 {
            0.0
        } else {
            round1(self.disk_used_kb / self.disk_total_kb * 100.0)
        }
    }

    /// Swap turned off has no percentage: 0 of 0 kB used is unknown, not 0%.
    pub fn swap_used_pct(&self) -> Option<f64> {
        match (self.swap_total_kb, self.swap_used_kb) {
            (Some(total), Some(used)) if total != 0.0 => Some(round1(used / total * 100.0)),
            _ => None,
        }
    }

    pub fn health(&self) -> Value {
        health::record(health::Inputs {
            cpu: Some(self.cpu_pct),
            mem: self.mem_used_pct(),
            swap: self.swap_used_pct(),
            disk: self.disk_used_pct(),
            load_per_core: self.load1 / self.cores.max(1.0),
            cpu_temp: self.cpu_temp_c,
            gpu_temp: self.gpu_temp_c,
        })
    }

    /// The record `sample`, `show` and `history` print, its keys in
    /// alphabetical order.
    pub fn record(&self) -> Value {
        let agents = if self.claude_sessions.is_none() && self.codex_sessions.is_none() {
            Value::Null
        } else {
            json!({
                "claude": opt_num(self.claude_sessions),
                "codex": opt_num(self.codex_sessions),
                "cpu_pct": opt_num(self.agent_cpu_pct),
                "rss_mb": opt_num(self.agent_rss_mb),
            })
        };
        json!({
            "address": self.address,
            "agent_sessions": opt_num(self.agent_sessions),
            "agents": agents,
            "arch": self.arch,
            "battery_pct": opt_num(self.battery_pct),
            "battery_state": self.battery_state,
            "cores": num(self.cores),
            "cpu_pct": num(self.cpu_pct),
            "cpu_temp_c": opt_num(self.cpu_temp_c),
            "disk_total_kb": num(self.disk_total_kb),
            "disk_used_kb": num(self.disk_used_kb),
            "disk_used_pct": num(self.disk_used_pct()),
            "gpu_mem_used_mb": opt_num(self.gpu_mem_used_mb),
            "gpu_temp_c": opt_num(self.gpu_temp_c),
            "gpu_util_pct": opt_num(self.gpu_util_pct),
            "health": self.health(),
            "hostname": self.hostname,
            "latency_ms": opt_num(self.latency_ms),
            "load1": num(self.load1),
            "load15": num(self.load15),
            "load5": num(self.load5),
            "mem_available_kb": num(self.mem_available_kb),
            "mem_total_kb": num(self.mem_total_kb),
            "mem_used_pct": num(self.mem_used_pct()),
            "net_rx_bytes": num(self.net_rx_bytes),
            "net_tx_bytes": num(self.net_tx_bytes),
            "os": self.os,
            "ping_target": self.ping_target,
            "swap_total_kb": opt_num(self.swap_total_kb),
            "swap_used_kb": opt_num(self.swap_used_kb),
            "swap_used_pct": opt_num(self.swap_used_pct()),
            "taken_at": iso_ms(self.taken_at),
            "uptime_s": opt_num(self.uptime_s),
        })
    }
    /// The record with network rates derived from an older sample, as
    /// `history` prints it.
    pub fn record_with_rates(&self, older: Option<&Sample>) -> Value {
        let seconds = older.map(|older| (self.taken_at - older.taken_at) as f64 / 1000.0);
        let rate_of = |counter: fn(&Sample) -> f64| {
            older.and_then(|older| rate(counter(self), counter(older), seconds.unwrap_or(0.0)))
        };
        let mut record = self.record();
        record["net_rx_bps"] = opt_num(rate_of(|sample| sample.net_rx_bytes));
        record["net_tx_bps"] = opt_num(rate_of(|sample| sample.net_tx_bytes));
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Linux machine's output captured from a sample script, with its
    /// names replaced: mem 15.4%, disk 51.9%, swap 24.4%, checked by hand.
    const LINUX: &str = "hostname=cedar-01\nos=Linux\narch=x86_64\ncores=24\nload1=0.07\nload5=0.16\nload15=0.21\ncpu_pct=0.7\nmem_total_kb=65618936\nmem_available_kb=55493984\ndisk_total_kb=982292956\ndisk_used_kb=510181132\nnet_rx_bytes=20474573431\nnet_tx_bytes=22516176468\nip=192.0.2.26\nmodel=MS-7D25\nchip=12th Gen Intel(R) Core(TM) i9-12900K\nos_version=Ubuntu 26.04 LTS\nuptime_s=85305\nswap_total_kb=33554424\nswap_used_kb=8171072\ncpu_temp_c=32.0\ngpu_temp_c=40\nagent_sessions=1\nconfig_commit=0123456789abcdef0123456789abcdef01234567\nconfig_verify=0\n";

    /// A Mac on battery, captured the same way: mem 62%,
    /// disk 75.4%, swap 27.5%, battery 100 and "charged".
    const MAC: &str = "hostname=cam-mbp.local\nos=Darwin\narch=arm64\ncores=10\nload1=1.40\nload5=1.45\nload15=1.42\ncpu_pct=1.4\nmem_total_kb=25165824\nmem_available_kb=9575344\ndisk_total_kb=482797652\ndisk_used_kb=364235916\nnet_rx_bytes=186219604300\nnet_tx_bytes=45005273671\nmodel=Mac16,13\nchip=Apple M4\nos_version=26.5.2\nswap_total_kb=2097152\nswap_used_kb=577280\nbattery_pct=100\nbattery_state=Charged\nagent_sessions=0\n";

    /// A session count that isn't a whole number at or above zero is
    /// unknown, so one machine can't subtract from the fleet's total.
    #[test]
    fn session_counts_are_whole_and_never_negative() {
        let sessions = |value: &str| {
            let text = LINUX.replace("agent_sessions=1", &format!("agent_sessions={value}"));
            parse(&text, "cedar-01", 0)
                .expect("parses")
                .sample
                .agent_sessions
        };
        assert_eq!(sessions("3"), Some(3.0));
        assert_eq!(sessions("0"), Some(0.0));
        for value in ["-1000", "1.5", "Infinity", "NaN", "x"] {
            assert_eq!(sessions(value), None, "{value}");
        }
    }

    /// Fresh is the last ten minutes, and at most a minute ahead.
    #[test]
    fn a_reading_from_the_future_is_not_fresh() {
        let now = 1_791_343_943_100;
        assert!(fresh(now, now));
        assert!(fresh(now, now - 600_000));
        assert!(!fresh(now, now - 600_001));
        assert!(fresh(now, now + 60_000));
        assert!(!fresh(now, now + 60_001));
        assert!(!fresh(now, now + 86_400_000));
    }

    #[test]
    fn readings_parse_and_derive_their_percentages() {
        let linux = parse(LINUX, "cedar-01", 1_791_343_943_100).expect("parses");
        let record = linux.sample.record();
        assert_eq!(
            (
                &record["mem_used_pct"],
                &record["disk_used_pct"],
                &record["swap_used_pct"]
            ),
            (&json!(15.4), &json!(51.9), &json!(24.4))
        );
        assert_eq!(
            (&record["cpu_temp_c"], &record["gpu_temp_c"]),
            (&json!(32), &json!(40))
        );
        assert_eq!(record["load1"], json!(0.07));
        assert_eq!(record["battery_pct"], Value::Null);
        assert_eq!(record["agent_sessions"], json!(1));
        assert_eq!(
            record["agents"],
            Value::Null,
            "this output carries no split"
        );
        assert_eq!(record["taken_at"], "2026-10-07T03:32:23.100Z");
        assert_eq!(linux.facts.model.as_deref(), Some("MS-7D25"));
        assert_eq!(linux.config.verify, Some(0));
        assert_eq!(linux.jiffies, None);

        let mac = parse(MAC, "cam-mbp", 0).expect("parses").sample.record();
        assert_eq!(
            (
                &mac["mem_used_pct"],
                &mac["disk_used_pct"],
                &mac["swap_used_pct"]
            ),
            (&json!(62), &json!(75.4), &json!(27.5))
        );
        assert_eq!(
            (&mac["battery_pct"], &mac["battery_state"]),
            (&json!(100), &json!("charged"))
        );
        assert_eq!(mac["cpu_temp_c"], Value::Null);
    }

    #[test]
    fn a_missing_or_broken_core_reading_names_itself() {
        assert_eq!(
            parse("hostname=x\nos=Linux\n", "x", 0).err().as_deref(),
            Some("cores")
        );
        let broken = LINUX.replace("mem_total_kb=65618936", "mem_total_kb=lots");
        assert_eq!(
            parse(&broken, "x", 0).err().as_deref(),
            Some("mem_total_kb")
        );
        // An optional probe that printed junk is unknown, not a failure.
        let junk = LINUX.replace("cpu_temp_c=32.0", "cpu_temp_c=N/A");
        assert_eq!(
            parse(&junk, "x", 0).expect("parses").sample.cpu_temp_c,
            None
        );
    }

    /// Two /proc/stat readings: 400 jiffies passed, 100 of them idle.
    #[test]
    fn counters_give_cpu_and_rates_between_readings() {
        assert_eq!(cpu_between((1000, 900), (1400, 1000)), Some(75.0));
        assert_eq!(cpu_between((1000, 900), (1000, 900)), None);
        assert_eq!(rate(6000.0, 1000.0, 5.0), Some(1000.0));
        assert_eq!(
            rate(1000.0, 2000.0, 5.0),
            None,
            "a reset counter is unknown"
        );
    }
}
