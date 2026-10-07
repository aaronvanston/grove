//! The alerts and hooks commands: rules, their state and events, and the
//! commands that run when an alert fires or clears.

use serde_json::{Value, json};

use super::machines::{open_store, require, valid_name};
use super::options::{bounded, duration, invalid_options, string};
use super::{Context, Done, NAME};
use crate::alerts::{Event, FLEET, RULE_METRICS, Rule, breaches_below, scope_record};
use crate::errors::{AppError, exit};
use crate::output::{format_duration, iso_ms, now_ms, num, opt_iso, opt_num, round1};
use crate::store::{Hook, HookRun, Store};

fn comparison(metric: &str) -> &'static str {
    if breaches_below(metric) {
        "below"
    } else {
        "above"
    }
}

/// "cpu above 90 for 10m", "unreachable 5m", "agent sessions above 10".
fn condition(metric: &str, threshold: &Value, window: &Value) -> String {
    let window = window.as_str().unwrap_or_default();
    match metric {
        "down" => format!("unreachable {window}"),
        "agent_sessions" => format!("agent sessions above {threshold}"),
        _ => format!("{metric} {} {threshold} for {window}", comparison(metric)),
    }
}

fn rule_record(rule: &Rule) -> Value {
    json!({
        "created_at": iso_ms(rule.created_at),
        "machine": scope_record(&rule.machine),
        "metric": rule.metric,
        "threshold": opt_num(rule.threshold),
        "window": format_duration(rule.window_ms),
        "window_ms": rule.window_ms,
    })
}

/// The machine argument at `index`, which must be registered when given;
/// absent means the fleet.
fn scope(context: &Context, store: &Store, index: usize) -> Result<String, AppError> {
    match context.argument(index).filter(|name| !name.is_empty()) {
        Some(name) => Ok(require(store, &name)?.name),
        None => Ok(FLEET.to_owned()),
    }
}

fn metric_argument(context: &Context) -> Result<&'static str, AppError> {
    let metric = context.argument(0).unwrap_or_default();
    RULE_METRICS
        .iter()
        .copied()
        .find(|known| *known == metric)
        .ok_or_else(|| {
            AppError::usage(
                "invalid_metric",
                format!("\"{metric}\" is not a metric a rule can watch."),
            )
            .hint("Use cpu, mem, disk, load1, swap, cpu_temp, battery, or down.")
        })
}

/// A threshold option: zod's `z.coerce.number().min(0).optional()`.
fn threshold_option(context: &Context, name: &str) -> Result<Option<f64>, AppError> {
    let Some(text) = string(&context.options, name) else {
        return Ok(None);
    };
    let value = super::js_number(&text);
    if value.is_nan() {
        return Err(invalid_options(vec![json!({
            "expected": "number",
            "code": "invalid_type",
            "received": "NaN",
            "path": [name],
            "message": "Invalid input: expected number, received NaN",
        })]));
    }
    if value < 0.0 {
        return Err(invalid_options(vec![json!({
            "origin": "number",
            "code": "too_small",
            "minimum": 0,
            "inclusive": true,
            "path": [name],
            "message": "Too small: expected number to be >=0",
        })]));
    }
    Ok(Some(value))
}

/// Which flag a metric takes is a property of the metric, so the wrong one
/// is a usage error naming the right one.
fn threshold_for(
    metric: &str,
    above: Option<f64>,
    below: Option<f64>,
) -> Result<Option<f64>, AppError> {
    let rule_error =
        |message: String, hint: String| AppError::usage("invalid_alert_rule", message).hint(hint);
    if metric == "down" {
        if above.is_some() || below.is_some() {
            return Err(rule_error(
                "down rules do not take a threshold.".into(),
                "A down rule only takes --for, the grace window.".into(),
            ));
        }
        return Ok(None);
    }
    let word = comparison(metric);
    let (given, other) = if breaches_below(metric) {
        (below, above)
    } else {
        (above, below)
    };
    if other.is_some() {
        return Err(rule_error(
            format!("A {metric} rule breaches {word} its threshold."),
            format!("Pass --{word} <value> instead."),
        ));
    }
    given.map(Some).ok_or_else(|| {
        rule_error(
            format!("A {metric} rule needs a threshold."),
            format!("Pass --{word} <value>, the threshold the metric must stay {word}."),
        )
    })
}

pub fn add(context: &Context) -> Result<Done, AppError> {
    let above = threshold_option(context, "above")?;
    let below = threshold_option(context, "below")?;
    let Some(window) = string(&context.options, "for") else {
        return Err(invalid_options(vec![json!({
            "expected": "string",
            "code": "invalid_type",
            "path": ["for"],
            "message": "Invalid input: expected string, received undefined",
        })]));
    };
    let metric = metric_argument(context)?;
    let window_ms = duration(&window)?;
    let threshold = threshold_for(metric, above, below)?;
    let store = open_store()?;
    let machine = scope(context, &store, 1)?;
    let rule = store.add_rule(metric, &machine, threshold, window_ms)?;
    let record = rule_record(&rule);
    let ui = &context.ui;
    let condition = if metric == "down" {
        format!("down for {}", record["window"].as_str().unwrap_or_default())
    } else {
        condition(metric, &record["threshold"], &record["window"])
    };
    let scope = if machine == FLEET {
        "the fleet"
    } else {
        &machine
    };
    let human = format!(
        "{} Alerting on {condition} for {}",
        ui.success(ui.symbols.success),
        ui.command(scope)
    );
    Ok(Done::new(record, human))
}

pub fn rm(context: &Context) -> Result<Done, AppError> {
    let metric = metric_argument(context)?;
    let machine = context.argument(1).unwrap_or_default();
    let store = open_store()?;
    if !store.remove_rule(metric, &machine)? {
        let scope = if machine == FLEET {
            "the fleet".to_owned()
        } else {
            format!("\"{machine}\"")
        };
        return Err(AppError::new(
            "alert_rule_not_found",
            format!("No {metric} rule exists for {scope}."),
        )
        .hint(format!("See what is configured with '{NAME} alerts list'.")));
    }
    let ui = &context.ui;
    let human = format!(
        "{} Removed the {metric} rule for {}",
        ui.success(ui.symbols.success),
        ui.command(if machine == FLEET {
            "the fleet"
        } else {
            &machine
        })
    );
    Ok(Done::new(
        json!({ "machine": scope_record(&machine), "metric": metric, "removed": true }),
        human,
    ))
}

pub fn list(context: &Context) -> Result<Done, AppError> {
    let rules = open_store()?.list_rules()?;
    let records: Vec<Value> = rules.iter().map(rule_record).collect();
    let ui = &context.ui;
    let human = if rules.is_empty() {
        format!(
            "{}\nAdd one with {}.",
            ui.muted("No alert rules configured."),
            ui.command(&format!("{NAME} alerts add cpu --above 90 --for 10m"))
        )
    } else {
        let rows: Vec<Vec<String>> = records
            .iter()
            .map(|rule| {
                let metric = rule["metric"].as_str().unwrap_or_default();
                vec![
                    metric.to_owned(),
                    rule["machine"].as_str().unwrap_or("fleet").to_owned(),
                    if metric == "down" {
                        "unreachable".into()
                    } else {
                        format!("{} {}", comparison(metric), rule["threshold"])
                    },
                    rule["window"].as_str().unwrap_or_default().to_owned(),
                ]
            })
            .collect();
        ui.table(&["Metric", "Scope", "Condition", "Window"], &rows)
    };
    Ok(Done::new(Value::Array(records), human))
}

/// Every rule that applies to each machine, with its current state, and
/// the fleet warning when one is set: the records `alerts state` prints.
pub fn applicable_states(
    store: &Store,
    machines: &[String],
    fleet: bool,
) -> Result<Vec<Value>, AppError> {
    let rules = store.list_rules()?;
    let states = store.list_states()?;
    let mut records = Vec::new();
    for machine in machines {
        for metric in RULE_METRICS {
            let rule = rules
                .iter()
                .find(|rule| rule.metric == metric && rule.machine == *machine)
                .or_else(|| {
                    rules
                        .iter()
                        .find(|rule| rule.metric == metric && rule.machine == FLEET)
                });
            let Some(rule) = rule else {
                continue;
            };
            let state = states
                .iter()
                .find(|state| state.machine == *machine && state.metric == metric);
            records.push(json!({
                "last_value": opt_num(state.and_then(|state| state.last_value).map(round1)),
                "machine": machine,
                "metric": metric,
                "since": opt_iso(state.and_then(|state| state.since)),
                "threshold": opt_num(rule.threshold),
                "triggered": state.is_some_and(|state| state.triggered),
                "window": format_duration(rule.window_ms),
            }));
        }
    }
    if fleet && let Some(warn) = store.policy(FLEET)?.and_then(|row| row.warn_sessions) {
        let state = states
            .iter()
            .find(|state| state.machine == FLEET && state.metric == "agent_sessions");
        records.push(json!({
            "last_value": opt_num(state.and_then(|state| state.last_value)),
            "machine": null,
            "metric": "agent_sessions",
            "since": opt_iso(state.and_then(|state| state.since)),
            "threshold": warn,
            "triggered": state.is_some_and(|state| state.triggered),
            "window": null,
        }));
    }
    Ok(records)
}

pub fn state(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let named = context.argument(0).filter(|name| !name.is_empty());
    let machines = match &named {
        Some(name) => vec![require(&store, name)?.name],
        None => store
            .list()?
            .into_iter()
            .map(|machine| machine.name)
            .collect(),
    };
    let states = applicable_states(&store, &machines, named.is_none())?;
    let firing = states
        .iter()
        .filter(|state| state["triggered"] == true)
        .count();
    let ui = &context.ui;
    let human = if states.is_empty() {
        format!(
            "{}\nAdd one with {}.",
            ui.muted("No alert rules apply."),
            ui.command(&format!("{NAME} alerts add cpu --above 90 --for 10m"))
        )
    } else {
        let rows: Vec<Vec<String>> = states
            .iter()
            .map(|state| {
                let triggered = state["triggered"] == true;
                let since = state["since"]
                    .as_str()
                    .map_or(String::new(), |since| format!(" since {}", &since[11..19]));
                vec![
                    if triggered {
                        ui.danger(ui.symbols.error)
                    } else {
                        ui.success(ui.symbols.success)
                    },
                    state["machine"].as_str().unwrap_or("fleet").to_owned(),
                    condition(
                        state["metric"].as_str().unwrap_or_default(),
                        &state["threshold"],
                        &state["window"],
                    ),
                    if triggered {
                        ui.danger(&format!("firing{since}"))
                    } else {
                        ui.success("ok")
                    },
                    if state["last_value"].is_null() {
                        String::new()
                    } else {
                        state["last_value"].to_string()
                    },
                ]
            })
            .collect();
        let summary = if firing == 0 {
            ui.success("nothing firing")
        } else {
            ui.danger(&format!(
                "{firing} alert{} firing",
                if firing == 1 { "" } else { "s" }
            ))
        };
        format!(
            "{}\n\n{summary}",
            ui.table(&["", "Machine", "Rule", "State", "Last"], &rows)
        )
    };
    let mut done = Done::new(
        json!({ "checked_at": iso_ms(now_ms()), "firing": firing, "states": states }),
        human,
    );
    if firing > 0 {
        done.outcome.exit_code = exit::ERROR;
    }
    Ok(done)
}

fn event_record(event: &Event) -> Value {
    json!({
        "at": iso_ms(event.at),
        "kind": event.kind,
        "machine": scope_record(&event.machine),
        "metric": event.metric,
        "threshold": opt_num(event.threshold),
        "value": opt_num(event.value),
        "window": format_duration(event.window_ms),
    })
}

pub fn events(context: &Context) -> Result<Done, AppError> {
    let limit = bounded(&context.options, "limit", 1, 1000)?;
    let events = open_store()?.list_events(limit)?;
    let ui = &context.ui;
    let human = if events.is_empty() {
        ui.muted("No alert events recorded.")
    } else {
        let rows: Vec<Vec<String>> = events
            .iter()
            .map(|event| {
                vec![
                    iso_ms(event.at).replace('T', " ")[..19].to_owned(),
                    if event.machine == FLEET {
                        "fleet".into()
                    } else {
                        event.machine.clone()
                    },
                    event.metric.clone(),
                    if event.kind == "fired" {
                        ui.danger("fired")
                    } else {
                        ui.success("cleared")
                    },
                    event
                        .value
                        .map_or(String::new(), |value| num(round1(value)).to_string()),
                ]
            })
            .collect();
        ui.table(&["At", "Machine", "Metric", "Event", "Value"], &rows)
    };
    Ok(Done::new(
        Value::Array(events.iter().map(event_record).collect()),
        human,
    ))
}

fn hook_record(hook: &Hook) -> Value {
    json!({
        "command": hook.command,
        "created_at": iso_ms(hook.created_at),
        "machine": scope_record(&hook.machine),
        "name": hook.name,
        "on": hook.on,
    })
}

fn hook_not_found(name: &str) -> AppError {
    AppError::new(
        "hook_not_found",
        format!("No hook named \"{name}\" is configured."),
    )
    .hint(format!("See what is configured with '{NAME} hooks list'."))
}

pub fn hooks_add(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    if !valid_name(&name) {
        return Err(AppError::usage(
            "invalid_hook_name",
            format!("\"{name}\" is not a valid hook name."),
        )
        .hint(
            "Use letters, digits, dots, dashes, or underscores, starting with a letter or digit.",
        ));
    }
    let command = context.argument(1).unwrap_or_default();
    if command.is_empty() {
        return Err(
            AppError::usage("invalid_hook_command", "A hook needs a command to run.").hint(
                format!("Pass the command in single quotes so {NAME} stores it whole."),
            ),
        );
    }
    let on = string(&context.options, "on").unwrap_or_else(|| "both".into());
    let store = open_store()?;
    let machine = scope(context, &store, 2)?;
    let hook = store.add_hook(&name, &command, &machine, &on)?;
    let ui = &context.ui;
    let when = if on == "both" {
        "fires and clears".to_owned()
    } else {
        format!("{on}s")
    };
    let human = format!(
        "{} Running {} on {when} for {}",
        ui.success(ui.symbols.success),
        ui.command(&name),
        ui.command(if machine == FLEET {
            "the fleet"
        } else {
            &machine
        })
    );
    Ok(Done::new(hook_record(&hook), human))
}

pub fn hooks_rm(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    if !open_store()?.remove_hook(&name)? {
        return Err(hook_not_found(&name));
    }
    let ui = &context.ui;
    let human = format!(
        "{} Removed {}",
        ui.success(ui.symbols.success),
        ui.command(&name)
    );
    Ok(Done::new(json!({ "name": name, "removed": true }), human))
}

pub fn hooks_list(context: &Context) -> Result<Done, AppError> {
    let hooks = open_store()?.list_hooks()?;
    let ui = &context.ui;
    let human = if hooks.is_empty() {
        format!(
            "{}\nAdd one with {}.",
            ui.muted("No hooks configured."),
            ui.command(&format!("{NAME} hooks add <name> '<command>'"))
        )
    } else {
        let rows: Vec<Vec<String>> = hooks
            .iter()
            .map(|hook| {
                vec![
                    hook.name.clone(),
                    if hook.machine == FLEET {
                        "fleet".into()
                    } else {
                        hook.machine.clone()
                    },
                    hook.on.clone(),
                    hook.command.clone(),
                ]
            })
            .collect();
        ui.table(&["Name", "Scope", "On", "Command"], &rows)
    };
    Ok(Done::new(
        Value::Array(hooks.iter().map(hook_record).collect()),
        human,
    ))
}

/// Runs one hook against a synthetic event; its exit code becomes this
/// command's.
pub fn hooks_test(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let event = string(&context.options, "event").unwrap_or_else(|| "fire".into());
    let store = open_store()?;
    let hook = store
        .list_hooks()?
        .into_iter()
        .find(|hook| hook.name == name)
        .ok_or_else(|| hook_not_found(&name))?;
    let payload = crate::hooks::test_payload(&hook, &event);
    let outcome = crate::hooks::run(&hook, &payload);
    store.record_hook_run(&HookRun {
        hook: hook.name.clone(),
        machine: hook.machine.clone(),
        metric: "test".into(),
        event: event.clone(),
        exit_code: outcome.exit_code,
        stderr: outcome.stderr.clone(),
        at: now_ms(),
    })?;
    let ran = outcome.exit_code == 0;
    let ui = &context.ui;
    let mut human = format!(
        "{} {} ran the synthetic {event} and exited {}",
        if ran {
            ui.success(ui.symbols.success)
        } else {
            ui.danger(ui.symbols.error)
        },
        ui.command(&hook.name),
        outcome.exit_code
    );
    if !outcome.stderr.is_empty() {
        human = format!("{human}\n{}", ui.muted(&outcome.stderr));
    }
    let mut done = Done::new(
        json!({
            "command": hook.command,
            "exit_code": outcome.exit_code,
            "hook": hook.name,
            "payload": payload,
            "ran": ran,
            "stderr": outcome.stderr,
        }),
        human,
    );
    if !ran {
        done.outcome.exit_code = exit::ERROR;
    }
    Ok(done)
}

pub fn hooks_runs(context: &Context) -> Result<Done, AppError> {
    let limit = bounded(&context.options, "limit", 1, 1000)?;
    let runs = open_store()?.list_hook_runs(limit)?;
    let records: Vec<Value> = runs
        .iter()
        .map(|run| {
            json!({
                "at": iso_ms(run.at),
                "event": run.event,
                "exit_code": run.exit_code,
                "hook": run.hook,
                "machine": scope_record(&run.machine),
                "metric": run.metric,
                "stderr": run.stderr,
            })
        })
        .collect();
    let ui = &context.ui;
    let human = if runs.is_empty() {
        ui.muted("No hook runs recorded.")
    } else {
        let rows: Vec<Vec<String>> = runs
            .iter()
            .map(|run| {
                vec![
                    iso_ms(run.at).replace('T', " ")[..19].to_owned(),
                    run.hook.clone(),
                    if run.machine == FLEET {
                        "fleet".into()
                    } else {
                        run.machine.clone()
                    },
                    format!("{} {}", run.metric, run.event),
                    if run.exit_code == 0 {
                        ui.success("0")
                    } else if run.stderr.is_empty() {
                        ui.danger(&run.exit_code.to_string())
                    } else {
                        ui.danger(&format!("{} {}", run.exit_code, run.stderr))
                    },
                ]
            })
            .collect();
        ui.table(&["At", "Hook", "Machine", "Event", "Exit"], &rows)
    };
    Ok(Done::new(Value::Array(records), human))
}
