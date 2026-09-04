//! Homelinter command-line interface.
//!
//! The binary owns an ephemeral service instance: it binds a loopback
//! listener, builds the service in-process, and drives it through the API
//! (see "Client Lifecycle" in DESIGN.md). Non-interactive modes run to
//! completion and shut the service down; `web` runs until an explicit
//! shutdown.

mod args;
mod driver;
mod output;
mod tui;

use std::process::ExitCode;

use args::{parse, Mode};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("homelinter: {msg}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<u8, String> {
    let mode = parse()?;
    match mode {
        Mode::Managed(opts) => driver::managed(opts).await,
        Mode::Web(opts) => driver::web(opts).await,
        Mode::Tui(opts) => driver::tui(opts).await,
    }
}
