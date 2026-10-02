mod app;
mod audio;
mod cli;
mod gemini;
mod hotkey;
mod ipc;
mod output;
mod xdg;

use std::io::{self, IsTerminal};

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let default = if cli.verbose {
        "info,gemini_dictation=debug"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        // Colors only in a terminal, not in the journal of a desktop launcher.
        .with_ansi(io::stderr().is_terminal())
        .with_target(false)
        .init();
    // Choose the TLS crypto provider explicitly, in case dependencies enable more than one.
    let _ = rustls::crypto::ring::default_provider().install_default();

    match cli.command {
        None => app::run(cli.run).await,
        Some(Command::Ctl { action }) => ipc::send(action).await,
        Some(Command::Devices) => {
            audio::print_devices()?;
            println!();
            hotkey::print_keyboards()
        }
        Some(Command::Keys) => hotkey::print_keys().await,
    }
}
