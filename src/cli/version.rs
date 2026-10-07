//! `version`: the build's name, version, target and commit. Install
//! scripts read `data.version`, so this must always succeed.

use serde_json::{Map, Value};

use super::{Context, Done, NAME, VERSION};

/// Node's names for the CPU, which release archives are named after.
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

/// Node's names for the OS.
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

pub fn run(context: &Context) -> Done {
    let commit = option_env!("GROVE_COMMIT");
    let mut record = Map::new();
    record.insert("arch".into(), Value::from(arch()));
    if let Some(commit) = commit {
        record.insert("commit".into(), Value::from(commit));
    }
    record.insert("name".into(), Value::from(NAME));
    record.insert("platform".into(), Value::from(platform()));
    record.insert("runtime".into(), Value::from("rust"));
    record.insert("version".into(), Value::from(VERSION));
    let ui = &context.ui;
    let mut lines = vec![
        format!("{} {} {VERSION}", ui.brand("◆"), ui.heading(NAME)),
        format!("{}  Rust", ui.muted("runtime")),
        format!("{}   {}-{}", ui.muted("target"), platform(), arch()),
    ];
    if let Some(commit) = commit {
        lines.push(format!("{}   {commit}", ui.muted("commit")));
    }
    Done::new(Value::Object(record), lines.join("\n"))
}
