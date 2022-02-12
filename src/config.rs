use serde::{Deserialize, Serialize};

mod default {
    pub fn shell() -> String {
        "/bin/sh".to_owned()
    }
}

/// Top level unit file structure
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    /// Human readable description
    ///
    /// It's printed when listing units etc.
    #[serde(default = "String::default")]
    description: String,

    /// Shell used for interpreting shell commands
    ///
    /// An absolute path to any executable which accepts a program on stdin, default is `/bin/sh`.
    #[serde(default = "default::shell")]
    shell: String,

    #[serde(flatten)]
    unit_type: Type,
}

/// Ensures only one type of unit is configured
#[derive(Serialize, Deserialize)]
pub enum Type {
    Service(Service),
    Timer(Timer),
}

/// Ensures only one run variant is configured
#[derive(Serialize, Deserialize)]
pub enum Run {
    /// Execute a file with arguments
    Exec(Vec<String>),

    /// Use the configured shell to execute a script
    Shell(String),
}

/// Service unit
///
/// Starts and maintains a child process
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    #[serde(flatten)]
    run: Run,
}

/// Timer unit
///
/// Runs periodically according to the configuration
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timer {
    #[serde(flatten)]
    run: Run,

    /// Start immediately for the first time, don't wait for the first scheduled time
    on_startup: bool,
}
