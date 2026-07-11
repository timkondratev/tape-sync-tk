use std::env;
use std::process;

use cpal::traits::DeviceTrait;
use tape_sync_tk::audio::AudioRuntime;
use tape_sync_tk::cli::CliArgs;
use tape_sync_tk::config::AppConfig;
use tape_sync_tk::runtime::initialize;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let program_name = env::args().next().unwrap_or_else(|| "tape-sync-tk".to_string());
    let args = CliArgs::parse_from(env::args())?;

    if args.show_help {
        print!("{}", CliArgs::help_text(&program_name));
        return Ok(());
    }

    let config = AppConfig::from_path(&args.config_path)?;
    let runtime = initialize(&config)?;

    for warning in runtime.report.warnings {
        eprintln!("warning: {warning}");
    }

    let mode_summary = match &runtime.audio {
        AudioRuntime::Generate { output } => format!(
            "generate mode using output '{}' channel {} at {} Hz",
            output.device.name()?,
            output.channel,
            output.sample_rate
        ),
        AudioRuntime::Decode { input } => format!(
            "decode mode using input '{}' channel {} at {} Hz",
            input.device.name()?,
            input.channel,
            input.sample_rate
        ),
    };

    println!(
        "startup initialization passed for {:?} mode with total latency {:.2} ms; {mode_summary}",
        config.mode, runtime.report.total_latency_ms
    );
    Ok(())
}

