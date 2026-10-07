//! The system commands: schema and describe print the catalog, whole or
//! one command at a time; completion writes shell completions; doctor runs
//! offline checks of this install.

use serde_json::{Value, json};

use super::machines::open_store;
use super::readings::human_bytes;
use super::{ArgValue, Context, Done, NAME};
use crate::catalog;
use crate::errors::{AppError, exit};
use crate::output::to_json;
use crate::store::resolve_home;

pub fn schema() -> Done {
    let data = catalog::json().clone();
    let human = to_json(&data, false);
    Done::new(data, human)
}

pub fn describe(context: &Context) -> Result<Done, AppError> {
    let tokens = match context.arguments.first() {
        Some(ArgValue::Many(tokens)) => tokens.clone(),
        Some(ArgValue::One(Some(token))) => vec![token.clone()],
        _ => Vec::new(),
    };
    let Some(position) = catalog::COMMANDS
        .iter()
        .position(|command| command.path == tokens)
    else {
        return Err(AppError::usage(
            "command_not_found",
            format!("No command matches \"{}\".", tokens.join(" ")),
        )
        .hint(format!("Run '{NAME} schema --json' to list command paths.")));
    };
    let command = &catalog::COMMANDS[position];
    let ui = &context.ui;
    let mut lines = vec![
        ui.heading(&format!("{NAME} {}", command.path.join(" "))),
        command.summary.to_owned(),
    ];
    if let Some(description) = command.description {
        lines.extend([String::new(), description.to_owned()]);
    }
    if !command.arguments.is_empty() {
        let rows: Vec<Vec<String>> = command
            .arguments
            .iter()
            .map(|argument| {
                vec![
                    argument.name.to_owned(),
                    if argument.required { "yes" } else { "no" }.to_owned(),
                    argument.description.unwrap_or_default().to_owned(),
                ]
            })
            .collect();
        lines.extend([
            String::new(),
            ui.heading("Arguments:"),
            ui.table(&["Name", "Required", "Description"], &rows),
        ]);
    }
    if !command.options.is_empty() {
        let rows: Vec<Vec<String>> = command
            .options
            .iter()
            .map(|option| vec![option.flags.to_owned(), option.description.to_owned()])
            .collect();
        lines.extend([
            String::new(),
            ui.heading("Options:"),
            ui.table(&["Flags", "Description"], &rows),
        ]);
    }
    if !command.examples.is_empty() {
        lines.extend([String::new(), ui.heading("Examples:")]);
        lines.extend(
            command
                .examples
                .iter()
                .map(|example| format!("  {} {example}", ui.muted("$"))),
        );
    }
    Ok(Done::new(
        catalog::json()["commands"][position].clone(),
        lines.join("\n"),
    ))
}

/// Every word of every command path, sorted and once each.
fn tokens() -> Vec<&'static str> {
    let mut words: Vec<&str> = catalog::COMMANDS
        .iter()
        .flat_map(|command| command.path.iter().copied())
        .collect();
    words.sort_unstable();
    words.dedup();
    words
}

pub fn completion(context: &Context) -> Result<Done, AppError> {
    let shell = context.argument(0).unwrap_or_default();
    let words = tokens().join(" ");
    let script = match shell.as_str() {
        "bash" => format!(
            "_{NAME}_completion() {{\n  local current=\"${{COMP_WORDS[COMP_CWORD]}}\"\n  COMPREPLY=( $(compgen -W \"{words}\" -- \"$current\") )\n}}\ncomplete -F _{NAME}_completion {NAME}\n"
        ),
        "zsh" => format!(
            "#compdef {NAME}\n_{NAME}() {{\n  local -a commands\n  commands=({words})\n  _describe '{NAME} commands' commands\n}}\ncompdef _{NAME} {NAME}\n"
        ),
        "fish" => {
            let lines: Vec<String> = tokens()
                .iter()
                .map(|token| format!("complete -c {NAME} -a '{token}'"))
                .collect();
            format!("complete -c {NAME} -f\n{}\n", lines.join("\n"))
        }
        _ => {
            return Err(AppError::usage(
                "unsupported_shell",
                format!("Unsupported shell \"{shell}\"."),
            )
            .hint("Choose bash, zsh, or fish."));
        }
    };
    let human = script.trim_end().to_owned();
    Ok(Done::new(Value::from(script), human))
}

fn check(name: &str, status: &str, detail: String, fix: Option<String>) -> Value {
    let mut record = json!({ "detail": detail });
    if let Some(fix) = fix {
        record["fix"] = json!(fix);
    }
    record["name"] = json!(name);
    record["status"] = json!(status);
    record
}

/// Offline checks: the store opens and its folder is writable, machines
/// are registered, and this is a platform grove supports.
pub fn doctor(context: &Context) -> Result<Done, AppError> {
    let home = resolve_home();
    let mut checks = Vec::new();
    let opened = open_store();
    checks.push(match &opened {
        Ok(_) => check("Data directory", "pass", home.display().to_string(), None),
        Err(error) => check(
            "Data directory",
            "fail",
            format!("{}: {}", home.display(), error.message),
            Some("Set GROVE_HOME to a writable directory.".into()),
        ),
    });
    if let Ok(store) = &opened {
        let machines = store.list()?.len();
        checks.push(if machines == 0 {
            check(
                "Registered machines",
                "warn",
                "none registered".into(),
                Some(format!("Add one with '{NAME} add <name> <endpoint>'.")),
            )
        } else {
            let samples = store.total_samples()?;
            let size = std::fs::metadata(home.join("grove.db")).map_or(String::new(), |meta| {
                format!(", db {}", human_bytes(meta.len() as f64))
            });
            check(
                "Registered machines",
                "pass",
                format!(
                    "{machines} registered, {samples} sample{}{size}",
                    if samples == 1 { "" } else { "s" }
                ),
                None,
            )
        });
    }
    let platform = format!("{}-{}", super::version::platform(), super::version::arch());
    let supported = matches!(std::env::consts::OS, "macos" | "linux");
    checks.push(check(
        "Operating system",
        if supported { "pass" } else { "warn" },
        platform,
        (!supported).then(|| "Use macOS or Linux.".to_owned()),
    ));
    let worst = ["fail", "warn"]
        .into_iter()
        .find(|status| checks.iter().any(|check| check["status"] == *status))
        .unwrap_or("pass");
    let ui = &context.ui;
    let rows: Vec<Vec<String>> = checks
        .iter()
        .map(|check| {
            let status = check["status"].as_str().unwrap_or_default();
            let symbol = match status {
                "pass" => ui.success(ui.symbols.success),
                "warn" => ui.warning(ui.symbols.warning),
                _ => ui.danger(ui.symbols.error),
            };
            let detail = check["detail"].as_str().unwrap_or_default();
            let detail = match check["fix"].as_str() {
                Some(fix) => format!("{detail} {}", ui.muted(&format!("Fix: {fix}"))),
                None => detail.to_owned(),
            };
            vec![
                symbol,
                check["name"].as_str().unwrap_or_default().to_owned(),
                detail,
            ]
        })
        .collect();
    let mut done = Done::new(
        json!({ "checks": checks, "status": worst }),
        ui.table(&["", "Check", "Detail"], &rows),
    );
    if worst == "fail" {
        done.outcome.exit_code = exit::ERROR;
        done.outcome.hint = Some("Resolve failed checks, then run the doctor again.".into());
    }
    Ok(done)
}
