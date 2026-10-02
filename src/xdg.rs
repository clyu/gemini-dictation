//! Where the files of gemini-dictation are, after the XDG Base Directory Specification.

use std::env;
use std::path::PathBuf;

/// Returns the path of a file in the configuration directory, ~/.config/gemini-dictation.
pub fn config_path(name: &str) -> PathBuf {
    let config = dir("XDG_CONFIG_HOME")
        .or_else(|| dir("HOME").map(|home| home.join(".config")))
        .unwrap_or_default();
    config.join("gemini-dictation").join(name)
}

/// Returns the path of a file in the runtime directory, which lasts as long as the login session.
pub fn runtime_path(name: &str) -> PathBuf {
    let runtime = dir("XDG_RUNTIME_DIR").unwrap_or_else(env::temp_dir);
    runtime.join(name)
}

/// Returns the directory that an environment variable names. An empty variable counts as one
/// that is not set, as the specification says.
fn dir(variable: &str) -> Option<PathBuf> {
    env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
