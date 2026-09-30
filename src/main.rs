use std::env;
use std::io::{self, Write};
use std::process;
use std::time::{Duration, Instant};

use cpal::traits::DeviceTrait;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use tape_sync_tk::audio::AudioRuntime;
use tape_sync_tk::cli::CliArgs;
use tape_sync_tk::config::AppConfig;
use tape_sync_tk::runtime::initialize;
use tape_sync_tk::startup::SystemInventory;
use tape_sync_tk::text_ui::{self, KeyPress, MenuAction, RawModeGuard};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let program_name = env::args()
        .next()
        .unwrap_or_else(|| "tape-sync-tk".to_string());
    let args = CliArgs::parse_from(env::args())?;

    if args.show_help {
        print!("{}", CliArgs::help_text(&program_name));
        return Ok(());
    }

    let mut config = match AppConfig::from_path(&args.config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("warning: {error}; using built-in defaults");
            AppConfig::default()
        }
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    loop {
        let inventory = SystemInventory::gather()?;
        match text_ui::run(&mut output, config, &inventory)? {
            MenuAction::Quit(updated_config) => {
                updated_config.save_to_path(&args.config_path)?;
                execute!(output, Clear(ClearType::All), MoveTo(0, 0))?;
                return Ok(());
            }
            MenuAction::Start(updated_config) => config = updated_config,
        }

        config.save_to_path(&args.config_path)?;
        let runtime = initialize(&config)?;
        let runtime_action = run_mode(&mut output, &config, &runtime)?;
        drop(runtime);

        if runtime_action == RuntimeAction::Quit {
            execute!(output, Clear(ClearType::All), MoveTo(0, 0))?;
            return Ok(());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeAction {
    Back,
    Quit,
}

fn run_mode<W: Write>(
    output: &mut W,
    config: &AppConfig,
    runtime: &tape_sync_tk::runtime::StartupRuntime,
) -> Result<RuntimeAction, Box<dyn std::error::Error>> {
    let _raw_mode = RawModeGuard::acquire()?;
    let mode_summary = match &runtime.audio {
        AudioRuntime::Generate { output } => format!(
            "Output: {} channel {} at {} Hz",
            output.device.name()?,
            output.channel,
            output.sample_rate
        ),
        AudioRuntime::Decode { input } => format!(
            "Input: {} channel {} at {} Hz",
            input.device.name()?,
            input.channel,
            input.sample_rate
        ),
    };
    let mut last_status = None;
    let mut last_draw = Instant::now() - Duration::from_millis(200);

    loop {
        let status = runtime.decode_status_snapshot();
        if last_status != status || last_draw.elapsed() >= Duration::from_secs(1) {
            execute!(output, Clear(ClearType::All), MoveTo(0, 0))?;
            writeln!(output, "Running {:?}\r", config.mode)?;
            writeln!(output, "{mode_summary}\r")?;
            writeln!(
                output,
                "Total latency: {:.2} ms\r",
                runtime.report.total_latency_ms
            )?;
            for warning in &runtime.report.warnings {
                writeln!(output, "Warning: {warning}\r")?;
            }
            if let Some(status) = &status {
                writeln!(output, "{}\r", render_status_line(status))?;
            }
            if let Some(scheduler) = runtime.scheduler_status_snapshot() {
                writeln!(
                    output,
                    "scheduler running={} holdover={} phase_us={} jumps={} dropped={} late={} error={}\r",
                    scheduler.running,
                    scheduler.holdover_active,
                    scheduler.phase_error_micros,
                    scheduler.discontinuity_count,
                    scheduler.dropped_update_count,
                    scheduler.late_tick_count,
                    scheduler.last_error.as_deref().unwrap_or("-")
                )?;
            }
            writeln!(output, "\rEsc/b: back to menu    q/Ctrl-C: quit\r")?;
            output.flush()?;
            last_status = status;
            last_draw = Instant::now();
        }

        match text_ui::poll_key(Duration::from_millis(100))? {
            Some(KeyPress::Back) => return Ok(RuntimeAction::Back),
            Some(KeyPress::Quit) => return Ok(RuntimeAction::Quit),
            Some(KeyPress::Character(character)) if character.eq_ignore_ascii_case(&'b') => {
                return Ok(RuntimeAction::Back);
            }
            Some(KeyPress::Character(character)) if character.eq_ignore_ascii_case(&'q') => {
                return Ok(RuntimeAction::Quit);
            }
            _ => {}
        }
    }
}

fn render_status_line(status: &tape_sync_tk::ltc::DecodeStatus) -> String {
    let fps = status
        .measured_fps
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "-".to_string());
    let bpm = status
        .smoothed_tempo_bpm
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "-".to_string());

    format!(
        "decode lock={:?} dir={:?} frames={} fps={} bpm={} edges={} v={} i={}",
        status.lock_status,
        status.direction,
        status.decoded_frame_count,
        fps,
        bpm,
        status.edge_count,
        status.consecutive_valid_windows,
        status.consecutive_invalid_windows,
    )
}
