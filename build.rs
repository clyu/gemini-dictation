//! Embeds the CI build number in the version string, so that a binary can be traced back to the
//! workflow run (and the artifact) that produced it.

use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");

    let mut version = env::var("CARGO_PKG_VERSION").unwrap();
    if let Ok(run) = env::var("GITHUB_RUN_NUMBER") {
        version = format!("{version}+{run}");
        if let Ok(sha) = env::var("GITHUB_SHA") {
            version = format!("{version} ({})", &sha[..sha.len().min(7)]);
        }
    }
    println!("cargo:rustc-env=GEMINI_DICTATION_VERSION={version}");
}
