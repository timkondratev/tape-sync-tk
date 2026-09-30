use std::fmt;
use std::io::Write;
use std::time::Duration;

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

use crate::config::{AppConfig, Fps, Mode, TimingEngine};
use crate::startup::{AudioDeviceInfo, SystemInventory};

#[derive(Debug, Clone)]
pub enum MenuAction {
    Start(AppConfig),
    Quit(AppConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    Character(char),
    Up,
    Down,
    Enter,
    Backspace,
    Back,
    Quit,
}

pub struct RawModeGuard;

impl RawModeGuard {
    pub fn acquire() -> Result<Self, TextUiError> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

pub fn poll_key(timeout: Duration) -> Result<Option<KeyPress>, TextUiError> {
    if !event::poll(timeout)? {
        return Ok(None);
    }

    loop {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => return Ok(Some(map_key(key))),
            _ if !event::poll(Duration::ZERO)? => return Ok(None),
            _ => {}
        }
    }
}

fn read_key() -> Result<KeyPress, TextUiError> {
    loop {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                return Ok(map_key(key));
            }
        }
    }
}

fn map_key(key: KeyEvent) -> KeyPress {
    match key.code {
        KeyCode::Up => KeyPress::Up,
        KeyCode::Down => KeyPress::Down,
        KeyCode::Enter => KeyPress::Enter,
        KeyCode::Backspace => KeyPress::Backspace,
        KeyCode::Esc => KeyPress::Back,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyPress::Quit,
        KeyCode::Char(character) => KeyPress::Character(character),
        _ => KeyPress::Character('\0'),
    }
}

pub fn run<W: Write>(
    output: &mut W,
    config: AppConfig,
    inventory: &SystemInventory,
) -> Result<MenuAction, TextUiError> {
    let _raw_mode = RawModeGuard::acquire()?;
    run_with_keys(&mut TerminalKeys, output, config, inventory)
}

trait KeySource {
    fn read_key(&mut self) -> Result<KeyPress, TextUiError>;
}

struct TerminalKeys;

impl KeySource for TerminalKeys {
    fn read_key(&mut self) -> Result<KeyPress, TextUiError> {
        read_key()
    }
}

fn run_with_keys<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    mut config: AppConfig,
    inventory: &SystemInventory,
) -> Result<MenuAction, TextUiError> {
    retain_or_default_routes(&mut config, inventory);

    loop {
        let details = current_settings(&config, inventory);
        match choose(
            keys,
            output,
            "Tape Sync TK",
            &details,
            &["Generate", "Decode", "Settings", "Quit"],
            0,
        )? {
            Choice::Selected(0) => {
                ensure_route_available(&config, inventory, AudioDirection::Output)?;
                config.mode = Mode::Generate;
                return Ok(MenuAction::Start(config));
            }
            Choice::Selected(1) => {
                ensure_route_available(&config, inventory, AudioDirection::Input)?;
                config.mode = Mode::Decode;
                return Ok(MenuAction::Start(config));
            }
            Choice::Selected(2) => {
                if edit_settings(keys, output, &mut config, inventory)? == Flow::Quit {
                    return Ok(MenuAction::Quit(config));
                }
            }
            Choice::Selected(3) | Choice::Back | Choice::Quit => {
                return Ok(MenuAction::Quit(config));
            }
            Choice::Selected(_) => unreachable!(),
        }
    }
}

fn edit_settings<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    config: &mut AppConfig,
    inventory: &SystemInventory,
) -> Result<Flow, TextUiError> {
    const HELP: [&str; 21] = [
        "Audio device and zero-based channel used to decode LTC from tape.",
        "Audio device and zero-based channel used to generate LTC for tape.",
        "Audio stream sample rate. Both generator and decoder support 44100 or 48000 Hz.",
        "SMPTE start position for generated LTC in HH:MM:SS:FF form.",
        "Nominal LTC frame rate: 24, 25, 29.97 non-drop-frame, or 30 fps.",
        "Project tempo at the reference tape speed. This anchors MIDI clock tempo recovery.",
        "LTC rate at which the reference BPM was established.",
        "Tempo low-pass smoothing from 0.01 to 1.0. Lower values smooth more slowly.",
        "Decoded LTC frames used for the measured frame-rate sliding window (1 to 120).",
        "Invalid decode windows before timing metrics reset (1 to 120).",
        "Hardened uses the independent clock scheduler. Legacy frame clock is a temporary rollback path for field comparison.",
        "Known audio output latency in milliseconds (-500 to 500).",
        "Known tape record/playback path latency in milliseconds (-500 to 500).",
        "Estimated LTC decoder latency in milliseconds (-500 to 500).",
        "Final manual timing adjustment in milliseconds (-2000 to 2000).",
        "Name of the virtual MIDI output exposed to the DAW or MIDI applications.",
        "Enable MIDI Time Code output. MTC generation is currently reserved for future use.",
        "Emit 24 PPQN MIDI clock while Decode is locked and moving forward.",
        "Emit MIDI Start, Stop, and Song Position Pointer during Decode.",
        "Reset every persisted setting to built-in defaults and select available audio routes.",
        "Return to the main mode selection screen.",
    ];

    loop {
        let items = vec![
            setting_row(
                "Input",
                &route_summary(config, inventory, AudioDirection::Input),
            ),
            setting_row(
                "Output",
                &route_summary(config, inventory, AudioDirection::Output),
            ),
            setting_row("Sample rate", &format!("{} Hz", config.audio.sample_rate)),
            setting_row("Timecode start", &config.timecode.start),
            setting_row("LTC FPS", &config.timecode.ltc_fps.as_f64().to_string()),
            setting_row("Reference BPM", &config.tempo.ref_bpm.to_string()),
            setting_row("Reference FPS", &config.tempo.ref_fps.as_f64().to_string()),
            setting_row(
                "Tempo smoothing alpha",
                &config.tempo.smoothing_alpha.to_string(),
            ),
            setting_row(
                "FPS estimate window",
                &format!("{} frames", config.decode.fps_estimate_window_frames),
            ),
            setting_row(
                "Dropout reset",
                &format!("{} windows", config.decode.dropout_reset_windows),
            ),
            setting_row("Timing engine", config.decode.timing_engine.as_str()),
            setting_row(
                "Audio output latency",
                &format!("{} ms", config.latency_ms.audio_output),
            ),
            setting_row(
                "Tape path latency",
                &format!("{} ms", config.latency_ms.tape_path),
            ),
            setting_row(
                "Decoder latency",
                &format!("{} ms", config.latency_ms.decoder),
            ),
            setting_row(
                "Manual latency",
                &format!("{} ms", config.latency_ms.manual),
            ),
            setting_row("MIDI port name", &config.midi.port_name),
            setting_row("Send MTC", enabled(config.midi.send_mtc)),
            setting_row("Send MIDI clock", enabled(config.midi.send_clock)),
            setting_row("Send MIDI transport", enabled(config.midi.send_transport)),
            "Load defaults".to_string(),
            "Back".to_string(),
        ];
        let item_refs: Vec<&str> = items.iter().map(String::as_str).collect();

        match choose(
            keys,
            output,
            "Settings",
            "Changes are saved when you start a mode or quit.",
            &item_refs,
            0,
        )? {
            Choice::Selected(0) => {
                if select_audio_route(
                    keys,
                    output,
                    config,
                    inventory,
                    AudioDirection::Input,
                    HELP[0],
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(1) => {
                if select_audio_route(
                    keys,
                    output,
                    config,
                    inventory,
                    AudioDirection::Output,
                    HELP[1],
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(2) => {
                let current = usize::from(config.audio.sample_rate == 48_000);
                match select_fixed(
                    keys,
                    output,
                    "Sample Rate",
                    &["44100 Hz", "48000 Hz"],
                    current,
                    HELP[2],
                )? {
                    FixedChoice::Selected(0) => config.audio.sample_rate = 44_100,
                    FixedChoice::Selected(1) => config.audio.sample_rate = 48_000,
                    FixedChoice::Back => {}
                    FixedChoice::Quit => return Ok(Flow::Quit),
                    FixedChoice::Selected(_) => unreachable!(),
                }
            }
            Choice::Selected(3) => {
                let current = config.timecode.start.clone();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Timecode Start",
                    HELP[3],
                    &current,
                    |candidate, raw| {
                        candidate.timecode.start = raw.to_string();
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(4) => {
                if edit_fps(
                    keys,
                    output,
                    "LTC FPS",
                    HELP[4],
                    &mut config.timecode.ltc_fps,
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(5) => {
                let current = config.tempo.ref_bpm.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Reference BPM",
                    HELP[5],
                    &current,
                    |candidate, raw| {
                        candidate.tempo.ref_bpm = parse_value(raw, "reference BPM")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(6) => {
                if edit_fps(
                    keys,
                    output,
                    "Reference FPS",
                    HELP[6],
                    &mut config.tempo.ref_fps,
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(7) => {
                let current = config.tempo.smoothing_alpha.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Smoothing Alpha",
                    HELP[7],
                    &current,
                    |candidate, raw| {
                        candidate.tempo.smoothing_alpha = parse_value(raw, "smoothing alpha")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(8) => {
                let current = config.decode.fps_estimate_window_frames.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "FPS Estimate Window",
                    HELP[8],
                    &current,
                    |candidate, raw| {
                        candidate.decode.fps_estimate_window_frames =
                            parse_value(raw, "frame window")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(9) => {
                let current = config.decode.dropout_reset_windows.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Dropout Reset Windows",
                    HELP[9],
                    &current,
                    |candidate, raw| {
                        candidate.decode.dropout_reset_windows =
                            parse_value(raw, "dropout windows")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(10) => {
                let current = usize::from(matches!(
                    config.decode.timing_engine,
                    TimingEngine::LegacyFrameClock
                ));
                match select_fixed(
                    keys,
                    output,
                    "Timing Engine",
                    &["Hardened scheduler", "Legacy frame clock"],
                    current,
                    HELP[10],
                )? {
                    FixedChoice::Selected(0) => {
                        config.decode.timing_engine = TimingEngine::Hardened
                    }
                    FixedChoice::Selected(1) => {
                        config.decode.timing_engine = TimingEngine::LegacyFrameClock
                    }
                    FixedChoice::Back => {}
                    FixedChoice::Quit => return Ok(Flow::Quit),
                    FixedChoice::Selected(_) => unreachable!(),
                }
            }
            Choice::Selected(11) => {
                let current = config.latency_ms.audio_output.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Audio Output Latency",
                    HELP[11],
                    &current,
                    |candidate, raw| {
                        candidate.latency_ms.audio_output =
                            parse_value(raw, "audio output latency")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(12) => {
                let current = config.latency_ms.tape_path.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Tape Path Latency",
                    HELP[12],
                    &current,
                    |candidate, raw| {
                        candidate.latency_ms.tape_path = parse_value(raw, "tape path latency")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(13) => {
                let current = config.latency_ms.decoder.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Decoder Latency",
                    HELP[13],
                    &current,
                    |candidate, raw| {
                        candidate.latency_ms.decoder = parse_value(raw, "decoder latency")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(14) => {
                let current = config.latency_ms.manual.to_string();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "Manual Latency",
                    HELP[14],
                    &current,
                    |candidate, raw| {
                        candidate.latency_ms.manual = parse_value(raw, "manual latency")?;
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(15) => {
                let current = config.midi.port_name.clone();
                if edit_validated(
                    keys,
                    output,
                    config,
                    "MIDI Port Name",
                    HELP[15],
                    &current,
                    |candidate, raw| {
                        candidate.midi.port_name = raw.to_string();
                        Ok(())
                    },
                )? == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(16) => {
                match edit_bool(keys, output, "Send MTC", HELP[16], config.midi.send_mtc)? {
                    BoolChoice::Value(value) => config.midi.send_mtc = value,
                    BoolChoice::Back => {}
                    BoolChoice::Quit => return Ok(Flow::Quit),
                }
            }
            Choice::Selected(17) => {
                match edit_bool(
                    keys,
                    output,
                    "Send MIDI Clock",
                    HELP[17],
                    config.midi.send_clock,
                )? {
                    BoolChoice::Value(value) => config.midi.send_clock = value,
                    BoolChoice::Back => {}
                    BoolChoice::Quit => return Ok(Flow::Quit),
                }
            }
            Choice::Selected(18) => match edit_bool(
                keys,
                output,
                "Send MIDI Transport",
                HELP[18],
                config.midi.send_transport,
            )? {
                BoolChoice::Value(value) => config.midi.send_transport = value,
                BoolChoice::Back => {}
                BoolChoice::Quit => return Ok(Flow::Quit),
            },
            Choice::Selected(19) => {
                match select_fixed(
                    keys,
                    output,
                    "Load Defaults",
                    &["Cancel", "Load defaults"],
                    0,
                    HELP[19],
                )? {
                    FixedChoice::Selected(1) => {
                        let mode = config.mode.clone();
                        *config = AppConfig::default();
                        config.mode = mode;
                        retain_or_default_routes(config, inventory);
                    }
                    FixedChoice::Quit => return Ok(Flow::Quit),
                    FixedChoice::Selected(0) | FixedChoice::Back => {}
                    FixedChoice::Selected(_) => unreachable!(),
                }
            }
            Choice::Selected(20) | Choice::Back => return Ok(Flow::Back),
            Choice::Quit => return Ok(Flow::Quit),
            Choice::Selected(_) => unreachable!(),
        }
    }
}

fn setting_row(name: &str, value: &str) -> String {
    const NAME_COLUMN_WIDTH: usize = 24;
    let leader_length = NAME_COLUMN_WIDTH
        .saturating_sub(name.chars().count())
        .max(3);
    format!("{name} {} {value}", ".".repeat(leader_length))
}

fn enabled(value: bool) -> &'static str {
    if value { "enabled" } else { "disabled" }
}

fn parse_value<T>(raw: &str, label: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    raw.trim()
        .parse()
        .map_err(|_| format!("{label} must be a valid number"))
}

fn edit_validated<K, W, F>(
    keys: &mut K,
    output: &mut W,
    config: &mut AppConfig,
    title: &str,
    help: &str,
    initial: &str,
    apply: F,
) -> Result<Flow, TextUiError>
where
    K: KeySource,
    W: Write,
    F: Fn(&mut AppConfig, &str) -> Result<(), String>,
{
    let mut value = initial.to_string();
    let mut error = None;
    loop {
        match edit_text(keys, output, title, help, value, error.as_deref())? {
            TextEdit::Value(updated) => {
                value = updated;
                let mut candidate = config.clone();
                error = match apply(&mut candidate, &value) {
                    Ok(()) => candidate.validate().err().map(|error| error.to_string()),
                    Err(message) => Some(message),
                };
                if error.is_none() {
                    *config = candidate;
                    return Ok(Flow::Back);
                }
            }
            TextEdit::Back => return Ok(Flow::Back),
            TextEdit::Quit => return Ok(Flow::Quit),
        }
    }
}

fn edit_text<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    title: &str,
    help: &str,
    mut value: String,
    error: Option<&str>,
) -> Result<TextEdit, TextUiError> {
    loop {
        execute!(output, Clear(ClearType::All), MoveTo(0, 0))?;
        writeln!(output, "{title}\r")?;
        writeln!(output, "{help}\r")?;
        writeln!(output, "Type a value, Enter to save, Esc to cancel.\r")?;
        if let Some(error) = error {
            writeln!(output, "Error: {error}\r")?;
        }
        writeln!(output, "\r> {value}_\r")?;
        output.flush()?;

        match keys.read_key()? {
            KeyPress::Character(character) if !character.is_control() => value.push(character),
            KeyPress::Backspace => {
                value.pop();
            }
            KeyPress::Enter => return Ok(TextEdit::Value(value)),
            KeyPress::Back => return Ok(TextEdit::Back),
            KeyPress::Quit => return Ok(TextEdit::Quit),
            _ => {}
        }
    }
}

fn edit_fps<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    title: &str,
    help: &str,
    fps: &mut Fps,
) -> Result<Flow, TextUiError> {
    let values = [Fps::Fps24, Fps::Fps25, Fps::Fps29_97, Fps::Fps30];
    let selected = values.iter().position(|value| value == fps).unwrap_or(3);
    match select_fixed(
        keys,
        output,
        title,
        &["24", "25", "29.97", "30"],
        selected,
        help,
    )? {
        FixedChoice::Selected(index) => *fps = values[index],
        FixedChoice::Back => return Ok(Flow::Back),
        FixedChoice::Quit => return Ok(Flow::Quit),
    }
    Ok(Flow::Back)
}

fn edit_bool<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    title: &str,
    help: &str,
    value: bool,
) -> Result<BoolChoice, TextUiError> {
    match select_fixed(
        keys,
        output,
        title,
        &["Disabled", "Enabled"],
        usize::from(value),
        help,
    )? {
        FixedChoice::Selected(index) => Ok(BoolChoice::Value(index == 1)),
        FixedChoice::Back => Ok(BoolChoice::Back),
        FixedChoice::Quit => Ok(BoolChoice::Quit),
    }
}

fn select_fixed<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    title: &str,
    values: &[&str],
    selected: usize,
    help: &str,
) -> Result<FixedChoice, TextUiError> {
    let details = format!("{help}\n\nEsc/b returns to Settings.");
    match choose(keys, output, title, &details, values, selected)? {
        Choice::Selected(index) => Ok(FixedChoice::Selected(index)),
        Choice::Back => Ok(FixedChoice::Back),
        Choice::Quit => Ok(FixedChoice::Quit),
    }
}

enum TextEdit {
    Value(String),
    Back,
    Quit,
}

enum FixedChoice {
    Selected(usize),
    Back,
    Quit,
}

enum BoolChoice {
    Value(bool),
    Back,
    Quit,
}

#[derive(Clone, Copy)]
enum AudioDirection {
    Input,
    Output,
}

impl AudioDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }

    fn channels(self, device: &AudioDeviceInfo) -> u16 {
        match self {
            Self::Input => device.input_channels,
            Self::Output => device.output_channels,
        }
    }

    fn route<'a>(self, config: &'a AppConfig) -> (&'a str, u16) {
        match self {
            Self::Input => (&config.audio.input_device, config.audio.input_channel),
            Self::Output => (&config.audio.output_device, config.audio.output_channel),
        }
    }
}

fn retain_or_default_routes(config: &mut AppConfig, inventory: &SystemInventory) {
    for direction in [AudioDirection::Input, AudioDirection::Output] {
        if route_is_available(config, inventory, direction) {
            continue;
        }
        let device = match inventory
            .audio_devices
            .iter()
            .find(|device| direction.channels(device) > 0)
        {
            Some(device) => device,
            None => continue,
        };
        match direction {
            AudioDirection::Input => {
                config.audio.input_device = device.name.clone();
                config.audio.input_channel = 0;
            }
            AudioDirection::Output => {
                config.audio.output_device = device.name.clone();
                config.audio.output_channel = 0;
            }
        }
    }
}

fn route_is_available(
    config: &AppConfig,
    inventory: &SystemInventory,
    direction: AudioDirection,
) -> bool {
    let (name, channel) = direction.route(config);
    match inventory
        .audio_devices
        .iter()
        .find(|device| device.name == name)
    {
        Some(device) => channel < direction.channels(device),
        None => false,
    }
}

fn ensure_route_available(
    config: &AppConfig,
    inventory: &SystemInventory,
    direction: AudioDirection,
) -> Result<(), TextUiError> {
    if route_is_available(config, inventory, direction) {
        Ok(())
    } else {
        Err(TextUiError::NoAudioDevices(direction.label()))
    }
}

fn current_settings(config: &AppConfig, inventory: &SystemInventory) -> String {
    let input = route_summary(config, inventory, AudioDirection::Input);
    let output = route_summary(config, inventory, AudioDirection::Output);
    return format!(
        "Current settings\nInput: {input}\nOutput: {output}\nSample rate: {sample_rate} Hz\n\nUse arrows + Enter or press a number. Esc/b goes back; q quits.",
        sample_rate = config.audio.sample_rate,
    );
}

fn route_summary(
    config: &AppConfig,
    inventory: &SystemInventory,
    direction: AudioDirection,
) -> String {
    let (name, channel) = direction.route(config);
    if route_is_available(config, inventory, direction) {
        return format!("{}, channel {}", name, channel);
    }
    "not available".to_string()
}

fn select_audio_route<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    config: &mut AppConfig,
    inventory: &SystemInventory,
    direction: AudioDirection,
    help: &str,
) -> Result<Flow, TextUiError> {
    let devices: Vec<&AudioDeviceInfo> = inventory
        .audio_devices
        .iter()
        .filter(|device| direction.channels(device) > 0)
        .collect();
    if devices.is_empty() {
        return Err(TextUiError::NoAudioDevices(direction.label()));
    }

    let names: Vec<&str> = devices.iter().map(|device| device.name.as_str()).collect();
    let current_name = direction.route(config).0;
    let selected = devices
        .iter()
        .position(|device| device.name == current_name)
        .unwrap_or(0);
    let device_help = format!("{help}\n\nEsc/b returns to Settings.");
    let device_index = match choose(
        keys,
        output,
        &format!("Select {} Device", direction.label()),
        &device_help,
        &names,
        selected,
    )? {
        Choice::Selected(index) => index,
        Choice::Back => return Ok(Flow::Back),
        Choice::Quit => return Ok(Flow::Quit),
    };
    let device = devices[device_index];
    let channels: Vec<String> = (0..direction.channels(device))
        .map(|channel| format!("Channel {channel}"))
        .collect();
    let channel_refs: Vec<&str> = channels.iter().map(String::as_str).collect();
    let selected_channel = (direction.route(config).1 as usize).min(channel_refs.len() - 1);
    let channel_help = format!("{help}\n\nEsc/b returns to device selection.");
    let channel = match choose(
        keys,
        output,
        &format!("Select {} Channel", direction.label()),
        &channel_help,
        &channel_refs,
        selected_channel,
    )? {
        Choice::Selected(index) => index as u16,
        Choice::Back => {
            return select_audio_route(keys, output, config, inventory, direction, help);
        }
        Choice::Quit => return Ok(Flow::Quit),
    };

    match direction {
        AudioDirection::Input => {
            config.audio.input_device = device.name.clone();
            config.audio.input_channel = channel;
        }
        AudioDirection::Output => {
            config.audio.output_device = device.name.clone();
            config.audio.output_channel = channel;
        }
    }
    Ok(Flow::Back)
}

fn choose<K: KeySource, W: Write>(
    keys: &mut K,
    output: &mut W,
    title: &str,
    details: &str,
    items: &[&str],
    initial: usize,
) -> Result<Choice, TextUiError> {
    let mut selected = initial.min(items.len() - 1);
    let number_width = items.len().to_string().len();
    loop {
        execute!(output, Clear(ClearType::All), MoveTo(0, 0))?;
        writeln!(output, "{title}\r")?;
        for line in details.lines() {
            writeln!(output, "{line}\r")?;
        }
        for (index, item) in items.iter().enumerate() {
            let marker = if index == selected { ">" } else { " " };
            writeln!(output, "{marker} {:>number_width$}. {item}\r", index + 1)?;
        }
        output.flush()?;

        match keys.read_key()? {
            KeyPress::Up => selected = selected.checked_sub(1).unwrap_or(items.len() - 1),
            KeyPress::Down => selected = (selected + 1) % items.len(),
            KeyPress::Enter => return Ok(Choice::Selected(selected)),
            KeyPress::Character(character) if character.eq_ignore_ascii_case(&'b') => {
                return Ok(Choice::Back);
            }
            KeyPress::Character(character) if character.eq_ignore_ascii_case(&'q') => {
                return Ok(Choice::Quit);
            }
            KeyPress::Character(character) if character.is_ascii_digit() => {
                let index = character.to_digit(10).unwrap_or(0) as usize;
                if (1..=items.len()).contains(&index) {
                    return Ok(Choice::Selected(index - 1));
                }
            }
            KeyPress::Back => return Ok(Choice::Back),
            KeyPress::Quit => return Ok(Choice::Quit),
            KeyPress::Character(_) | KeyPress::Backspace => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Selected(usize),
    Back,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Back,
    Quit,
}

#[derive(Debug)]
pub enum TextUiError {
    Io(std::io::Error),
    NoAudioDevices(&'static str),
}

impl fmt::Display for TextUiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "text UI failed: {source}"),
            Self::NoAudioDevices(direction) => write!(
                formatter,
                "no audio {direction} route is available; connect a device or choose one in Settings"
            ),
        }
    }
}

impl std::error::Error for TextUiError {}

impl From<std::io::Error> for TextUiError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct ScriptedKeys(VecDeque<KeyPress>);

    impl ScriptedKeys {
        fn new(keys: impl IntoIterator<Item = KeyPress>) -> Self {
            Self(keys.into_iter().collect())
        }
    }

    impl KeySource for ScriptedKeys {
        fn read_key(&mut self) -> Result<KeyPress, TextUiError> {
            Ok(self.0.pop_front().expect("scripted key input exhausted"))
        }
    }

    fn config() -> AppConfig {
        let mut config = AppConfig::default();
        config.audio.input_device = "Input Only".to_string();
        config.audio.input_channel = 1;
        config.audio.output_device = "Output Only".to_string();
        config.audio.output_channel = 1;
        config
    }

    fn inventory() -> SystemInventory {
        SystemInventory {
            audio_devices: vec![
                AudioDeviceInfo {
                    name: "Input Only".to_string(),
                    input_channels: 2,
                    output_channels: 0,
                },
                AudioDeviceInfo {
                    name: "Output Only".to_string(),
                    input_channels: 0,
                    output_channels: 2,
                },
            ],
            midi: crate::startup::MidiEnvironment {
                backend_available: true,
            },
        }
    }

    fn started_config(action: MenuAction) -> AppConfig {
        match action {
            MenuAction::Start(config) => config,
            MenuAction::Quit(_) => panic!("expected start action"),
        }
    }

    #[test]
    fn generate_starts_immediately_with_saved_output_route() {
        let mut keys = ScriptedKeys::new([KeyPress::Character('1')]);
        let mut output = Vec::new();

        let action = run_with_keys(&mut keys, &mut output, config(), &inventory())
            .expect("menu should complete");
        let config = started_config(action);

        assert_eq!(config.mode, Mode::Generate);
        assert_eq!(config.audio.output_device, "Output Only");
        assert_eq!(config.audio.output_channel, 1);
    }

    #[test]
    fn startup_draws_current_settings() {
        let mut keys = ScriptedKeys::new([KeyPress::Quit]);
        let mut output = Vec::new();

        run_with_keys(&mut keys, &mut output, config(), &inventory())
            .expect("menu should complete");
        let rendered = String::from_utf8(output).expect("output should be UTF-8");

        assert!(rendered.contains("Input: Input Only, channel 1"));
        assert!(rendered.contains("Output: Output Only, channel 1"));
        assert!(rendered.contains("Sample rate: 44100 Hz"));
    }

    #[test]
    fn back_leaves_device_selection_without_changing_route() {
        let mut keys = ScriptedKeys::new([
            KeyPress::Character('3'),
            KeyPress::Character('2'),
            KeyPress::Back,
            KeyPress::Back,
            KeyPress::Character('1'),
        ]);
        let mut output = Vec::new();

        let action = run_with_keys(&mut keys, &mut output, config(), &inventory())
            .expect("menu should complete");
        let config = started_config(action);

        assert_eq!(config.audio.output_device, "Output Only");
        assert_eq!(config.audio.output_channel, 1);
    }

    #[test]
    fn unavailable_saved_routes_receive_available_defaults() {
        let mut config = config();
        config.audio.input_device = "Disconnected".to_string();
        config.audio.output_device = "Disconnected".to_string();
        let mut keys = ScriptedKeys::new([KeyPress::Quit]);
        let mut output = Vec::new();

        let action = run_with_keys(&mut keys, &mut output, config, &inventory())
            .expect("menu should complete");
        let config = match action {
            MenuAction::Quit(config) => config,
            MenuAction::Start(_) => panic!("expected quit action"),
        };

        assert_eq!(config.audio.input_device, "Input Only");
        assert_eq!(config.audio.input_channel, 0);
        assert_eq!(config.audio.output_device, "Output Only");
        assert_eq!(config.audio.output_channel, 0);
    }

    #[test]
    fn settings_show_every_persisted_section() {
        let mut keys = ScriptedKeys::new([KeyPress::Character('3'), KeyPress::Character('q')]);
        let mut output = Vec::new();

        run_with_keys(&mut keys, &mut output, config(), &inventory())
            .expect("settings should render");
        let rendered = String::from_utf8(output).expect("output should be UTF-8");

        for label in [
            "Timecode start",
            "Reference BPM",
            "FPS estimate window",
            "Timing engine",
            "Manual latency",
            "MIDI port name",
            "Send MIDI transport",
            "Load defaults",
        ] {
            assert!(rendered.contains(label), "missing setting label: {label}");
        }
    }

    #[test]
    fn settings_align_names_and_values_with_dotted_leaders() {
        let short = setting_row("LTC FPS", "30");
        let long = setting_row("Tempo smoothing alpha", "0.15");

        assert_eq!(short.find("30"), long.find("0.15"));
        assert!(short.contains("LTC FPS ..."));
        assert!(long.contains("Tempo smoothing alpha ..."));

        let mut keys = ScriptedKeys::new([KeyPress::Character('3'), KeyPress::Character('q')]);
        let mut output = Vec::new();
        run_with_keys(&mut keys, &mut output, config(), &inventory())
            .expect("settings should render");
        let rendered = String::from_utf8(output).expect("output should be UTF-8");

        assert!(rendered.contains(">  1. Input"));
        assert!(rendered.contains("  10. Dropout reset"));
    }

    #[test]
    fn entering_setting_renders_its_help_automatically() {
        let mut keys = ScriptedKeys::new([
            KeyPress::Character('3'),
            KeyPress::Character('1'),
            KeyPress::Back,
            KeyPress::Character('q'),
        ]);
        let mut output = Vec::new();

        run_with_keys(&mut keys, &mut output, config(), &inventory()).expect("help should render");
        let rendered = String::from_utf8(output).expect("output should be UTF-8");

        assert!(rendered.contains("used to decode LTC from tape"));
        assert!(!rendered.contains("h: help"));
    }

    #[test]
    fn timing_engine_can_select_legacy_frame_clock() {
        let mut scripted = vec![KeyPress::Character('3')];
        scripted.extend(std::iter::repeat_n(KeyPress::Down, 10));
        scripted.extend([
            KeyPress::Enter,
            KeyPress::Character('2'),
            KeyPress::Character('q'),
        ]);
        let mut keys = ScriptedKeys::new(scripted);
        let mut output = Vec::new();

        let action = run_with_keys(&mut keys, &mut output, config(), &inventory())
            .expect("timing engine should be selectable");
        let config = match action {
            MenuAction::Quit(config) => config,
            MenuAction::Start(_) => panic!("expected quit action"),
        };

        assert_eq!(config.decode.timing_engine, TimingEngine::LegacyFrameClock);
    }

    #[test]
    fn load_defaults_resets_values_and_selects_available_routes() {
        let mut config = config();
        config.tempo.ref_bpm = 96.0;
        config.latency_ms.manual = 125.0;
        let mut keys = ScriptedKeys::new([
            KeyPress::Character('3'),
            KeyPress::Up,
            KeyPress::Up,
            KeyPress::Enter,
            KeyPress::Character('2'),
            KeyPress::Character('q'),
        ]);
        let mut output = Vec::new();

        let action = run_with_keys(&mut keys, &mut output, config, &inventory())
            .expect("defaults should load");
        let config = match action {
            MenuAction::Quit(config) => config,
            MenuAction::Start(_) => panic!("expected quit action"),
        };

        assert_eq!(config.tempo.ref_bpm, 128.0);
        assert_eq!(config.latency_ms.manual, 0.0);
        assert_eq!(config.audio.input_device, "Input Only");
        assert_eq!(config.audio.output_device, "Output Only");
    }

    #[test]
    fn text_editor_updates_and_validates_numeric_setting() {
        let mut keys = ScriptedKeys::new([
            KeyPress::Character('3'),
            KeyPress::Down,
            KeyPress::Down,
            KeyPress::Down,
            KeyPress::Down,
            KeyPress::Down,
            KeyPress::Enter,
            KeyPress::Backspace,
            KeyPress::Backspace,
            KeyPress::Backspace,
            KeyPress::Character('1'),
            KeyPress::Character('2'),
            KeyPress::Character('6'),
            KeyPress::Enter,
            KeyPress::Character('q'),
        ]);
        let mut output = Vec::new();

        let action = run_with_keys(&mut keys, &mut output, config(), &inventory())
            .expect("numeric setting should update");
        let config = match action {
            MenuAction::Quit(config) => config,
            MenuAction::Start(_) => panic!("expected quit action"),
        };

        assert_eq!(config.tempo.ref_bpm, 126.0);
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("Project tempo at the reference tape speed"));
    }
}
