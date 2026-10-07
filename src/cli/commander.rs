//! A command-line parser that reads argv the way Commander 15 does,
//! decision for decision, because those decisions are part of the contract:
//!
//! - Each level (the program, a group such as `alerts`, a command) takes
//!   the options it knows from wherever they sit before `--`, so a global
//!   such as `--json` is consumed even between an option and its value
//!   (`--gate --json --yes` gives --gate the value `--yes`).
//! - From the first option a level doesn't know, the rest of the line is
//!   handed down to the next level to parse; plain words before it are
//!   operands, which are never parsed again.
//! - An option that takes a value takes the next word, whatever it is.
//! - `--opt=value`, combined short flags (`-qV`), `--no-*` negation, and
//!   `--` ending option parsing all behave as Commander's did.
//! - Errors carry Commander's exact message text and "Did you mean"
//!   suggestions, because `--json` wraps them in an invalid_usage envelope.
//!
//! The parser writes what Commander wrote itself (help, `--version`, the
//! error line and what follows it) into an [`Output`] and stops with a
//! [`Stop`] carrying Commander's exit code and message.

use std::collections::HashMap;

use serde_json::Value;

/// What follows an error line: a fixed message or the command's whole help.
#[derive(Debug, Clone)]
pub enum HelpAfterError {
    Message(String),
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OptValue {
    Bool(bool),
    Str(String),
    /// A default from the catalog, typed as the catalog wrote it.
    Default(Value),
}

#[derive(Debug, Clone)]
pub struct Opt {
    pub flags: String,
    pub description: String,
    pub short: Option<String>,
    pub long: Option<String>,
    pub takes_value: bool,
    pub negate: bool,
    pub attribute: String,
    pub choices: Option<Vec<String>>,
    pub default: Option<Value>,
    pub prints_version: bool,
}

impl Opt {
    /// Builds an option from Commander-style flags such as `-y, --yes`,
    /// `--cron <expression>` or `--no-gate`.
    pub fn new(flags: &str, description: &str) -> Self {
        let mut parts: Vec<&str> = flags
            .split([' ', '|', ','])
            .filter(|part| !part.is_empty())
            .collect();
        parts.push("guard");
        let is_short = |part: &str| part.len() == 2 && part.starts_with('-') && part != "--";
        let is_long =
            |part: &str| part.starts_with("--") && part.len() > 2 && !part[2..].starts_with('-');
        let mut index = 0;
        let short = is_short(parts[index]).then(|| {
            index += 1;
            parts[index - 1].to_owned()
        });
        let long = is_long(parts[index]).then(|| parts[index].to_owned());
        let negate = long
            .as_deref()
            .is_some_and(|long| long.starts_with("--no-"));
        let name = long.as_deref().map_or_else(
            || {
                short
                    .as_deref()
                    .unwrap_or_default()
                    .trim_start_matches('-')
                    .to_owned()
            },
            |long| long.trim_start_matches("--").to_owned(),
        );
        let name = if negate {
            name.trim_start_matches("no-").to_owned()
        } else {
            name
        };
        Self {
            flags: flags.to_owned(),
            description: description.to_owned(),
            short,
            long,
            takes_value: flags.contains('<'),
            negate,
            attribute: camel_case(&name),
            choices: None,
            default: None,
            prints_version: false,
        }
    }

    fn is(&self, arg: &str) -> bool {
        self.short.as_deref() == Some(arg) || self.long.as_deref() == Some(arg)
    }
}

fn camel_case(name: &str) -> String {
    let mut result = String::new();
    let mut upper = false;
    for character in name.chars() {
        if character == '-' {
            upper = true;
        } else if upper {
            result.extend(character.to_uppercase());
            upper = false;
        } else {
            result.push(character);
        }
    }
    result
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub name: String,
    pub required: bool,
    pub variadic: bool,
    pub description: Option<String>,
}

impl Arg {
    fn human(&self) -> String {
        let name = if self.variadic {
            format!("{}...", self.name)
        } else {
            self.name.clone()
        };
        if self.required {
            format!("<{name}>")
        } else {
            format!("[{name}]")
        }
    }
}

#[derive(Debug, Clone)]
pub struct Cmd {
    pub name: String,
    pub aliases: Vec<String>,
    /// What lists and help show as the command's description.
    pub summary: String,
    /// Help text printed before and after the generated help.
    pub before_help: Option<String>,
    pub after_help: Option<String>,
    pub arguments: Vec<Arg>,
    pub options: Vec<Opt>,
    pub commands: Vec<Cmd>,
    pub help_after_error: HelpAfterError,
    /// The heading this command is listed under in its parent's help.
    pub help_group: Option<String>,
    /// Leaves run something; the program and groups only dispatch.
    pub has_action: bool,
    /// Groups get Commander's implicit `help [command]` subcommand.
    pub help_command: bool,
    /// The program's own description section is left out of its help.
    pub hide_description: bool,
}

impl Cmd {
    fn find_command(&self, name: &str) -> Option<&Cmd> {
        self.commands.iter().find(|command| {
            command.name == name || command.aliases.iter().any(|alias| alias == name)
        })
    }

    fn find_option(&self, arg: &str) -> Option<&Opt> {
        self.options.iter().find(|option| option.is(arg))
    }
}

#[derive(Debug, Default)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

/// Commander stopped instead of handing over a command to run. Exit code 0
/// means it answered the request itself (help, version).
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    pub exit_code: i32,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArgValue {
    One(Option<String>),
    Many(Vec<String>),
}

/// A command to run, with everything Commander resolved for it.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub path: Vec<String>,
    pub arguments: Vec<ArgValue>,
    pub options: HashMap<String, OptValue>,
    /// The program-level (global) option values.
    pub globals: HashMap<String, OptValue>,
}

struct Parser<'a> {
    version: &'a str,
    output: &'a mut Output,
    globals: HashMap<String, OptValue>,
}

fn defaults(command: &Cmd) -> HashMap<String, OptValue> {
    let mut values = HashMap::new();
    for option in &command.options {
        if let Some(default) = &option.default {
            values.insert(option.attribute.clone(), OptValue::Default(default.clone()));
        } else if option.negate
            && !command
                .options
                .iter()
                .any(|other| !other.negate && other.attribute == option.attribute)
        {
            values.insert(option.attribute.clone(), OptValue::Bool(true));
        }
    }
    values
}

fn maybe_option(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
}

/// `-5`, `-.5`, `-1e3`: Commander lets these through as values.
fn negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    let (mantissa, exponent) = match rest.find('e') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let mantissa_ok = digits(mantissa)
        || mantissa.split_once('.').is_some_and(|(whole, fraction)| {
            (whole.is_empty() || digits(whole)) && digits(fraction)
        });
    let exponent_ok = exponent
        .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)));
    mantissa_ok && exponent_ok
}

/// Commander's "Did you mean" line: candidates within the smallest edit
/// distance (at most 3) that are also more than 40% similar.
pub fn suggest_similar(word: &str, candidates: &[String]) -> String {
    let mut unique: Vec<String> = Vec::new();
    for candidate in candidates {
        if !unique.contains(candidate) {
            unique.push(candidate.clone());
        }
    }
    let searching_options = word.starts_with("--");
    let (word, unique): (&str, Vec<String>) = if searching_options {
        (
            &word[2..],
            unique
                .iter()
                .map(|candidate| candidate.get(2..).unwrap_or("").to_owned())
                .collect(),
        )
    } else {
        (word, unique)
    };
    let word: Vec<char> = word.chars().collect();
    let mut similar: Vec<String> = Vec::new();
    let mut best = 3;
    for candidate in unique {
        let chars: Vec<char> = candidate.chars().collect();
        if chars.len() <= 1 {
            continue;
        }
        let distance = edit_distance(&word, &chars);
        let length = word.len().max(chars.len()) as f64;
        let similarity = (length - distance as f64) / length;
        if similarity > 0.4 {
            if distance < best {
                best = distance;
                similar = vec![candidate];
            } else if distance == best {
                similar.push(candidate);
            }
        }
    }
    similar.sort();
    if searching_options {
        similar = similar
            .into_iter()
            .map(|candidate| format!("--{candidate}"))
            .collect();
    }
    match similar.len() {
        0 => String::new(),
        1 => format!("\n(Did you mean {}?)", similar[0]),
        _ => format!("\n(Did you mean one of {}?)", similar.join(", ")),
    }
}

/// Optimal string alignment distance, with Commander's early exit.
fn edit_distance(a: &[char], b: &[char]) -> usize {
    if a.len().abs_diff(b.len()) > 3 {
        return a.len().max(b.len());
    }
    let mut d = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for j in 1..=b.len() {
        for i in 1..=a.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

const HELP_FLAGS: [&str; 2] = ["-h", "--help"];

/// Commander's useColor(): NO_COLOR and FORCE_COLOR decide, else whether
/// the stream help goes to is a terminal.
fn help_has_colors(stream_is_terminal: bool) -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let force = std::env::var("FORCE_COLOR").ok();
    if set("NO_COLOR") || matches!(force.as_deref(), Some("0" | "false")) {
        return false;
    }
    if set("FORCE_COLOR") || std::env::var_os("CLICOLOR_FORCE").is_some() {
        return true;
    }
    stream_is_terminal
}

/// Help as Commander wrote it: with its color codes only when the stream
/// it goes to shows color.
fn help_for(text: String, to_stderr: bool) -> String {
    use std::io::IsTerminal;
    let terminal = if to_stderr {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    };
    if help_has_colors(terminal) {
        text
    } else {
        strip_ansi(&text)
    }
}

fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(character);
        }
    }
    result
}

impl Parser<'_> {
    fn stop(&mut self, chain: &[&Cmd], message: String) -> Stop {
        let command = chain
            .last()
            .copied()
            .expect("the chain holds at least the program");
        self.output.stderr.push_str(&message);
        self.output.stderr.push('\n');
        match &command.help_after_error {
            HelpAfterError::Message(text) => {
                self.output.stderr.push_str(text);
                self.output.stderr.push('\n');
            }
            HelpAfterError::Help => {
                self.output.stderr.push('\n');
                let help = help_for(render_help(chain), true);
                self.output.stderr.push_str(&help);
            }
        }
        Stop {
            exit_code: 1,
            message,
        }
    }

    /// `help()`: to stdout and exit 0, or with `error` to stderr and exit 1.
    fn help(&mut self, chain: &[&Cmd], error: bool) -> Stop {
        let help = help_for(render_help(chain), error);
        if error {
            self.output.stderr.push_str(&help);
        } else {
            self.output.stdout.push_str(&help);
        }
        Stop {
            exit_code: i32::from(error),
            message: "(outputHelp)".into(),
        }
    }

    fn emit(
        &mut self,
        chain: &[&Cmd],
        option: &Opt,
        value: Option<String>,
        values: &mut HashMap<String, OptValue>,
    ) -> Result<(), Stop> {
        if option.prints_version {
            self.output.stdout.push_str(self.version);
            self.output.stdout.push('\n');
            return Err(Stop {
                exit_code: 0,
                message: self.version.to_owned(),
            });
        }
        let resolved = if option.negate {
            OptValue::Bool(false)
        } else if option.takes_value {
            let value = value.unwrap_or_default();
            if let Some(choices) = &option.choices
                && !choices.contains(&value)
            {
                let message = format!(
                    "error: option '{}' argument '{value}' is invalid. Allowed choices are {}.",
                    option.flags,
                    choices.join(", ")
                );
                return Err(self.stop(chain, message));
            }
            OptValue::Str(value)
        } else {
            OptValue::Bool(true)
        };
        values.insert(option.attribute.clone(), resolved);
        Ok(())
    }

    /// Commander's parseOptions: splits one level's args into operands and
    /// the unknown remainder, consuming the options this level knows.
    fn parse_options(
        &mut self,
        chain: &[&Cmd],
        args: &[String],
        values: &mut HashMap<String, OptValue>,
    ) -> Result<(Vec<String>, Vec<String>), Stop> {
        let command = *chain.last().expect("non-empty chain");
        let mut operands: Vec<String> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        let mut to_unknown = false;
        let mut group: Option<String> = None;
        let mut index = 0;
        while index < args.len() || group.is_some() {
            let arg = match group.take() {
                Some(pending) => pending,
                None => {
                    index += 1;
                    args[index - 1].clone()
                }
            };
            if arg == "--" {
                if to_unknown {
                    unknown.push(arg);
                    unknown.extend_from_slice(&args[index..]);
                } else {
                    operands.extend_from_slice(&args[index..]);
                }
                break;
            }
            if maybe_option(&arg)
                && let Some(option) = command.find_option(&arg)
            {
                if option.takes_value {
                    let value = args.get(index).cloned();
                    index += 1;
                    if value.is_none() {
                        let message = format!("error: option '{}' argument missing", option.flags);
                        return Err(self.stop(chain, message));
                    }
                    self.emit(chain, option, value, values)?;
                } else {
                    self.emit(chain, option, None, values)?;
                }
                continue;
            }
            if arg.len() > 2 && arg.starts_with('-') && !arg.starts_with("--") {
                let first = arg.chars().nth(1).unwrap_or_default();
                if let Some(option) = command.find_option(&format!("-{first}")) {
                    let rest: String = arg.chars().skip(2).collect();
                    if option.takes_value {
                        self.emit(chain, option, Some(rest), values)?;
                    } else {
                        self.emit(chain, option, None, values)?;
                        group = Some(format!("-{rest}"));
                    }
                    continue;
                }
            }
            if arg.starts_with("--")
                && arg.len() > 2
                && let Some(equals) = arg[2..].find('=').map(|offset| offset + 2)
                && equals > 2
                && let Some(option) = command.find_option(&arg[..equals])
                && option.takes_value
            {
                self.emit(chain, option, Some(arg[equals + 1..].to_owned()), values)?;
                continue;
            }
            if !to_unknown
                && maybe_option(&arg)
                && !(command.commands.is_empty() && negative_number(&arg))
            {
                to_unknown = true;
            }
            if to_unknown {
                unknown.push(arg);
            } else {
                operands.push(arg);
            }
        }
        Ok((operands, unknown))
    }

    fn unknown_option(&mut self, chain: &[&Cmd], flag: &str) -> Stop {
        let mut suggestion = String::new();
        if flag.starts_with("--") {
            let mut candidates = Vec::new();
            for command in chain.iter().rev() {
                candidates.extend(
                    command
                        .options
                        .iter()
                        .filter_map(|option| option.long.clone()),
                );
                candidates.push("--help".to_owned());
            }
            suggestion = suggest_similar(flag, &candidates);
        }
        self.stop(chain, format!("error: unknown option '{flag}'{suggestion}"))
    }

    fn unknown_command(&mut self, chain: &[&Cmd], name: &str) -> Stop {
        let command = *chain.last().expect("non-empty chain");
        let mut candidates = Vec::new();
        for sub in &command.commands {
            candidates.push(sub.name.clone());
            if let Some(alias) = sub.aliases.first() {
                candidates.push(alias.clone());
            }
        }
        if command.help_command {
            candidates.push("help".into());
        }
        let suggestion = suggest_similar(name, &candidates);
        self.stop(
            chain,
            format!("error: unknown command '{name}'{suggestion}"),
        )
    }

    /// Commander's _parseCommand for one level of the tree.
    fn parse_command<'c>(
        &mut self,
        chain: &mut Vec<&'c Cmd>,
        operands: Vec<String>,
        unknown: Vec<String>,
    ) -> Result<Invocation, Stop> {
        let command: &'c Cmd = chain.last().copied().expect("non-empty chain");
        let is_program = chain.len() == 1;
        let mut values = if is_program {
            std::mem::take(&mut self.globals)
        } else {
            defaults(command)
        };
        let parsed = self.parse_options(chain, &unknown, &mut values);
        if is_program {
            self.globals = values.clone();
        }
        let (parsed_operands, parsed_unknown) = parsed?;
        let mut operands = operands;
        operands.extend(parsed_operands);
        let unknown = parsed_unknown;
        let all: Vec<String> = operands.iter().chain(unknown.iter()).cloned().collect();

        if let Some(sub) = operands.first().and_then(|name| command.find_command(name)) {
            chain.push(sub);
            return self.parse_command(chain, operands[1..].to_vec(), unknown);
        }
        if command.help_command && operands.first().is_some_and(|name| name == "help") {
            let Some(target) = operands.get(1) else {
                return Err(self.help(chain, false));
            };
            if let Some(sub) = command.find_command(target) {
                chain.push(sub);
                return Err(self.help(chain, false));
            }
            return Err(self.help(chain, true));
        }
        if !command.commands.is_empty() && all.is_empty() && !command.has_action {
            return Err(self.help(chain, true));
        }
        if unknown.iter().any(|arg| HELP_FLAGS.contains(&arg.as_str())) {
            return Err(self.help(chain, false));
        }

        if command.has_action {
            if let Some(flag) = unknown.first() {
                return Err(self.unknown_option(chain, &flag.clone()));
            }
            for (position, argument) in command.arguments.iter().enumerate() {
                if argument.required && all.get(position).is_none() {
                    let message = format!("error: missing required argument '{}'", argument.name);
                    return Err(self.stop(chain, message));
                }
            }
            let variadic_last = command
                .arguments
                .last()
                .is_some_and(|argument| argument.variadic);
            if !variadic_last && all.len() > command.arguments.len() {
                let expected = command.arguments.len();
                let message = format!(
                    "error: too many arguments for '{}'. Expected {expected} argument{} but got {}: {}.",
                    command.name,
                    if expected == 1 { "" } else { "s" },
                    all.len(),
                    all.join(", ")
                );
                return Err(self.stop(chain, message));
            }
            let arguments = command
                .arguments
                .iter()
                .enumerate()
                .map(|(position, argument)| {
                    if argument.variadic {
                        ArgValue::Many(
                            all.get(position..)
                                .map(<[String]>::to_vec)
                                .unwrap_or_default(),
                        )
                    } else {
                        ArgValue::One(all.get(position).cloned())
                    }
                })
                .collect();
            return Ok(Invocation {
                path: chain[1..]
                    .iter()
                    .map(|command| command.name.clone())
                    .collect(),
                arguments,
                options: values,
                globals: self.globals.clone(),
            });
        }
        if let Some(name) = operands.first() {
            return Err(self.unknown_command(chain, &name.clone()));
        }
        if let Some(flag) = unknown.first() {
            return Err(self.unknown_option(chain, &flag.clone()));
        }
        Err(self.help(chain, true))
    }
}

/// Parses argv (without the program name) against the program's tree.
pub fn parse(
    program: &Cmd,
    version: &str,
    argv: &[String],
    output: &mut Output,
) -> Result<Invocation, Stop> {
    let mut parser = Parser {
        version,
        output,
        globals: defaults(program),
    };
    let mut chain = vec![program];
    parser.parse_command(&mut chain, Vec::new(), argv.to_vec())
}

/// The program's help, as printed for a bare invocation.
pub fn program_help(program: &Cmd) -> String {
    help_for(render_help(&[program]), false)
}

/// Wraps at whitespace, keeping existing line breaks (Commander's boxWrap).
fn box_wrap(text: &str, width: usize) -> String {
    if width < 40 {
        return text.to_owned();
    }
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut seen_word = false;
        for character in raw.chars() {
            if character.is_whitespace() && seen_word {
                chunks.push(std::mem::take(&mut current));
                seen_word = false;
            }
            if !character.is_whitespace() {
                seen_word = true;
            }
            current.push(character);
        }
        if seen_word {
            chunks.push(current);
        }
        let mut chunks = chunks.into_iter();
        let Some(first) = chunks.next() else {
            lines.push(String::new());
            continue;
        };
        let mut line = first;
        let mut line_width = display_width(&line);
        for chunk in chunks {
            let chunk_width = display_width(&chunk);
            if line_width + chunk_width <= width {
                line.push_str(&chunk);
                line_width += chunk_width;
            } else {
                lines.push(std::mem::take(&mut line));
                line = chunk.trim_start().to_owned();
                line_width = display_width(&line);
            }
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Characters on screen, ignoring ANSI color codes.
fn display_width(text: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for character in text.chars() {
        if in_escape {
            in_escape = !character.is_ascii_alphabetic();
        } else if character == '\x1b' {
            in_escape = true;
        } else {
            width += 1;
        }
    }
    width
}

fn format_item(term: &str, term_width: usize, description: &str) -> String {
    if description.is_empty() {
        return format!("  {term}");
    }
    let padded = format!(
        "{term}{}",
        " ".repeat(term_width.saturating_sub(display_width(term)))
    );
    let remaining = 80_usize.saturating_sub(term_width + 4);
    let preformatted = description.contains("\n ") || description.contains("\n\t");
    let description = if remaining < 40 || preformatted {
        description.to_owned()
    } else {
        box_wrap(description, remaining).replace('\n', &format!("\n{}", " ".repeat(term_width + 2)))
    };
    format!("  {padded}  {}", description.replace('\n', "\n  "))
}

struct HelpItem {
    term: String,
    description: String,
    group: String,
}

/// Commander's formatHelp plus the command's before and after text.
fn render_help(chain: &[&Cmd]) -> String {
    let command = *chain.last().expect("non-empty chain");
    let ancestors: Vec<&str> = chain[..chain.len() - 1]
        .iter()
        .map(|cmd| cmd.name.as_str())
        .collect();
    let mut name = command.name.clone();
    if let Some(alias) = command.aliases.first() {
        name = format!("{name}|{alias}");
    }
    let mut usage_parts = vec!["[options]".to_owned()];
    if !command.commands.is_empty() {
        usage_parts.push("[command]".into());
    }
    usage_parts.extend(command.arguments.iter().map(Arg::human));
    let mut prefix = ancestors.join(" ");
    if !prefix.is_empty() {
        prefix.push(' ');
    }

    let arguments: Vec<HelpItem> = if command
        .arguments
        .iter()
        .any(|argument| argument.description.is_some())
    {
        command
            .arguments
            .iter()
            .map(|argument| HelpItem {
                term: argument.name.clone(),
                description: argument.description.clone().unwrap_or_default(),
                group: "Arguments:".into(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut options: Vec<HelpItem> = command
        .options
        .iter()
        .map(|option| HelpItem {
            term: option.flags.clone(),
            description: option.description.clone(),
            group: "Options:".into(),
        })
        .collect();
    options.push(HelpItem {
        term: "-h, --help".into(),
        description: "display help for command".into(),
        group: "Options:".into(),
    });
    let mut commands: Vec<HelpItem> = command
        .commands
        .iter()
        .map(|sub| HelpItem {
            term: sub.name.clone(),
            description: sub.summary.clone(),
            group: sub.help_group.clone().unwrap_or_else(|| "Commands:".into()),
        })
        .collect();
    if command.help_command {
        commands.push(HelpItem {
            term: "help".into(),
            description: "display help for command".into(),
            group: "Commands:".into(),
        });
    }
    let term_width = arguments
        .iter()
        .chain(&options)
        .chain(&commands)
        .map(|item| display_width(&item.term))
        .max()
        .unwrap_or(0);

    let mut lines = vec![
        format!("Usage: {prefix}{name} {}", usage_parts.join(" ")),
        String::new(),
    ];
    if !command.hide_description && !command.summary.is_empty() {
        lines.push(box_wrap(&command.summary, 80));
        lines.push(String::new());
    }
    for items in [arguments, options, commands] {
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for item in items {
            let rendered = format_item(&item.term, term_width, &item.description);
            match groups.iter_mut().find(|(group, _)| *group == item.group) {
                Some((_, entries)) => entries.push(rendered),
                None => groups.push((item.group, vec![rendered])),
            }
        }
        for (group, entries) in groups {
            lines.push(group);
            lines.extend(entries);
            lines.push(String::new());
        }
    }
    let mut help = String::new();
    if let Some(before) = &command.before_help {
        help.push_str(before);
        help.push('\n');
    }
    help.push_str(&lines.join("\n"));
    if let Some(after) = command.after_help.as_ref().filter(|text| !text.is_empty()) {
        help.push_str(after);
        help.push('\n');
    }
    help
}
