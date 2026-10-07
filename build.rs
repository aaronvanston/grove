// Bakes the git commit into the binary for `version`. An explicit
// GROVE_COMMIT wins; a build outside a git checkout simply has no commit.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Release builds are stripped by the linker rather than by `strip` in the
/// profile: rustc strips macOS binaries with LLVM's objcopy, which leaves
/// chained fixups' string pool misaligned (dyld refuses such dylibs, and
/// `dyld_info` such executables). On Linux this is the flag rustc's own
/// stripping passes.
fn link_args() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let target = |name: &str| std::env::var(format!("CARGO_CFG_TARGET_{name}")).unwrap_or_default();
    match target("OS").as_str() {
        "macos" => {
            println!("cargo:rustc-link-arg-bins=-Wl,-x,-S");
            // Nothing looks symbols up in the executable itself.
            println!("cargo:rustc-link-arg-bins=-Wl,-no_exported_symbols");
            // Chained fixups let dyld slide each pointer when its page is
            // first touched (page-in linking) instead of walking all of them
            // on every launch. Apple Silicon's dyld reads them from macOS 11,
            // the target's minimum; Intel builds keep supporting older systems.
            if target("ARCH") == "aarch64" {
                println!("cargo:rustc-link-arg-bins=-Wl,-fixup_chains");
            }
        }
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,--strip-all"),
        _ => {}
    }
}

/// Writes src/catalog.json into OUT_DIR twice: as JSON without the
/// whitespace between tokens, which `schema` and `describe` print, and as
/// Rust statics the command-line parser is built from, so no launch has to
/// parse JSON just to read argv.
fn catalog() {
    println!("cargo:rerun-if-changed=src/catalog.json");
    let text = std::fs::read_to_string("src/catalog.json").expect("src/catalog.json is readable");
    let catalog: serde_json::Value =
        serde_json::from_str(&text).expect("src/catalog.json is valid JSON");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join("catalog.json"), catalog.to_string()).expect("OUT_DIR is writable");
    std::fs::write(out.join("catalog.rs"), catalog_statics(&catalog)).expect("OUT_DIR is writable");
}

/// The catalog's commands as a `&[CatalogCommand]` expression.
fn catalog_statics(catalog: &serde_json::Value) -> String {
    use serde_json::Value;
    let text = |value: &Value| format!("{:?}", value.as_str().expect("a string"));
    let optional = |value: Option<&Value>| match value.and_then(Value::as_str) {
        Some(value) => format!("Some({value:?})"),
        None => "None".to_owned(),
    };
    let list = |value: &Value| {
        let items: Vec<String> = value.as_array().expect("a list").iter().map(text).collect();
        format!("&[{}]", items.join(", "))
    };
    let flag = |value: &Value, key: &str| value[key].as_bool().expect("a boolean");
    let mut out = String::from("&[\n");
    for command in catalog["commands"].as_array().expect("commands") {
        let arguments: Vec<String> = command["arguments"]
            .as_array()
            .expect("arguments")
            .iter()
            .map(|argument| {
                format!(
                    "CatalogArgument {{ name: {}, required: {}, variadic: {}, description: {} }}",
                    text(&argument["name"]),
                    flag(argument, "required"),
                    flag(argument, "variadic"),
                    optional(argument.get("description")),
                )
            })
            .collect();
        let options: Vec<String> = command["options"]
            .as_array()
            .expect("options")
            .iter()
            .map(|option| {
                let choices = match option.get("choices") {
                    Some(choices) if !choices.is_null() => format!("Some({})", list(choices)),
                    _ => "None".to_owned(),
                };
                // Kept as JSON text: a default may be any JSON value.
                let default = match option.get("defaultValue") {
                    Some(value) if !value.is_null() => format!("Some({:?})", value.to_string()),
                    _ => "None".to_owned(),
                };
                format!(
                    "CatalogOption {{ flags: {}, description: {}, choices: {choices}, default_json: {default} }}",
                    text(&option["flags"]),
                    text(&option["description"]),
                )
            })
            .collect();
        out.push_str(&format!(
            "CatalogCommand {{ path: {}, aliases: {}, module: {}, summary: {}, description: {}, examples: {}, arguments: &[{}], options: &[{}] }},\n",
            list(&command["path"]),
            list(&command["aliases"]),
            text(&command["module"]),
            text(&command["summary"]),
            optional(command.get("description")),
            list(&command["examples"]),
            arguments.join(", "),
            options.join(", "),
        ));
    }
    out.push(']');
    out
}

fn main() {
    link_args();
    catalog();
    println!("cargo:rerun-if-env-changed=GROVE_COMMIT");
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    let commit = std::env::var("GROVE_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]));
    if let Some(commit) = commit {
        println!("cargo:rustc-env=GROVE_COMMIT={commit}");
    }
}
