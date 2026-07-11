use crate::config::{AppConfig, Mode};
use crate::startup::{StartupReport, SystemInventory, preflight};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SupportedStreamConfigRange};
use midir::MidiOutputConnection;
use std::fmt;

#[cfg(unix)]
use midir::os::unix::VirtualOutput;

const RETRY_HINT: &str = "Fix the device or channel configuration, then try again.";

#[derive(Debug)]
pub struct StartupRuntime<A = Device, M = MidiOutputConnection> {
    pub audio: AudioRuntime<A>,
    pub midi: M,
    pub report: StartupReport,
}

#[derive(Debug)]
pub enum AudioRuntime<A = Device> {
    Generate { output: AudioEndpoint<A> },
    Decode { input: AudioEndpoint<A> },
}

#[derive(Debug)]
pub struct AudioEndpoint<A> {
    pub device: A,
    pub sample_rate: u32,
    pub channel: u16,
}

pub fn initialize(config: &AppConfig) -> Result<StartupRuntime, RuntimeError> {
    let inventory = SystemInventory::gather().map_err(RuntimeError::Inventory)?;
    let backend = SystemBackend;
    initialize_with(config, &inventory, &backend)
}

pub fn initialize_with<B>(
    config: &AppConfig,
    inventory: &SystemInventory,
    backend: &B,
) -> Result<StartupRuntime<B::AudioHandle, B::MidiHandle>, RuntimeError>
where
    B: RuntimeBackend,
{
    let report = preflight(config, inventory).map_err(RuntimeError::Preflight)?;
    let audio = match config.mode {
        Mode::Generate => AudioRuntime::Generate {
            output: backend.open_output(AudioRequest {
                device_name: &config.audio.output_device,
                sample_rate: config.audio.sample_rate,
                channel: config.audio.output_channel,
            })?,
        },
        Mode::Decode => AudioRuntime::Decode {
            input: backend.open_input(AudioRequest {
                device_name: &config.audio.input_device,
                sample_rate: config.audio.sample_rate,
                channel: config.audio.input_channel,
            })?,
        },
    };
    let midi = backend.create_virtual_midi_output(&config.midi.port_name)?;

    Ok(StartupRuntime { audio, midi, report })
}

#[derive(Debug, Clone, Copy)]
pub struct AudioRequest<'a> {
    pub device_name: &'a str,
    pub sample_rate: u32,
    pub channel: u16,
}

pub trait RuntimeBackend {
    type AudioHandle;
    type MidiHandle;

    fn open_input(&self, request: AudioRequest<'_>) -> Result<AudioEndpoint<Self::AudioHandle>, RuntimeError>;
    fn open_output(&self, request: AudioRequest<'_>) -> Result<AudioEndpoint<Self::AudioHandle>, RuntimeError>;
    fn create_virtual_midi_output(&self, port_name: &str) -> Result<Self::MidiHandle, RuntimeError>;
}

pub struct SystemBackend;

impl RuntimeBackend for SystemBackend {
    type AudioHandle = Device;
    type MidiHandle = MidiOutputConnection;

    fn open_input(&self, request: AudioRequest<'_>) -> Result<AudioEndpoint<Self::AudioHandle>, RuntimeError> {
        let host = cpal::default_host();
        let device = find_device(&host, request.device_name)?;
        ensure_input_support(&device, request.sample_rate, request.channel)?;

        Ok(AudioEndpoint {
            device,
            sample_rate: request.sample_rate,
            channel: request.channel,
        })
    }

    fn open_output(&self, request: AudioRequest<'_>) -> Result<AudioEndpoint<Self::AudioHandle>, RuntimeError> {
        let host = cpal::default_host();
        let device = find_device(&host, request.device_name)?;
        ensure_output_support(&device, request.sample_rate, request.channel)?;

        Ok(AudioEndpoint {
            device,
            sample_rate: request.sample_rate,
            channel: request.channel,
        })
    }

    fn create_virtual_midi_output(&self, port_name: &str) -> Result<Self::MidiHandle, RuntimeError> {
        create_virtual_midi_output(port_name)
    }
}

fn find_device(host: &cpal::Host, name: &str) -> Result<Device, RuntimeError> {
    let mut devices = host
        .devices()
        .map_err(|source| RuntimeError::AudioEnumeration(source.to_string()))?;

    devices
        .find(|device| device.name().ok().as_deref() == Some(name))
        .ok_or_else(|| RuntimeError::MissingAudioDevice(name.to_string()))
}

fn ensure_input_support(device: &Device, sample_rate: u32, channel: u16) -> Result<(), RuntimeError> {
    let ranges = device
        .supported_input_configs()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?;
    ensure_supported(ranges, sample_rate, channel, "input", &device_name(device)?)
}

fn ensure_output_support(device: &Device, sample_rate: u32, channel: u16) -> Result<(), RuntimeError> {
    let ranges = device
        .supported_output_configs()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?;
    ensure_supported(ranges, sample_rate, channel, "output", &device_name(device)?)
}

fn ensure_supported<I>(
    ranges: I,
    sample_rate: u32,
    channel: u16,
    direction: &str,
    device_name: &str,
) -> Result<(), RuntimeError>
where
    I: Iterator<Item = SupportedStreamConfigRange>,
{
    let supported = ranges.into_iter().any(|range| {
        range.channels() > channel
            && range.min_sample_rate().0 <= sample_rate
            && range.max_sample_rate().0 >= sample_rate
    });

    if supported {
        Ok(())
    } else {
        Err(RuntimeError::UnsupportedAudioConfiguration {
            direction: direction.to_string(),
            device_name: device_name.to_string(),
            sample_rate,
            channel,
        })
    }
}

fn device_name(device: &Device) -> Result<String, RuntimeError> {
    device
        .name()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))
}

#[cfg(unix)]
fn create_virtual_midi_output(port_name: &str) -> Result<MidiOutputConnection, RuntimeError> {
    let midi_output = midir::MidiOutput::new("TapeSync runtime init")
        .map_err(|source| RuntimeError::Midi(source.to_string()))?;
    midi_output
        .create_virtual(port_name)
        .map_err(|source| RuntimeError::Midi(source.to_string()))
}

#[cfg(not(any(target_os = "macos", all(unix, not(target_os = "macos")))))]
fn create_virtual_midi_output(_port_name: &str) -> Result<MidiOutputConnection, RuntimeError> {
    Err(RuntimeError::Midi(
        "virtual MIDI output is unsupported on this platform".to_string(),
    ))
}

#[derive(Debug)]
pub enum RuntimeError {
    Inventory(crate::startup::InventoryError),
    Preflight(crate::startup::StartupError),
    AudioEnumeration(String),
    AudioConfiguration(String),
    MissingAudioDevice(String),
    UnsupportedAudioConfiguration {
        direction: String,
        device_name: String,
        sample_rate: u32,
        channel: u16,
    },
    Midi(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inventory(source) => write!(f, "{source}"),
            Self::Preflight(source) => write!(f, "{source}"),
            Self::AudioEnumeration(source) => {
                write!(f, "failed to enumerate audio devices: {source}")
            }
            Self::AudioConfiguration(source) => {
                write!(f, "failed to inspect audio device capabilities: {source}")
            }
            Self::MissingAudioDevice(name) => {
                write!(f, "configured audio device '{name}' was not found during runtime initialization. {RETRY_HINT}")
            }
            Self::UnsupportedAudioConfiguration {
                direction,
                device_name,
                sample_rate,
                channel,
            } => {
                write!(
                    f,
                    "configured {direction} device '{device_name}' does not support sample rate {sample_rate} on channel {channel}. {RETRY_HINT}"
                )
            }
            Self::Midi(source) => write!(f, "failed to initialize MIDI output: {source}. {RETRY_HINT}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::startup::{AudioDeviceInfo, MidiEnvironment};

    #[derive(Default)]
    struct FakeBackend {
        fail_output: bool,
        fail_input: bool,
        fail_midi: bool,
    }

    impl RuntimeBackend for FakeBackend {
        type AudioHandle = String;
        type MidiHandle = String;

        fn open_input(&self, request: AudioRequest<'_>) -> Result<AudioEndpoint<Self::AudioHandle>, RuntimeError> {
            if self.fail_input {
                return Err(RuntimeError::UnsupportedAudioConfiguration {
                    direction: "input".to_string(),
                    device_name: request.device_name.to_string(),
                    sample_rate: request.sample_rate,
                    channel: request.channel,
                });
            }

            Ok(AudioEndpoint {
                device: request.device_name.to_string(),
                sample_rate: request.sample_rate,
                channel: request.channel,
            })
        }

        fn open_output(&self, request: AudioRequest<'_>) -> Result<AudioEndpoint<Self::AudioHandle>, RuntimeError> {
            if self.fail_output {
                return Err(RuntimeError::UnsupportedAudioConfiguration {
                    direction: "output".to_string(),
                    device_name: request.device_name.to_string(),
                    sample_rate: request.sample_rate,
                    channel: request.channel,
                });
            }

            Ok(AudioEndpoint {
                device: request.device_name.to_string(),
                sample_rate: request.sample_rate,
                channel: request.channel,
            })
        }

        fn create_virtual_midi_output(&self, port_name: &str) -> Result<Self::MidiHandle, RuntimeError> {
            if self.fail_midi {
                return Err(RuntimeError::Midi("backend unavailable".to_string()));
            }

            Ok(port_name.to_string())
        }
    }

    fn inventory() -> SystemInventory {
        SystemInventory {
            audio_devices: vec![
                AudioDeviceInfo {
                    name: "Input A".to_string(),
                    input_channels: 2,
                    output_channels: 0,
                },
                AudioDeviceInfo {
                    name: "Output A".to_string(),
                    input_channels: 0,
                    output_channels: 2,
                },
            ],
            midi: MidiEnvironment {
                backend_available: true,
            },
        }
    }

    fn config(mode: &str) -> AppConfig {
        AppConfig::from_toml_str(&format!(
            r#"
mode = "{mode}"

[audio]
sample_rate = 44100
input_device = "Input A"
input_channel = 1
output_device = "Output A"
output_channel = 0

[timecode]
start = "01:00:00:00"
ltc_fps = 30.0

[tempo]
ref_bpm = 128.0
ref_fps = 30.0
smoothing_alpha = 0.15

[latency_ms]
audio_output = 5.0
tape_path = 20.0
decoder = 10.0
manual = 0.0

[midi]
port_name = "TapeSync MIDI Out"
send_mtc = false
send_clock = true
send_transport = true
"#
        ))
        .expect("config should parse")
    }

    #[test]
    fn initializes_generate_runtime_with_fake_backend() {
        let runtime = initialize_with(&config("generate"), &inventory(), &FakeBackend::default())
            .expect("runtime should initialize");

        match runtime.audio {
            AudioRuntime::Generate { output } => {
                assert_eq!(output.device, "Output A");
                assert_eq!(output.sample_rate, 44100);
                assert_eq!(output.channel, 0);
            }
            AudioRuntime::Decode { .. } => panic!("expected generate runtime"),
        }

        assert_eq!(runtime.midi, "TapeSync MIDI Out");
    }

    #[test]
    fn propagates_backend_audio_failure() {
        let error = initialize_with(
            &config("generate"),
            &inventory(),
            &FakeBackend {
                fail_output: true,
                ..FakeBackend::default()
            },
        )
        .expect_err("runtime should fail");

        assert!(error
            .to_string()
            .contains("does not support sample rate 44100 on channel 0"));
    }

    #[test]
    fn propagates_backend_midi_failure() {
        let error = initialize_with(
            &config("decode"),
            &inventory(),
            &FakeBackend {
                fail_midi: true,
                ..FakeBackend::default()
            },
        )
        .expect_err("midi init should fail");

        assert!(error.to_string().contains("failed to initialize MIDI output"));
    }
}
