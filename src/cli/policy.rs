//! The policy commands and drift: what the fleet is allowed to do, whether
//! a machine may take work now, and whether its config matches the source.

use std::time::Duration;

use serde_json::{Value, json};

use super::machines::{config_cell, open_store, require};
use super::options::{bounded, string};
use super::{Context, Done, NAME};
use crate::alerts::{FLEET, scope_record};
use crate::errors::{AppError, exit};
use crate::output::{iso_ms, now_ms, opt_iso};
use crate::policy::{
    Evidence, checks, format_clock, format_quiet, in_quiet_window, local_minute_of_day,
    parse_quiet_hours,
};
use crate::reading::STALE_AFTER_MS;
use crate::store::{PolicyPatch, PolicyRow, Store};
use crate::{drift, transport};

fn policy_record(row: &PolicyRow) -> Value {
    json!({
        "machine": scope_record(&row.machine),
        "max_sessions": row.max_sessions,
        "quiet_hours": row.quiet.map(format_quiet),
        "updated_at": iso_ms(row.updated_at),
        "warn_sessions": row.warn_sessions,
    })
}

/// A session count option, `z.coerce.number().int().min(0).max(1000)`.
fn count(context: &Context, name: &str) -> Result<Option<i64>, AppError> {
    if string(&context.options, name).is_none() {
        return Ok(None);
    }
    bounded(&context.options, name, 0, 1000).map(Some)
}

pub fn set(context: &Context) -> Result<Done, AppError> {
    let max_sessions = count(context, "maxSessions")?;
    let warn_sessions = count(context, "warnSessions")?;
    let quiet = string(&context.options, "quietHours");
    let machine = context.argument(0).filter(|name| !name.is_empty());
    if max_sessions.is_none() && warn_sessions.is_none() && quiet.is_none() {
        return Err(
            AppError::usage("no_policy_given", "Setting policy needs a setting.")
                .hint("Pass at least one of --max-sessions, --quiet-hours, or --warn-sessions."),
        );
    }
    if let (Some(warn), Some(_)) = (warn_sessions, &machine) {
        return Err(AppError::usage(
            "invalid_policy_scope",
            "The concurrency warning counts the whole fleet and has no per-machine form.",
        )
        .hint(format!(
            "Set it fleet-wide with '{NAME} policy set --warn-sessions {warn}'."
        )));
    }
    let store = open_store()?;
    if let Some(machine) = &machine {
        require(&store, machine)?;
    }
    let patch = PolicyPatch {
        max_sessions,
        quiet: quiet.map(|text| parse_quiet_hours(&text)).transpose()?,
        warn_sessions,
    };
    let row = store.set_policy(machine.as_deref().unwrap_or(FLEET), &patch)?;
    let parts: Vec<String> = [
        row.max_sessions.map(|cap| format!("cap {cap}")),
        row.quiet
            .map(|quiet| format!("quiet {}", format_quiet(quiet))),
        row.warn_sessions.map(|warn| format!("warn at {warn}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let ui = &context.ui;
    let human = format!(
        "{} Policy for {}: {}",
        ui.success(ui.symbols.success),
        ui.command(machine.as_deref().unwrap_or("the fleet")),
        if parts.is_empty() {
            "nothing set".to_owned()
        } else {
            parts.join(", ")
        }
    );
    Ok(Done::new(policy_record(&row), human))
}

pub fn rm(context: &Context) -> Result<Done, AppError> {
    let machine = context.argument(0).unwrap_or_default();
    if !open_store()?.remove_policy(&machine)? {
        let scope = if machine == FLEET {
            "the fleet".to_owned()
        } else {
            format!("\"{machine}\"")
        };
        return Err(
            AppError::new("policy_not_found", format!("No policy is set for {scope}."))
                .hint(format!("See what is configured with '{NAME} policy show'.")),
        );
    }
    let ui = &context.ui;
    let human = format!(
        "{} Removed the policy for {}",
        ui.success(ui.symbols.success),
        ui.command(if machine == FLEET {
            "the fleet"
        } else {
            &machine
        })
    );
    Ok(Done::new(
        json!({ "machine": scope_record(&machine), "removed": true }),
        human,
    ))
}

pub fn show(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let fleet = store.fleet_sessions(now_ms())?;
    let rows = store.list_policies()?;
    let ui = &context.ui;
    let human = if rows.is_empty() {
        format!(
            "{}\nSet one with {}.",
            ui.muted("No policy configured."),
            ui.command(&format!("{NAME} policy set --max-sessions 4"))
        )
    } else {
        let dash = |value: Option<String>| value.unwrap_or_else(|| "-".into());
        let table: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                vec![
                    if row.machine == FLEET {
                        "fleet".into()
                    } else {
                        row.machine.clone()
                    },
                    dash(row.max_sessions.map(|cap| cap.to_string())),
                    dash(row.quiet.map(format_quiet)),
                    dash(row.warn_sessions.map(|warn| warn.to_string())),
                ]
            })
            .collect();
        let summary = match fleet.total {
            None => ui.muted("no fresh session counts"),
            Some(total) => ui.muted(&format!(
                "{total} session{} running across {} machine{}{}",
                if total == 1 { "" } else { "s" },
                fleet.known,
                if fleet.known == 1 { "" } else { "s" },
                if fleet.unknown == 0 {
                    String::new()
                } else {
                    format!(", {} unknown", fleet.unknown)
                }
            )),
        };
        format!(
            "{}\n\n{summary}",
            ui.table(&["Scope", "Cap", "Quiet hours", "Warn at"], &table)
        )
    };
    Ok(Done::new(
        json!({
            "fleet_sessions": { "known": fleet.known, "total": fleet.total, "unknown": fleet.unknown },
            "policies": rows.iter().map(policy_record).collect::<Vec<_>>(),
        }),
        human,
    ))
}

/// The newest reading of the machine, stored or not.
fn sampled_at(store: &Store, machine: &str) -> Result<Option<i64>, AppError> {
    let latest = store.latest(machine)?.map(|latest| latest.taken_at);
    let stored = store.last_sample_at(machine)?;
    Ok(latest.max(stored))
}

pub fn explain(context: &Context) -> Result<Done, AppError> {
    let timeout = bounded(&context.options, "timeout", 1, 60_000)?;
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    let machine = require(&store, &name)?;
    let now = now_ms();
    let probe = transport::run_script(
        &machine,
        &store.home,
        "true\n",
        Duration::from_millis(timeout as u64),
    );
    let policy = store.resolve_policy(&name)?;
    // A state counts only while a rule still applies to it, as in
    // `alerts state`.
    let mut firing = Vec::new();
    for state in store.list_states()? {
        if state.machine == name
            && state.triggered
            && store.rule_for(&state.metric, &name)?.is_some()
        {
            firing.push(state.metric);
        }
    }
    let fleet = store.fleet_sessions(now)?;
    let sessions = store.agent_sessions_for(&name, now)?;
    let sampled = sampled_at(&store, &name)?;
    let checks = checks(&Evidence {
        machine: &name,
        now,
        reachable: probe.ok(),
        sampled_at: sampled,
        sessions,
        policy: &policy,
        firing: &firing,
    });
    let eligible = checks.iter().all(|check| check["ok"] == true);
    let age = sampled.map(|at| now - at);
    let minute = local_minute_of_day(now);
    let data = json!({
        "alerts": { "firing": firing },
        "capacity": {
            "age_s": age.map(|age| (age as f64 / 1000.0).round() as i64),
            "sampled_at": opt_iso(sampled),
            "stale": age.is_none_or(|age| age > STALE_AFTER_MS),
        },
        "checked_at": iso_ms(now),
        "checks": checks,
        "eligible": eligible,
        "machine": name,
        "quiet_hours": {
            "active": policy.quiet.is_some_and(|(window, _)| in_quiet_window(minute, window)),
            "local_time": format_clock(minute),
            "scope": policy.quiet.map(|(_, scope)| scope),
            "window": policy.quiet.map(|(window, _)| format_quiet(window)),
        },
        "reachable": probe.ok(),
        "reasons": checks.iter().filter_map(|check| check["reason"].as_str()).collect::<Vec<_>>(),
        "sessions": {
            "cap": policy.max_sessions.map(|(cap, _)| cap),
            "cap_scope": policy.max_sessions.map(|(_, scope)| scope),
            "fleet_total": fleet.total,
            "fleet_warn": policy.warn_sessions,
            "used": sessions,
        },
    });
    let ui = &context.ui;
    let mut lines = vec![
        format!(
            "{} is {}eligible for new unattended work.",
            ui.command(&name),
            if eligible { "" } else { "not " }
        ),
        String::new(),
    ];
    for check in &checks {
        let symbol = if check["ok"] == true {
            ui.success(ui.symbols.success)
        } else {
            ui.danger(ui.symbols.error)
        };
        lines.push(format!(
            "{symbol} {}",
            check["detail"].as_str().unwrap_or_default()
        ));
    }
    if let (Some(warn), Some(total)) = (policy.warn_sessions, fleet.total)
        && total > warn
    {
        lines.push(String::new());
        lines.push(ui.warning(&format!(
            "{} The fleet is running {total} sessions, past the warning at {warn}.",
            ui.symbols.warning
        )));
    }
    lines.push(String::new());
    lines.push(ui.muted("Nothing is enforced. Gate on the exit code."));
    let mut done = Done::new(data, lines.join("\n"));
    if !eligible {
        done.outcome.exit_code = exit::ERROR;
    }
    Ok(done)
}

pub fn drift(context: &Context) -> Result<Done, AppError> {
    let timeout = bounded(&context.options, "timeout", 1, 60_000)?;
    let store = open_store()?;
    let reference = drift::resolve_reference(Duration::from_millis(timeout as u64))?;
    let now = now_ms();
    let machines = store.list()?;
    let mut counts = [0_usize; 4];
    let states = ["current", "outdated", "diverged", "unknown"];
    let ui = &context.ui;
    let mut rows = Vec::new();
    let records: Vec<Value> = machines
        .iter()
        .map(|machine| {
            let verdict = drift::derive(&machine.config, &reference.commit, now);
            if let Some(index) = states.iter().position(|state| *state == verdict.state) {
                counts[index] += 1;
            }
            let mark = match verdict.state {
                "current" => ui.success(ui.symbols.success),
                "diverged" => ui.danger(ui.symbols.error),
                "outdated" => ui.warning(ui.symbols.warning),
                _ => ui.muted("○"),
            };
            rows.push(vec![
                mark,
                machine.name.clone(),
                verdict.state.to_owned(),
                config_cell(machine, context),
                ui.muted(&verdict.detail),
            ]);
            json!({
                "commit": machine.config.commit,
                "config_checked_at": opt_iso(machine.config.checked_at),
                "detail": verdict.detail,
                "name": machine.name,
                "reason": verdict.reason,
                "state": verdict.state,
                "verify": machine.config.verify,
            })
        })
        .collect();
    let human = if machines.is_empty() {
        format!(
            "{}\nAdd one with {}.",
            ui.muted("No machines to compare."),
            ui.command(&format!("{NAME} add <name> <endpoint>"))
        )
    } else {
        let mut summary = vec![format!("{} current", counts[0])];
        for (index, state) in states.iter().enumerate().skip(1) {
            if counts[index] > 0 {
                summary.push(format!("{} {state}", counts[index]));
            }
        }
        let short: String = reference.commit.chars().take(12).collect();
        format!(
            "{}\n\n{}\n{}",
            ui.table(&["", "Name", "State", "Applied", "Detail"], &rows),
            summary.join(", "),
            ui.muted(&format!(
                "Source head {short} read from the {} clone.",
                reference.source
            ))
        )
    };
    let all_current = counts[0] == machines.len();
    let mut done = Done::new(
        json!({
            "checked_at": iso_ms(now),
            "machines": records,
            "reference": { "commit": reference.commit, "source": reference.source },
            "states": { "current": counts[0], "diverged": counts[2], "outdated": counts[1], "unknown": counts[3] },
        }),
        human,
    );
    if !all_current {
        done.outcome.exit_code = exit::ERROR;
    }
    Ok(done)
}
