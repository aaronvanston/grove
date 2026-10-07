//! Running the user's commands on alert transitions. A hook runs through
//! `sh -c` with the event in GROVE_* variables and as one JSON object on
//! stdin, after the readings and transitions that caused it have been
//! committed, so a slow or broken command can't lose a reading or change
//! an exit code. Every run is recorded, successful or not.

use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::alerts::{Event, FLEET};
use crate::output::{now_ms, num};
use crate::store::{Hook, HookRun, Store};
use crate::transport;

/// Long enough for a curl to a slow endpoint, short enough that a wedged
/// command can't outlive the sampling interval that started it.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Enough of stderr to see why a command failed, bounded so a chatty
/// script can't grow the store.
const STDERR_LIMIT: usize = 500;

/// How a hook run ended.
pub struct Outcome {
    pub exit_code: i64,
    pub stderr: String,
}

/// Runs one hook with `payload` (an event's payload) and returns how it
/// ended. A run that outlives the timeout is killed and exits -1.
pub fn run(hook: &Hook, payload: &Value) -> Outcome {
    let text = |key: &str| match &payload[key] {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    };
    let mut command = Command::new("sh");
    command.arg("-c").arg(&hook.command);
    for (name, key) in [
        ("GROVE_AT", "at"),
        ("GROVE_EVENT", "event"),
        ("GROVE_MACHINE", "machine"),
        ("GROVE_METRIC", "metric"),
        ("GROVE_THRESHOLD", "threshold"),
        ("GROVE_VALUE", "value"),
        ("GROVE_WINDOW", "window"),
    ] {
        command.env(name, text(key));
    }
    let stdin = format!("{payload}\n");
    let ran = transport::run(&mut command, "sh", Some(stdin.as_bytes()), TIMEOUT);
    if ran.timed_out.is_some() {
        return Outcome {
            exit_code: -1,
            stderr: format!("timed out after {}s", TIMEOUT.as_secs()),
        };
    }
    Outcome {
        exit_code: ran.code.map_or(-1, i64::from),
        stderr: ran.stderr.trim().chars().take(STDERR_LIMIT).collect(),
    }
}

/// Runs every hook the events call for, all at once, and records each run.
pub fn run_for_events(store: &Store, events: &[Event]) -> crate::store::Result<()> {
    let mut targets = Vec::new();
    for event in events {
        for hook in store.hooks_for(&event.machine, event.event_name())? {
            targets.push((event, hook));
        }
    }
    let outcomes = transport::each(&targets, targets.len(), |(event, hook)| {
        run(hook, &event.payload())
    });
    store.write(|store| {
        for ((event, hook), outcome) in targets.iter().zip(outcomes) {
            store.record_hook_run(&HookRun {
                hook: hook.name.clone(),
                machine: event.machine.clone(),
                metric: event.metric.clone(),
                event: event.event_name().into(),
                exit_code: outcome.exit_code,
                stderr: outcome.stderr,
                at: now_ms(),
            })?;
        }
        Ok(())
    })
}

/// The synthetic event `hooks test` runs a hook against.
pub fn test_payload(hook: &Hook, event: &str) -> Value {
    serde_json::json!({
        "at": crate::output::iso_ms(now_ms()),
        "event": event,
        "machine": if hook.machine == FLEET { "test-machine" } else { &hook.machine },
        "metric": "cpu",
        "threshold": 90,
        "value": num(95.5),
        "window": "10m",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic fire: the event in the
    /// variables and the same event as JSON on stdin, stderr kept.
    #[test]
    fn a_hook_gets_the_event_in_variables_and_on_stdin() {
        let hook = Hook {
            name: "h1".into(),
            command: r#"printf "%s %s %s " "$GROVE_EVENT" "$GROVE_VALUE" "$GROVE_MACHINE" >&2; cat >&2; exit 3"#.into(),
            machine: FLEET.into(),
            on: "fire".into(),
            created_at: 0,
        };
        let payload = serde_json::json!({
            "at": "2026-10-07T04:06:17.666Z",
            "event": "fire",
            "machine": "test-machine",
            "metric": "cpu",
            "threshold": 90,
            "value": 95.5,
            "window": "10m",
        });
        let outcome = run(&hook, &payload);
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(
            outcome.stderr,
            r#"fire 95.5 test-machine {"at":"2026-10-07T04:06:17.666Z","event":"fire","machine":"test-machine","metric":"cpu","threshold":90,"value":95.5,"window":"10m"}"#
        );
    }
}
