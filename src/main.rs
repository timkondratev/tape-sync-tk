use std::env;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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

    for warning in &runtime.report.warnings {
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

    if let Some(status) = runtime.decode_status_snapshot() {
        println!(
            "decode status: lock={:?}, direction={:?}, decoded_frames={}, measured_fps={:?}, smoothed_tempo_bpm={:?}",
            status.lock_status,
            status.direction,
            status.decoded_frame_count,
            status.measured_fps,
            status.smoothed_tempo_bpm
        );
    }

    println!("running {:?} mode; press Ctrl-C to stop", config.mode);
    wait_for_shutdown()?;

    drop(runtime);
    println!("shutdown complete for {:?} mode", config.mode);
    Ok(())
}

fn wait_for_shutdown() -> Result<(), Box<dyn std::error::Error>> {
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&shutdown_requested);

    ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::SeqCst);
    })?;

    while !shutdown_requested.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(200));
    }

    Ok(())
}

