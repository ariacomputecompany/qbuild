mod cli;
mod error;
mod platform;
mod protocol;

#[cfg(target_os = "linux")]
mod build;
#[cfg(target_os = "linux")]
mod containers;
#[cfg(target_os = "linux")]
mod guestd;
#[cfg(target_os = "linux")]
mod image;
#[cfg(target_os = "linux")]
mod registry;
#[cfg(target_os = "linux")]
mod runtime;
#[cfg(target_os = "linux")]
mod services;

use clap::Parser;
use cli::Cli;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), error::AppError> {
    let cli = Cli::parse();
    platform::run(cli).await
}

fn default_data_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".qbuild")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn default_store_dir() -> PathBuf {
    default_data_root().join("images")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn default_work_dir() -> PathBuf {
    default_data_root().join("builds")
}
