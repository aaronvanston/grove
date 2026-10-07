//! grove: a machine registry with health and capacity at a glance.

// Errors carry their whole envelope (code, message, hint, details) and are
// only built on the way out, so their size never costs anything.
#![allow(clippy::result_large_err)]

mod alerts;
mod catalog;
mod charts;
mod cli;
mod drift;
mod errors;
mod health;
mod hooks;
mod output;
mod ping;
mod policy;
mod probe;
mod reading;
mod script;
mod sha256;
mod store;
mod style;
mod transport;

fn main() {
    // Arguments that aren't valid UTF-8 are read lossily.
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let code = cli::run(&argv);
    std::process::exit(code);
}
