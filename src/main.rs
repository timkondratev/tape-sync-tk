use std::env;
use std::process;

use tape_sync_tk::config::AppConfig;
use tape_sync_tk::startup::{SystemInventory, preflight};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "tape-sync.toml".to_string());

    let config = AppConfig::from_path(&config_path)?;
    let inventory = SystemInventory::gather()?;
    let report = preflight(&config, &inventory)?;

    for warning in report.warnings {
        eprintln!("warning: {warning}");
    }

    println!(
        "startup preflight passed for {:?} mode with total latency {:.2} ms",
        config.mode, report.total_latency_ms
    );
    Ok(())
}
