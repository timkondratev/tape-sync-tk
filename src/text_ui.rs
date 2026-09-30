use std::fmt;
use std::io::Write;
use std::time::Duration;

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

use crate::config::{AppConfig, Mode};
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
        KeyCode::Esc => KeyPress::Back,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyPress::Quit,
        KeyCode::Char(character) if character.eq_ignore_ascii_case(&'b') => KeyPress::Back,
        KeyCode::Char(character) if character.eq_ignore_ascii_case(&'q') => KeyPress::Quit,
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
    loop {
        let items = [
            format!(
                "Input: {}",
                route_summary(config, inventory, AudioDirection::Input)
            ),
            format!(
                "Output: {}",
                route_summary(config, inventory, AudioDirection::Output)
            ),
            format!("Sample rate: {} Hz", config.audio.sample_rate),
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
                if select_audio_route(keys, output, config, inventory, AudioDirection::Input)?
                    == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(1) => {
                if select_audio_route(keys, output, config, inventory, AudioDirection::Output)?
                    == Flow::Quit
                {
                    return Ok(Flow::Quit);
                }
            }
            Choice::Selected(2) => match choose(
                keys,
                output,
                "Sample Rate",
                "Select the audio sample rate.",
                &["44100 Hz", "48000 Hz"],
                usize::from(config.audio.sample_rate == 48_000),
            )? {
                Choice::Selected(0) => config.audio.sample_rate = 44_100,
                Choice::Selected(1) => config.audio.sample_rate = 48_000,
                Choice::Quit => return Ok(Flow::Quit),
                Choice::Back => {}
                Choice::Selected(_) => unreachable!(),
            },
            Choice::Selected(3) | Choice::Back => return Ok(Flow::Back),
            Choice::Quit => return Ok(Flow::Quit),
            Choice::Selected(_) => unreachable!(),
        }
    }
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
        "Current settings\nMode: {mode:?}\nInput: {input}\nOutput: {output}\nSample rate: {sample_rate} Hz\n\nUse arrows + Enter or press a number. Esc/b goes back; q quits.",
        mode = config.mode,
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
    let device_index = match choose(
        keys,
        output,
        &format!("Select {} Device", direction.label()),
        "Esc/b returns to Settings.",
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
    let channel = match choose(
        keys,
        output,
        &format!("Select {} Channel", direction.label()),
        "Esc/b returns to device selection.",
        &channel_refs,
        selected_channel,
    )? {
        Choice::Selected(index) => index as u16,
        Choice::Back => return select_audio_route(keys, output, config, inventory, direction),
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
    loop {
        execute!(output, Clear(ClearType::All), MoveTo(0, 0))?;
        writeln!(output, "{title}\r")?;
        for line in details.lines() {
            writeln!(output, "{line}\r")?;
        }
        for (index, item) in items.iter().enumerate() {
            let marker = if index == selected { ">" } else { " " };
            writeln!(output, "{marker} {}. {item}\r", index + 1)?;
        }
        output.flush()?;

        match keys.read_key()? {
            KeyPress::Up => selected = selected.checked_sub(1).unwrap_or(items.len() - 1),
            KeyPress::Down => selected = (selected + 1) % items.len(),
            KeyPress::Enter => return Ok(Choice::Selected(selected)),
            KeyPress::Character(character) if character.is_ascii_digit() => {
                let index = character.to_digit(10).unwrap_or(0) as usize;
                if (1..=items.len()).contains(&index) {
                    return Ok(Choice::Selected(index - 1));
                }
            }
            KeyPress::Back => return Ok(Choice::Back),
            KeyPress::Quit => return Ok(Choice::Quit),
            KeyPress::Character(_) => {}
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
}
