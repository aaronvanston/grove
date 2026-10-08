//! Policy: whether a machine may take new unattended work right now, and
//! why. Anything grove can't establish counts against the machine, because
//! the honest answer to "may I start work here" without evidence is no.

use serde_json::{Value, json};

use crate::errors::AppError;
use crate::reading::{MAX_AHEAD_MS, STALE_AFTER_MS};
use crate::store::Resolved;

/// "HH:MM" to minutes from midnight.
pub fn parse_clock(text: &str) -> Option<i64> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let digits = |part: &str, most: usize| {
        (!part.is_empty() && part.len() <= most && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<i64>().ok())
            .flatten()
    };
    let (hours, minutes) = (
        digits(hours, 2)?,
        digits(minutes, 2).filter(|_| minutes.len() == 2)?,
    );
    (hours <= 23 && minutes <= 59).then_some(hours * 60 + minutes)
}

pub fn format_clock(minute_of_day: i64) -> String {
    format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60)
}

/// "22:00-07:00" to its start and end minutes. A window whose start is
/// after its end crosses midnight; one whose start equals its end is empty
/// and refused.
pub fn parse_quiet_hours(text: &str) -> Result<(i64, i64), AppError> {
    let parts: Vec<&str> = text.trim().split('-').collect();
    let window = match parts.as_slice() {
        [start, end] => parse_clock(start).zip(parse_clock(end)),
        _ => None,
    };
    let Some((start, end)) = window else {
        return Err(AppError::usage(
            "invalid_quiet_hours",
            format!("\"{text}\" is not a valid quiet-hours window."),
        )
        .hint("Use two 24-hour times joined by a dash, such as 22:00-07:00."));
    };
    if start == end {
        return Err(AppError::usage(
            "invalid_quiet_hours",
            "A quiet-hours window with the same start and end is empty.",
        )
        .hint("Give the window a start and an end that differ."));
    }
    Ok((start, end))
}

pub fn format_quiet((start, end): (i64, i64)) -> String {
    format!("{}-{}", format_clock(start), format_clock(end))
}

pub fn in_quiet_window(minute: i64, (start, end): (i64, i64)) -> bool {
    if start < end {
        minute >= start && minute < end
    } else {
        minute >= start || minute < end
    }
}

/// Minutes from local midnight on this machine's clock, where the decision
/// to start work is made.
pub fn local_minute_of_day(at_ms: i64) -> i64 {
    let seconds = at_ms.div_euclid(1000) as libc::time_t;
    // SAFETY: localtime_r writes only into the zeroed struct it is given.
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    let converted = unsafe { libc::localtime_r(&seconds, &mut local) };
    if converted.is_null() {
        return at_ms.div_euclid(60_000).rem_euclid(1440);
    }
    i64::from(local.tm_hour) * 60 + i64::from(local.tm_min)
}

/// "3d 4h", "2h 5m", "7m" or "40s".
pub fn human_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let days = (seconds / 86_400.0).floor();
    let hours = ((seconds % 86_400.0) / 3600.0).floor();
    let minutes = ((seconds % 3600.0) / 60.0).floor();
    if days > 0.0 {
        format!("{days}d {hours}h")
    } else if hours > 0.0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0.0 {
        format!("{minutes}m")
    } else {
        format!("{}s", seconds.round())
    }
}

/// What eligibility is judged from.
pub struct Evidence<'a> {
    pub machine: &'a str,
    pub now: i64,
    pub reachable: bool,
    pub sampled_at: Option<i64>,
    pub sessions: Option<i64>,
    pub policy: &'a Resolved,
    pub firing: &'a [String],
}

fn check(name: &str, ok: bool, reason: Option<&str>, detail: String) -> Value {
    json!({ "check": name, "detail": detail, "ok": ok, "reason": reason })
}

fn cap_name(machine: &str, scope: &str) -> String {
    if scope == "machine" {
        format!("the {machine} cap")
    } else {
        "the fleet cap".into()
    }
}

/// The five checks, in order: reachability, capacity,
/// sessions, quiet hours, alerts.
pub fn checks(evidence: &Evidence) -> Vec<Value> {
    let reachability = if evidence.reachable {
        check(
            "reachability",
            true,
            None,
            "The machine answered over SSH.".into(),
        )
    } else {
        check(
            "reachability",
            false,
            Some("unreachable"),
            "The machine did not answer over SSH.".into(),
        )
    };
    let window = human_duration(STALE_AFTER_MS as f64 / 1000.0);
    let capacity = match evidence.sampled_at {
        None => check(
            "capacity",
            false,
            Some("no_capacity"),
            "No sample has ever been recorded, so capacity is unknown.".into(),
        ),
        Some(at) if at - evidence.now > MAX_AHEAD_MS => check(
            "capacity",
            false,
            Some("stale_capacity"),
            format!(
                "The last sample is dated {} ahead of this clock, so capacity is unknown.",
                human_duration((at - evidence.now) as f64 / 1000.0)
            ),
        ),
        Some(at) if evidence.now - at > STALE_AFTER_MS => check(
            "capacity",
            false,
            Some("stale_capacity"),
            format!(
                "The last sample is {} old, past the {window} freshness window, so capacity is unknown.",
                human_duration((evidence.now - at) as f64 / 1000.0)
            ),
        ),
        Some(at) => check(
            "capacity",
            true,
            None,
            format!(
                "Capacity was sampled {} ago.",
                human_duration((evidence.now - at) as f64 / 1000.0)
            ),
        ),
    };
    let sessions = match (evidence.sessions, evidence.policy.max_sessions) {
        (None, _) => check(
            "sessions",
            false,
            Some("unknown_sessions"),
            "There is no fresh session count, so the cap cannot be checked.".into(),
        ),
        (Some(used), None) => check(
            "sessions",
            true,
            None,
            format!(
                "{used} unattended session{} running and no cap is set.",
                if used == 1 { " is" } else { "s are" }
            ),
        ),
        (Some(used), Some((cap, scope))) if used >= cap => check(
            "sessions",
            false,
            Some("at_capacity"),
            format!(
                "{used} unattended sessions are running, {} {} of {cap}.",
                if used == cap { "at" } else { "past" },
                cap_name(evidence.machine, scope)
            ),
        ),
        (Some(used), Some((cap, scope))) => check(
            "sessions",
            true,
            None,
            format!(
                "{used} of {cap} unattended sessions are running under {}, {} to spare.",
                cap_name(evidence.machine, scope),
                cap - used
            ),
        ),
    };
    let minute = local_minute_of_day(evidence.now);
    let clock = format_clock(minute);
    let quiet = match evidence.policy.quiet {
        None => check(
            "quiet_hours",
            true,
            None,
            format!("The local time is {clock} and no quiet hours are set."),
        ),
        Some((window, scope)) => {
            let span = format!("{} to {}", format_clock(window.0), format_clock(window.1));
            let whose = if scope == "machine" {
                evidence.machine
            } else {
                "the fleet"
            };
            if in_quiet_window(minute, window) {
                check(
                    "quiet_hours",
                    false,
                    Some("quiet_hours"),
                    format!(
                        "The local time is {clock}, inside the quiet hours {span} set for {whose}."
                    ),
                )
            } else {
                check(
                    "quiet_hours",
                    true,
                    None,
                    format!(
                        "The local time is {clock}, outside the quiet hours {span} set for {whose}."
                    ),
                )
            }
        }
    };
    let alerts = if evidence.firing.is_empty() {
        check("alerts", true, None, "No alerts are firing.".into())
    } else {
        check(
            "alerts",
            false,
            Some("alerts_firing"),
            format!(
                "{} {} firing.",
                evidence.firing.join(", "),
                if evidence.firing.len() == 1 {
                    "is"
                } else {
                    "are"
                }
            ),
        )
    };
    vec![reachability, capacity, sessions, quiet, alerts]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 9:05-7:00 is 09:05-07:00, a window crossing
    /// midnight; a bare time or an empty window is refused.
    #[test]
    fn quiet_hours_read_and_cross_midnight() {
        let window = parse_quiet_hours("9:05-7:00").expect("valid");
        assert_eq!(format_quiet(window), "09:05-07:00");
        assert!(
            in_quiet_window(15 * 60 + 6, window),
            "15:06 is inside 09:05 to 07:00"
        );
        assert!(!in_quiet_window(8 * 60, window));
        assert!(in_quiet_window(23 * 60, (22 * 60, 7 * 60)));
        assert!(
            !in_quiet_window(7 * 60, (22 * 60, 7 * 60)),
            "the end is outside"
        );
        assert_eq!(
            parse_quiet_hours("22:00").unwrap_err().message,
            "\"22:00\" is not a valid quiet-hours window."
        );
        assert_eq!(
            parse_quiet_hours("22:00-22:00").unwrap_err().message,
            "A quiet-hours window with the same start and end is empty."
        );
        assert!(parse_quiet_hours("24:00-07:00").is_err());
    }

    /// Unknown fails closed, each check naming its own reason: the explain
    /// for a reachable machine never sampled, inside its quiet hours,
    /// under a fleet cap of 6.
    #[test]
    fn missing_evidence_fails_closed_with_reasons() {
        let policy = Resolved {
            max_sessions: Some((6, "fleet")),
            quiet: Some(((0, 1439), "machine")),
            warn_sessions: Some(10),
        };
        let evidence = Evidence {
            machine: "cam-mbp",
            now: 0,
            reachable: true,
            sampled_at: None,
            sessions: None,
            policy: &policy,
            firing: &[],
        };
        let reasons: Vec<Value> = checks(&evidence)
            .iter()
            .map(|check| check["reason"].clone())
            .collect();
        assert_eq!(
            reasons,
            [
                json!(null),
                json!("no_capacity"),
                json!("unknown_sessions"),
                json!("quiet_hours"),
                json!(null)
            ]
        );
        let fresh = Evidence {
            sampled_at: Some(-60_000),
            sessions: Some(6),
            ..evidence
        };
        let sessions = &checks(&fresh)[2];
        assert_eq!(
            sessions["detail"],
            "6 unattended sessions are running, at the fleet cap of 6."
        );
        assert_eq!(checks(&fresh)[1]["detail"], "Capacity was sampled 1m ago.");
    }
}
