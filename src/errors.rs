//! The error every command and the parser report through: a stable code,
//! a message, and the process exit code it maps to.

use serde_json::Value;

/// Process exit codes, from sysexits where one fits.
pub mod exit {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const CONFIG: i32 = 78;
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub exit_code: i32,
    pub hint: Option<String>,
    pub details: Option<Value>,
}

impl AppError {
    /// An error that exits 1, the default for anything not a usage problem.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            exit_code: exit::ERROR,
            hint: None,
            details: None,
        }
    }

    pub fn usage(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message).exit(exit::USAGE)
    }

    pub fn exit(mut self, exit_code: i32) -> Self {
        self.exit_code = exit_code;
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

/// SQLite failures surface with SQLite's own message.
impl From<rusqlite::Error> for AppError {
    fn from(error: rusqlite::Error) -> Self {
        let message = match &error {
            rusqlite::Error::SqliteFailure(_, Some(message)) => message.clone(),
            other => other.to_string(),
        };
        AppError::new("unexpected_error", message)
    }
}
