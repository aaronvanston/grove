//! The command line: builds the command tree from the catalog, parses argv
//! as Commander does, runs the command, and writes its outcome or error in
//! the mode the global flags ask for. Only this module writes to stdout or
//! stderr.

mod alerts;
pub mod commander;
mod data;
mod machines;
mod options;
mod policy;
mod probes;
mod readings;
mod system;
mod version;
mod views;

use std::collections::HashMap;
use std::io::Write;

use serde_json::Value;

use crate::catalog;
use crate::errors::{AppError, exit};
use crate::output::{self, ColorMode, Globals, Mode, Outcome};
use crate::style::{Ui, terminal_safe};
use commander::{Arg, ArgValue, Cmd, HelpAfterError, Invocation, Opt, OptValue, Output};
pub use options::js_number;

pub const NAME: &str = "grove";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DESCRIPTION: &str = "Machine registry with health and capacity at a glance";

/// The program-level options, in their help order.
fn global_options() -> Vec<Opt> {
    let mut version = Opt::new("-V, --version", "Print version");
    version.prints_version = true;
    let mut color = Opt::new("--color <when>", "Color output: auto, always, or never");
    color.choices = Some(vec!["auto".into(), "always".into(), "never".into()]);
    color.default = Some(Value::from("auto"));
    vec![
        version,
        color,
        Opt::new("--no-color", "Disable color output"),
        Opt::new("--json", "Emit one structured JSON result"),
        Opt::new("--jsonl", "Emit versioned JSON event records"),
        Opt::new("--compact", "Compact JSON onto one line"),
        Opt::new("--non-interactive", "Never prompt or animate"),
        Opt::new("--no-input", "Alias for --non-interactive"),
        Opt::new("-q, --quiet", "Suppress warnings and hints"),
        Opt::new("--verbose", "Include verbose diagnostics"),
    ]
}

fn examples_text(examples: &[&str], ui: &Ui) -> String {
    if examples.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = examples
        .iter()
        .map(|example| format!("  {} {}", ui.muted("$"), ui.command(example)))
        .collect();
    format!("\n{}\n{}\n", ui.heading("Examples:"), lines.join("\n"))
}

/// Commands in the order help
/// lists them in (the catalog itself is sorted by path).
const REGISTRATION_ORDER: [&str; 39] = [
    "add",
    "list",
    "label",
    "rm",
    "status",
    "sample",
    "show",
    "drift",
    "history",
    "graph",
    "watch",
    "ps",
    "top",
    "alerts add",
    "alerts list",
    "alerts rm",
    "alerts state",
    "alerts events",
    "hooks add",
    "hooks list",
    "hooks rm",
    "hooks test",
    "hooks runs",
    "policy set",
    "policy show",
    "policy rm",
    "policy explain",
    "probe install",
    "probe use",
    "probe uninstall",
    "probe status",
    "stream",
    "prune",
    "retention",
    "version",
    "doctor",
    "schema",
    "describe",
    "completion",
];

const GROUPS: [(&str, &str); 4] = [
    ("alerts", "Manage alert rules and report alert state"),
    ("hooks", "Manage commands that run on alert transitions"),
    (
        "policy",
        "Set fleet policy and report eligibility for unattended work",
    ),
    ("probe", "Install and check the resident probe on machines"),
];

/// The whole command tree, built from the catalog so the parser accepts
/// exactly what `schema` describes.
pub fn program(ui: &Ui) -> Cmd {
    let root_hint = HelpAfterError::Message(format!("(run '{NAME} <command> --help' for details)"));
    let mut program = Cmd {
        name: NAME.into(),
        aliases: Vec::new(),
        summary: DESCRIPTION.into(),
        before_help: Some(format!(
            "{} {} {}\n{DESCRIPTION}\n",
            ui.brand("◆"),
            ui.heading(NAME),
            ui.muted(VERSION)
        )),
        after_help: Some(format!(
            "\n{}\n",
            ui.muted(&format!(
                "Discover contracts with '{NAME} schema --json' or '{NAME} describe <command>'."
            ))
        )),
        arguments: Vec::new(),
        options: global_options(),
        commands: Vec::new(),
        help_after_error: root_hint.clone(),
        help_group: None,
        has_action: false,
        help_command: false,
        hide_description: true,
    };
    for path in REGISTRATION_ORDER {
        let tokens: Vec<&str> = path.split(' ').collect();
        let Some(spec) = catalog::find(&tokens) else {
            continue;
        };
        let help_group = Some(
            if spec.module == "system" {
                "System commands:"
            } else {
                "Machine commands:"
            }
            .to_owned(),
        );
        let mut parent = &mut program;
        for token in &tokens[..tokens.len() - 1] {
            if parent.find_index(token).is_none() {
                let description = GROUPS.iter().find(|(name, _)| name == token).map_or_else(
                    || format!("{token} commands"),
                    |(_, text)| (*text).to_owned(),
                );
                parent.commands.push(Cmd {
                    name: (*token).to_owned(),
                    aliases: Vec::new(),
                    summary: description,
                    before_help: None,
                    after_help: None,
                    arguments: Vec::new(),
                    options: Vec::new(),
                    commands: Vec::new(),
                    help_after_error: HelpAfterError::Help,
                    help_group: help_group.clone(),
                    has_action: false,
                    help_command: true,
                    hide_description: false,
                });
            }
            let index = parent.find_index(token).expect("group was just added");
            parent = &mut parent.commands[index];
        }
        // Commander copies this setting into a command when it is created:
        // the program's one-line hint, or a group's whole help.
        let inherited = parent.help_after_error.clone();
        let options = spec
            .options
            .iter()
            .map(|option| {
                let mut built = Opt::new(option.flags, option.description);
                built.choices = option
                    .choices
                    .map(|choices| choices.iter().map(|&choice| choice.to_owned()).collect());
                built.default = option.default_value();
                built
            })
            .collect();
        parent.commands.push(Cmd {
            name: tokens
                .last()
                .map_or_else(String::new, |&name| name.to_owned()),
            aliases: spec.aliases.iter().map(|&alias| alias.to_owned()).collect(),
            summary: spec.summary.to_owned(),
            before_help: spec.description.map(|text| format!("\n{text}\n")),
            after_help: Some(examples_text(spec.examples, ui)),
            arguments: spec
                .arguments
                .iter()
                .map(|argument| Arg {
                    name: argument.name.to_owned(),
                    required: argument.required,
                    variadic: argument.variadic,
                    description: Some(argument.description.unwrap_or_default().to_owned()),
                })
                .collect(),
            options,
            commands: Vec::new(),
            help_after_error: inherited,
            // Commands inside a group list under its plain "Commands:".
            help_group: (tokens.len() == 1).then_some(help_group).flatten(),
            has_action: true,
            help_command: false,
            hide_description: false,
        });
    }
    program
}

impl Cmd {
    fn find_index(&self, name: &str) -> Option<usize> {
        self.commands
            .iter()
            .position(|command| command.name == name)
    }
}

/// What is read from argv before parsing, to decide how to report a
/// failure: plain checks over every word. Errors are rendered in this mode
/// even when parsing never got as far as reading the flags.
fn preflight(argv: &[String]) -> Globals {
    let has = |flag: &str| argv.iter().any(|arg| arg == flag);
    let value_after = |name: &str| -> Option<String> {
        if let Some(index) = argv.iter().position(|arg| arg == name) {
            return argv.get(index + 1).cloned();
        }
        let prefix = format!("{name}=");
        argv.iter()
            .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_owned))
    };
    let color_value = value_after("--color");
    let color = if has("--no-color") || color_value.as_deref() == Some("never") {
        ColorMode::Never
    } else if color_value.as_deref() == Some("always") {
        ColorMode::Always
    } else {
        ColorMode::Auto
    };
    let mode = if has("--jsonl") {
        Mode::Jsonl
    } else if has("--json") {
        Mode::Json
    } else {
        Mode::Human
    };
    Globals {
        color,
        compact: has("--compact"),
        mode,
        quiet: has("--quiet") || has("-q"),
    }
}

/// `Boolean(process.env.CI)`: set and not empty.
fn ci() -> bool {
    std::env::var_os("CI").is_some_and(|value| !value.is_empty())
}

fn flag(values: &HashMap<String, OptValue>, name: &str) -> bool {
    matches!(values.get(name), Some(OptValue::Bool(true)))
}

/// The global flags as the parser resolved them, and whether prompting is
/// ruled out, for a command that runs.
fn normalize(values: &HashMap<String, OptValue>) -> Result<(Globals, bool), AppError> {
    let json = flag(values, "json");
    let jsonl = flag(values, "jsonl");
    if json && jsonl {
        return Err(AppError::usage(
            "conflicting_output_modes",
            "Use either --json or --jsonl, not both.",
        ));
    }
    let color = match values.get("color") {
        Some(OptValue::Bool(false)) => ColorMode::Never,
        Some(OptValue::Str(value)) if value == "always" => ColorMode::Always,
        Some(OptValue::Str(value)) if value == "never" => ColorMode::Never,
        _ => ColorMode::Auto,
    };
    let mode = if jsonl {
        Mode::Jsonl
    } else if json {
        Mode::Json
    } else {
        Mode::Human
    };
    let non_interactive = flag(values, "nonInteractive")
        || matches!(values.get("input"), Some(OptValue::Bool(false)))
        || ci();
    let globals = Globals {
        color,
        compact: flag(values, "compact"),
        mode,
        quiet: flag(values, "quiet"),
    };
    Ok((globals, non_interactive))
}

/// Everything a command gets to run with.
pub struct Context {
    /// Prompts are allowed: human mode, not --non-interactive, and both
    /// stdin and stderr are terminals.
    pub interactive: bool,
    pub ui: Ui,
    pub arguments: Vec<ArgValue>,
    pub options: HashMap<String, OptValue>,
    /// --jsonl: a long-running command reports each event as it happens,
    /// one record a line, before its result.
    pub events: bool,
}

impl Context {
    /// Writes one event record now, when the caller asked for --jsonl;
    /// otherwise nothing.
    pub fn event(&self, kind: &str, data: Value) {
        if self.events {
            stdout(&format!("{}\n", output::event_record(kind, data)));
        }
    }

    /// The positional argument at `index`, trimmed, when it was given.
    pub fn argument(&self, index: usize) -> Option<String> {
        match self.arguments.get(index) {
            Some(ArgValue::One(Some(value))) => Some(value.trim().to_owned()),
            _ => None,
        }
    }
}

/// A command's success: the data for machine modes and the text for
/// people. Empty text prints nothing.
pub struct Done {
    pub outcome: Outcome,
    pub human: String,
    /// The command already owned the terminal (top, watch), so nothing is
    /// printed after it, in any mode.
    pub silent: bool,
}

impl Done {
    pub fn new(data: Value, human: String) -> Self {
        Self {
            outcome: Outcome::new(data),
            human,
            silent: false,
        }
    }
}

fn dispatch(path: &str, context: &Context) -> Result<Done, AppError> {
    match path {
        "add" => machines::add(context),
        "list" => machines::list(context),
        "label" => machines::label(context),
        "rm" => machines::rm(context),
        "status" => machines::status(context),
        "sample" => readings::sample(context),
        "show" => readings::show(context),
        "history" => readings::history(context),
        "prune" => data::prune(context),
        "retention" => data::retention(context),
        "alerts add" => alerts::add(context),
        "alerts list" => alerts::list(context),
        "alerts rm" => alerts::rm(context),
        "alerts state" => alerts::state(context),
        "alerts events" => alerts::events(context),
        "hooks add" => alerts::hooks_add(context),
        "hooks list" => alerts::hooks_list(context),
        "hooks rm" => alerts::hooks_rm(context),
        "hooks test" => alerts::hooks_test(context),
        "hooks runs" => alerts::hooks_runs(context),
        "policy set" => policy::set(context),
        "policy show" => policy::show(context),
        "policy rm" => policy::rm(context),
        "policy explain" => policy::explain(context),
        "drift" => policy::drift(context),
        "graph" => views::graph(context),
        "ps" => views::ps(context),
        "top" => views::top(context),
        "watch" => views::watch(context),
        "probe install" => probes::install(context),
        "probe use" => probes::use_dir(context),
        "probe uninstall" => probes::uninstall(context),
        "probe status" => probes::status(context),
        "stream" => probes::stream(context),
        "version" => Ok(version::run(context)),
        "doctor" => system::doctor(context),
        "schema" => Ok(system::schema()),
        "describe" => system::describe(context),
        "completion" => system::completion(context),
        _ => Err(AppError::new(
            "unexpected_error",
            format!("'{NAME} {path}' has no handler."),
        )),
    }
}

/// Writes one block, adding a final newline.
fn line(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

fn render_error(error: &AppError, globals: &Globals, ui: &Ui) -> i32 {
    if globals.mode == Mode::Human {
        stderr(&line(&terminal_safe(&format!(
            "{} {} {}",
            ui.danger(ui.symbols.error),
            ui.danger("Error:"),
            error.message
        ))));
        if let Some(hint) = &error.hint {
            stderr(&line(&terminal_safe(&format!(
                "{} {hint}",
                ui.muted("hint:")
            ))));
        }
        return error.exit_code;
    }
    stderr(&line(&output::error_envelope(error, globals.compact)));
    error.exit_code
}

fn render_done(path: &str, done: &Done, globals: &Globals, ui: &Ui) -> i32 {
    if done.silent {
        return done.outcome.exit_code;
    }
    if globals.mode == Mode::Human {
        // Human text carries names and errors from machines, so it is
        // made safe for the terminal on its way out.
        if !done.human.is_empty() {
            stdout(&line(&terminal_safe(&done.human)));
        }
        if !globals.quiet {
            for warning in &done.outcome.warnings {
                stderr(&line(&terminal_safe(&format!(
                    "{} {} {warning}",
                    ui.warning(ui.symbols.warning),
                    ui.warning("Warning:")
                ))));
            }
            if let Some(hint) = &done.outcome.hint {
                stderr(&line(&terminal_safe(&format!(
                    "{} {hint}",
                    ui.muted("hint:")
                ))));
            }
        }
    } else {
        stdout(&line(&output::success_envelope(
            path,
            &done.outcome,
            globals,
        )));
    }
    done.outcome.exit_code
}

/// Runs one invocation and returns the process exit code.
pub fn run(argv: &[String]) -> i32 {
    let preflight = preflight(argv);
    let preflight_ui = Ui::new(&preflight);
    let program = program(&preflight_ui);
    if argv.is_empty() {
        stdout(&commander::program_help(&program));
        return exit::OK;
    }
    let mut parsed_output = Output::default();
    let parsed = commander::parse(&program, VERSION, argv, &mut parsed_output);
    if !parsed_output.stdout.is_empty() {
        stdout(&parsed_output.stdout);
    }
    if !parsed_output.stderr.is_empty() {
        stderr(&parsed_output.stderr);
    }
    let invocation: Invocation = match parsed {
        Ok(invocation) => invocation,
        Err(stop) if stop.exit_code == 0 => return exit::OK,
        Err(stop) => {
            if preflight.mode == Mode::Human {
                return exit::USAGE;
            }
            let error = AppError::usage("invalid_usage", stop.message);
            return render_error(&error, &preflight, &preflight_ui);
        }
    };
    let (globals, non_interactive) = match normalize(&invocation.globals) {
        Ok(resolved) => resolved,
        Err(error) => return render_error(&error, &preflight, &preflight_ui),
    };
    let path = invocation.path.join(" ");
    let interactive = {
        use std::io::IsTerminal;
        globals.mode == Mode::Human
            && !non_interactive
            && std::io::stdin().is_terminal()
            && std::io::stderr().is_terminal()
    };
    let context = Context {
        interactive,
        ui: Ui::new(&globals),
        arguments: invocation.arguments,
        options: invocation.options,
        events: globals.mode == Mode::Jsonl,
    };
    match dispatch(&path, &context) {
        Ok(done) => render_done(&path, &done, &globals, &context.ui),
        Err(error) => render_error(&error, &preflight, &preflight_ui),
    }
}

/// Writes to the process's stdout or stderr. A closed pipe
/// (`grove history x | head`) ends the program quietly with exit 0.
fn write_or_exit(mut stream: impl Write, text: &str) {
    if let Err(error) = stream
        .write_all(text.as_bytes())
        .and_then(|()| stream.flush())
        && error.kind() == std::io::ErrorKind::BrokenPipe
    {
        std::process::exit(exit::OK);
    }
}

fn stdout(text: &str) {
    write_or_exit(std::io::stdout().lock(), text);
}

fn stderr(text: &str) {
    write_or_exit(std::io::stderr().lock(), text);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parser and the catalog agree: every command the catalog lists is
    /// in the tree, and help lists every one of them once.
    #[test]
    fn every_catalog_command_is_in_the_tree() {
        assert_eq!(catalog::json()["schemaVersion"], output::SCHEMA_VERSION);
        let ui = Ui::new(&preflight(&[]));
        let program = program(&ui);
        for command in catalog::COMMANDS {
            let mut level = &program;
            for token in command.path {
                level = level
                    .commands
                    .iter()
                    .find(|candidate| candidate.name == *token)
                    .unwrap_or_else(|| panic!("{} is missing", command.path.join(" ")));
            }
            assert!(level.has_action);
        }
        assert_eq!(REGISTRATION_ORDER.len(), catalog::COMMANDS.len());
    }
}
