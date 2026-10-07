//! The command catalog (catalog.json), in the shape `schema --json`
//! prints. It is the single description of every command: `schema` and `describe`
//! print it, and the command-line parser is built from it, so what the
//! catalog promises and what the parser accepts cannot drift apart.
//!
//! build.rs compiles catalog.json into the statics below, so building the
//! parser parses no JSON; the JSON itself is parsed only to print it.

use std::sync::OnceLock;

use serde_json::Value;

// catalog.json without its whitespace; see catalog() in build.rs.
const CATALOG_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/catalog.json"));

#[derive(Debug)]
pub struct CatalogCommand {
    pub path: &'static [&'static str],
    pub aliases: &'static [&'static str],
    pub module: &'static str,
    pub summary: &'static str,
    pub description: Option<&'static str>,
    pub examples: &'static [&'static str],
    pub arguments: &'static [CatalogArgument],
    pub options: &'static [CatalogOption],
}

#[derive(Debug)]
pub struct CatalogArgument {
    pub name: &'static str,
    pub required: bool,
    pub variadic: bool,
    pub description: Option<&'static str>,
}

#[derive(Debug)]
pub struct CatalogOption {
    pub flags: &'static str,
    pub description: &'static str,
    pub choices: Option<&'static [&'static str]>,
    /// The default as JSON text, typed as the catalog wrote it.
    pub default_json: Option<&'static str>,
}

impl CatalogOption {
    pub fn default_value(&self) -> Option<Value> {
        self.default_json
            .map(|text| serde_json::from_str(text).expect("build.rs wrote valid JSON"))
    }
}

/// Every command, sorted by path as catalog.json lists them.
pub const COMMANDS: &[CatalogCommand] = include!(concat!(env!("OUT_DIR"), "/catalog.rs"));

/// The catalog as JSON, keys in their original order, for printing.
pub fn json() -> &'static Value {
    static JSON: OnceLock<Value> = OnceLock::new();
    JSON.get_or_init(|| serde_json::from_str(CATALOG_JSON).expect("catalog.json is valid JSON"))
}

pub fn find(path: &[&str]) -> Option<&'static CatalogCommand> {
    COMMANDS.iter().find(|command| command.path == path)
}
