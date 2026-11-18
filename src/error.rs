//! Errors that map to specific exit codes and machine-readable error codes.

use std::fmt;

#[derive(Debug)]
pub struct CliError {
    pub exit: i32,
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_PARTIAL: i32 = 3;
pub const EXIT_FAILED: i32 = 4;

pub fn usage(message: impl Into<String>) -> anyhow::Error {
    CliError {
        exit: EXIT_USAGE,
        code: "usage",
        message: message.into(),
    }
    .into()
}

pub fn not_found(message: impl Into<String>) -> anyhow::Error {
    CliError {
        exit: EXIT_ERROR,
        code: "not_found",
        message: message.into(),
    }
    .into()
}

pub fn invalid(message: impl Into<String>) -> anyhow::Error {
    CliError {
        exit: EXIT_ERROR,
        code: "invalid_input",
        message: message.into(),
    }
    .into()
}
