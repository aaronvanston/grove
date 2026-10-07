//! The registry commands: add, list, label and rm, and the machine record
//! every command that names a machine reports.

use serde_json::{Value, json};

use super::commander::OptValue;
use super::options::{bounded, flag};
use super::{Context, Done, NAME};
use crate::errors::AppError;
use crate::output::{iso_ms, now_ms, opt_iso, round1};
use crate::store::{LABEL_KEYS, LabelPatch, Labels, Machine, Store, normalize_label, resolve_home};
use crate::transport;

/// The commit length a person reads. Machine output keeps the full hash.
const SHORT_COMMIT: usize = 12;

pub fn open_store() -> Result<Store, AppError> {
    Store::open(&resolve_home())
}

pub fn not_found(name: &str) -> AppError {
    AppError::new(
        "machine_not_found",
        format!("No machine named \"{name}\" is registered."),
    )
    .hint(format!("See what is registered with '{NAME} list'."))
}

pub fn require(store: &Store, name: &str) -> Result<Machine, AppError> {
    store.get(name)?.ok_or_else(|| not_found(name))
}

/// Every machine, or just the one named, which must be registered.
pub fn targets(store: &Store, name: Option<&str>) -> Result<Vec<Machine>, AppError> {
    match name {
        None => store.list(),
        Some(name) => Ok(vec![require(store, name)?]),
    }
}

pub fn labels_record(labels: &Labels) -> Value {
    json!({
        "locality": labels[3],
        "power": labels[2],
        "privacy": labels[1],
        "trust": labels[0],
    })
}

pub fn config_record(machine: &Machine) -> Value {
    json!({
        "checked_at": opt_iso(machine.config.checked_at),
        "commit": machine.config.commit,
        "verify": machine.config.verify,
    })
}

pub fn machine_record(machine: &Machine) -> Value {
    json!({
        "added_at": iso_ms(machine.added_at),
        "config": config_record(machine),
        "endpoint": machine.endpoint,
        "labels": labels_record(&machine.labels),
        "name": machine.name,
        "port": machine.port,
    })
}

/// A commit with a marker when the files it wrote no longer match it.
pub fn config_cell(machine: &Machine, context: &Context) -> String {
    let Some(commit) = &machine.config.commit else {
        return "-".into();
    };
    let short: String = commit.chars().take(SHORT_COMMIT).collect();
    if machine.config.verify == Some(0) {
        short
    } else {
        let ui = &context.ui;
        format!("{short} {}", ui.warning(ui.symbols.warning))
    }
}

/// "5m ago": the largest whole unit, at least 1.
pub fn relative(ms: i64, now: i64) -> String {
    let magnitude = (now - ms).abs();
    let (size, label) = [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "m"),
        (1000, "s"),
    ]
    .into_iter()
    .find(|(size, _)| magnitude >= *size)
    .unwrap_or((1000, "s"));
    let count = ((magnitude as f64) / (size as f64)).round().max(1.0);
    format!("{count}{label} ago")
}

fn label_patch(context: &Context) -> LabelPatch {
    LABEL_KEYS.map(|key| match context.options.get(key) {
        Some(OptValue::Str(value)) => Some(normalize_label(value)),
        _ => None,
    })
}

fn add_hint(context: &Context) -> String {
    format!(
        "Add one with {}.",
        context.ui.command(&format!("{NAME} add <name> <endpoint>"))
    )
}

/// Registry identifiers: a letter or digit, then letters, digits, dots,
/// dashes or underscores.
pub fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && characters
            .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
}

pub fn add(context: &Context) -> Result<Done, AppError> {
    let port = bounded(&context.options, "port", 1, 65_535)?;
    let name = context.argument(0).unwrap_or_default();
    let endpoint = context.argument(1).unwrap_or_default();
    if !valid_name(&name) {
        return Err(AppError::usage(
            "invalid_machine_name",
            format!("\"{name}\" is not a valid machine name."),
        )
        .hint(
            "Use letters, digits, dots, dashes, or underscores, starting with a letter or digit.",
        ));
    }
    if endpoint.is_empty() || endpoint.chars().any(char::is_whitespace) {
        return Err(AppError::usage(
            "invalid_endpoint",
            format!("\"{endpoint}\" is not a valid endpoint."),
        )
        .hint("Pass a hostname or IP address without whitespace."));
    }
    let store = open_store()?;
    if store.get(&name)?.is_some() {
        return Err(AppError::new(
            "machine_exists",
            format!("A machine named \"{name}\" is already registered."),
        )
        .hint(format!("Remove it first with '{NAME} rm {name}'.")));
    }
    let labels: Labels = label_patch(context).map(Option::flatten);
    let machine = store.add(&name, &endpoint, port, &labels)?;
    let ui = &context.ui;
    let human = format!(
        "{} Added {} at {}:{}",
        ui.success(ui.symbols.success),
        ui.command(&machine.name),
        machine.endpoint,
        machine.port
    );
    Ok(Done::new(machine_record(&machine), human))
}

pub fn label(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    require(&store, &name)?;
    let patch = label_patch(context);
    if patch.iter().all(Option::is_none) {
        let flags: Vec<String> = LABEL_KEYS.iter().map(|key| format!("--{key}")).collect();
        return Err(AppError::usage(
            "no_labels_given",
            "Labeling a machine needs a label to set.",
        )
        .hint(format!("Pass at least one of {}.", flags.join(", "))));
    }
    store.set_labels(&name, &patch)?;
    let machine = require(&store, &name)?;
    let set: Vec<String> = LABEL_KEYS
        .iter()
        .zip(&machine.labels)
        .filter_map(|(key, value)| value.as_ref().map(|value| format!("{key} {value}")))
        .collect();
    let ui = &context.ui;
    let human = format!(
        "{} Labeled {}{}",
        ui.success(ui.symbols.success),
        ui.command(&machine.name),
        if set.is_empty() {
            " with nothing".to_owned()
        } else {
            format!(": {}", set.join(", "))
        }
    );
    Ok(Done::new(machine_record(&machine), human))
}

/// The label columns any listed machine has a value for.
pub fn label_columns(machines: &[Machine]) -> Vec<usize> {
    (0..LABEL_KEYS.len())
        .filter(|&index| {
            machines
                .iter()
                .any(|machine| machine.labels[index].is_some())
        })
        .collect()
}

fn capitalized(key: &str) -> String {
    let mut characters = key.chars();
    characters
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
        .unwrap_or_default()
}

pub fn list(context: &Context) -> Result<Done, AppError> {
    let machines = open_store()?.list()?;
    let data = Value::Array(machines.iter().map(machine_record).collect());
    let ui = &context.ui;
    if machines.is_empty() {
        let human = format!(
            "{}\n{}",
            ui.muted("No machines registered."),
            add_hint(context)
        );
        return Ok(Done::new(data, human));
    }
    let labels = label_columns(&machines);
    let config = machines
        .iter()
        .any(|machine| machine.config.commit.is_some());
    let header_text: Vec<String> = ["Name".to_owned(), "Endpoint".into(), "Port".into()]
        .into_iter()
        .chain(labels.iter().map(|&index| capitalized(LABEL_KEYS[index])))
        .chain(config.then(|| "Config".to_owned()))
        .chain(["Added".to_owned()])
        .collect();
    let headers: Vec<&str> = header_text.iter().map(String::as_str).collect();
    let now = now_ms();
    let rows: Vec<Vec<String>> = machines
        .iter()
        .map(|machine| {
            [
                machine.name.clone(),
                machine.endpoint.clone(),
                machine.port.to_string(),
            ]
            .into_iter()
            .chain(
                labels
                    .iter()
                    .map(|&index| machine.labels[index].clone().unwrap_or_else(|| "-".into())),
            )
            .chain(config.then(|| config_cell(machine, context)))
            .chain([relative(machine.added_at, now)])
            .collect()
        })
        .collect();
    Ok(Done::new(data, ui.table(&headers, &rows)))
}

/// Asks on stderr; anything but y or yes is no.
fn confirm(question: &str) -> bool {
    use std::io::{BufRead, Write};
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{question} (y/N) ");
    let _ = stderr.flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn rm(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let mut store = open_store()?;
    require(&store, &name)?;
    let confirmed = flag(&context.options, "yes")
        || (context.interactive
            && confirm(&format!("Remove the machine \"{name}\" from the registry?")));
    if !confirmed {
        return Err(AppError::usage(
            "action_required",
            "Removing a machine needs explicit confirmation.",
        )
        .hint("Re-run with --yes."));
    }
    store.remove(&name)?;
    let ui = &context.ui;
    let human = format!(
        "{} Removed {}",
        ui.success(ui.symbols.success),
        ui.command(&name)
    );
    Ok(Done::new(json!({ "name": name, "removed": true }), human))
}

pub fn status(context: &Context) -> Result<Done, AppError> {
    let timeout = bounded(&context.options, "timeout", 1, 60_000)?;
    let store = open_store()?;
    let machines = targets(&store, context.argument(0).as_deref())?;
    let checked_at = now_ms();
    let timeout = std::time::Duration::from_millis(timeout as u64);
    let probes = transport::each(&machines, machines.len(), |machine| {
        transport::run_script(machine, &store.home, "true\n", timeout)
    });
    let mut up = 0;
    let mut records = Vec::new();
    let mut rows = Vec::new();
    let ui = &context.ui;
    for (machine, probe) in machines.iter().zip(&probes) {
        let reachable = probe.ok();
        up += usize::from(reachable);
        let error = (!reachable).then(|| probe.failure());
        let latency = reachable.then(|| round1(probe.elapsed.as_secs_f64() * 1000.0));
        let mut record = machine_record(machine);
        record["error"] = json!(error);
        record["latency_ms"] = crate::output::opt_num(latency);
        record["reachable"] = json!(reachable);
        record["health"] = super::readings::current_health(&store, machine, reachable, checked_at)?;
        rows.push(vec![
            if reachable {
                ui.success(ui.symbols.success)
            } else {
                ui.danger(ui.symbols.error)
            },
            machine.name.clone(),
            format!("{}:{}", machine.endpoint, machine.port),
            match (latency, &error) {
                (Some(latency), _) => format!("{}ms", crate::output::num(latency)),
                (None, error) => ui.muted(error.as_deref().unwrap_or("unreachable")),
            },
            super::readings::health_cell(&record["health"], context),
        ]);
        records.push(record);
    }
    let down = machines.len() - up;
    let data = json!({
        "checked_at": iso_ms(checked_at),
        "down": down,
        "machines": records,
        "up": up,
    });
    let human = if machines.is_empty() {
        format!(
            "{}\n{}",
            ui.muted("No machines to check."),
            add_hint(context)
        )
    } else {
        let summary = if down == 0 {
            ui.success(&format!("{up} up"))
        } else {
            format!(
                "{}, {}",
                ui.success(&format!("{up} up")),
                ui.danger(&format!("{down} down"))
            )
        };
        format!(
            "{}\n\n{summary}",
            ui.table(&["", "Name", "Endpoint", "Latency", "Health"], &rows)
        )
    };
    let mut done = Done::new(data, human);
    if down > 0 {
        done.outcome.exit_code = crate::errors::exit::ERROR;
    }
    Ok(done)
}
