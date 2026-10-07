//! Alert rules and how a window of readings turns into a fire, a clear, or
//! nothing. An alert fires only when the condition held across the whole
//! window and clears the same way; anything mixed holds the current state.

use serde_json::{Value, json};

use crate::output::{format_duration, iso_ms, opt_num, round1};
use crate::reading::Sample;

/// The empty machine name scopes a rule, a state, a hook or a policy row to
/// the whole fleet.
pub const FLEET: &str = "";

/// What a rule can watch. The fleet concurrency warning raises
/// agent_sessions events too, but it is set with `policy set`.
pub const RULE_METRICS: [&str; 8] = [
    "cpu", "mem", "disk", "load1", "swap", "cpu_temp", "battery", "down",
];

/// A battery is a problem when it runs low; everything else when it runs
/// high.
pub fn breaches_below(metric: &str) -> bool {
    metric == "battery"
}

/// The reading a rule compares, or None when the machine reported none,
/// which is why a temperature rule can't fire without a sensor.
pub fn metric_value(sample: &Sample, metric: &str) -> Option<f64> {
    match metric {
        "cpu" => Some(sample.cpu_pct),
        "mem" => Some((1.0 - sample.mem_available_kb / sample.mem_total_kb) * 100.0),
        "disk" => Some(if sample.disk_total_kb == 0.0 {
            0.0
        } else {
            sample.disk_used_kb / sample.disk_total_kb * 100.0
        }),
        "load1" => Some(sample.load1),
        "swap" => match (sample.swap_total_kb, sample.swap_used_kb) {
            (Some(total), Some(used)) if total != 0.0 => Some(used / total * 100.0),
            _ => None,
        },
        "cpu_temp" => sample.cpu_temp_c,
        "battery" => sample.battery_pct,
        "agent_sessions" => sample.agent_sessions,
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fire,
    Clear,
    Hold,
}

/// The lookback reaches a little past the window because sampling cadence
/// rarely lines up with it: a reading just older than the window is the
/// evidence its start was observed.
pub const LOOKBACK_FRACTION: f64 = 1.25;
const COVERAGE_FRACTION: f64 = 0.8;

/// Coverage first: at least two readings, the oldest near the window's
/// start, so one spike over a sparse history never fires.
pub fn evaluate_window(
    points: &[(i64, f64)],
    threshold: f64,
    window_ms: i64,
    now: i64,
    below: bool,
) -> Verdict {
    let window = window_ms as f64;
    let in_window: Vec<f64> = points
        .iter()
        .filter(|(at, _)| *at as f64 >= now as f64 - window * LOOKBACK_FRACTION)
        .map(|(_, value)| *value)
        .collect();
    let oldest = points
        .iter()
        .find(|(at, _)| *at as f64 >= now as f64 - window * LOOKBACK_FRACTION)
        .map(|(at, _)| *at);
    let Some(oldest) = oldest.filter(|_| in_window.len() >= 2) else {
        return Verdict::Hold;
    };
    if oldest as f64 > now as f64 - window * COVERAGE_FRACTION {
        return Verdict::Hold;
    }
    let breached = |value: &f64| {
        if below {
            *value < threshold
        } else {
            *value > threshold
        }
    };
    if in_window.iter().all(breached) {
        Verdict::Fire
    } else if in_window.iter().all(|value| !breached(value)) {
        Verdict::Clear
    } else {
        Verdict::Hold
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub metric: String,
    pub machine: String,
    pub threshold: Option<f64>,
    pub window_ms: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub machine: String,
    pub metric: String,
    pub triggered: bool,
    pub since: Option<i64>,
    pub last_value: Option<f64>,
}

/// A fire or a clear.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub machine: String,
    pub metric: String,
    /// `fired` or `cleared`.
    pub kind: &'static str,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub window_ms: i64,
    pub at: i64,
}

impl Event {
    /// `fire` or `clear`, as hooks subscribe to them.
    pub fn event_name(&self) -> &'static str {
        if self.kind == "fired" {
            "fire"
        } else {
            "clear"
        }
    }

    /// What a hook receives: a fleet event names the fleet, because an
    /// empty variable means "no reading" everywhere else.
    pub fn payload(&self) -> Value {
        json!({
            "at": iso_ms(self.at),
            "event": self.event_name(),
            "machine": if self.machine == FLEET { "fleet" } else { &self.machine },
            "metric": self.metric,
            "threshold": opt_num(self.threshold),
            "value": opt_num(self.value.map(round1)),
            "window": format_duration(self.window_ms),
        })
    }
}

/// A null machine is the fleet in every record.
pub fn scope_record(machine: &str) -> Value {
    if machine == FLEET {
        Value::Null
    } else {
        Value::from(machine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;

    /// The window rule at a 10-minute window: the lookback is 12.5
    /// minutes and the oldest reading must be at least 8 minutes old.
    #[test]
    fn a_window_fires_or_clears_only_when_covered_and_unanimous() {
        let now = 100 * MINUTE;
        let at = |minutes_ago: i64, value: f64| (now - minutes_ago * MINUTE, value);
        let window = 10 * MINUTE;
        let verdict = |points: &[(i64, f64)]| evaluate_window(points, 90.0, window, now, false);
        assert_eq!(
            verdict(&[at(10, 95.0), at(5, 96.0), at(0, 97.0)]),
            Verdict::Fire
        );
        assert_eq!(verdict(&[at(10, 50.0), at(0, 40.0)]), Verdict::Clear);
        assert_eq!(
            verdict(&[at(10, 95.0), at(5, 50.0), at(0, 97.0)]),
            Verdict::Hold,
            "mixed"
        );
        assert_eq!(verdict(&[at(0, 99.0)]), Verdict::Hold, "one reading");
        assert_eq!(
            verdict(&[at(7, 99.0), at(0, 99.0)]),
            Verdict::Hold,
            "window not covered"
        );
        // A reading past the lookback is no evidence either way.
        assert_eq!(
            verdict(&[at(13, 10.0), at(9, 95.0), at(0, 95.0)]),
            Verdict::Fire
        );
        // At the threshold is not past it; battery breaches downwards.
        assert_eq!(verdict(&[at(10, 90.0), at(0, 90.0)]), Verdict::Clear);
        let battery = evaluate_window(&[at(10, 15.0), at(0, 12.0)], 20.0, window, now, true);
        assert_eq!(battery, Verdict::Fire);
    }
}
