use crate::config::{AppConfig, Mode};
use crate::audio::{self, AudioEndpoint, AudioRuntime};
use crate::ltc::{DecodeRequest, GeneratorRequest, SharedDecodeStatusHandler, Timecode};
use crate::midi::{self, MidiOutputPort, MidiTransport};
use crate::startup::{StartupReport, SystemInventory, preflight};
use crate::sync_core::{DecodeSyncBridge, SyncEngine};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Stream};
use std::fmt;
use std::sync::{Arc, Mutex};

const RETRY_HINT: &str = "Fix the device or channel configuration, then try again.";

#[derive(Debug)]
pub struct StartupRuntime<A = Device, S = Stream, M = MidiOutputPort> {
    pub audio: AudioRuntime<A, S>,
    pub midi_port_name: String,
    pub report: StartupReport,
    _midi: Option<M>,
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
) -> Result<StartupRuntime<B::AudioHandle, B::StreamHandle, B::MidiHandle>, RuntimeError>
where
    B: RuntimeBackend,
    B::MidiHandle: MidiTransport + Send + 'static,
{
    let report = preflight(config, inventory).map_err(RuntimeError::Preflight)?;
    let midi_port_name = config.midi.port_name.clone();
    let midi = backend.create_virtual_midi_output(&midi_port_name)?;

    let (audio, midi) = match config.mode {
        Mode::Generate => (
            AudioRuntime::Generate {
                output: backend.open_output(
                    AudioRequest {
                        device_name: &config.audio.output_device,
                        sample_rate: config.audio.sample_rate,
                        channel: config.audio.output_channel,
                    },
                    GeneratorRequest {
                        start: &config.timecode.start,
                        fps: config.timecode.ltc_fps,
                    },
                )?,
            },
            Some(midi),
        ),
        Mode::Decode => {
            let decode_status_handler = Some(make_decode_status_handler(
                midi,
                config.tempo.ref_bpm,
                config.timecode.ltc_fps.as_f64(),
                config.midi.send_clock,
                config.midi.send_transport,
            ));
            (
                AudioRuntime::Decode {
                    input: backend.open_input(
                        AudioRequest {
                            device_name: &config.audio.input_device,
                            sample_rate: config.audio.sample_rate,
                            channel: config.audio.input_channel,
                        },
                        DecodeRequest {
                            fps: config.timecode.ltc_fps,
                        },
                        decode_status_handler,
                    )?,
                },
                None,
            )
        }
    };

    Ok(StartupRuntime {
        audio,
        midi_port_name,
        report,
        _midi: midi,
    })
}

fn make_decode_status_handler<M>(
    midi: M,
    ref_bpm: f64,
    ltc_fps: f64,
    send_clock: bool,
    send_transport: bool,
) -> SharedDecodeStatusHandler
where
    M: MidiTransport + Send + 'static,
{
    let engine = SyncEngine::new(midi, send_clock, send_transport);
    let bridge = DecodeSyncBridge::new(
        engine,
        ref_bpm,
        ltc_fps,
        Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
        },
    );
    Arc::new(Mutex::new(Box::new(bridge)))
}

#[derive(Debug, Clone, Copy)]
pub struct AudioRequest<'a> {
    pub device_name: &'a str,
    pub sample_rate: u32,
    pub channel: u16,
}

pub trait RuntimeBackend {
    type AudioHandle;
    type StreamHandle;
    type MidiHandle;

    fn open_input(
        &self,
        request: AudioRequest<'_>,
        decode_request: DecodeRequest,
        decode_status_handler: Option<SharedDecodeStatusHandler>,
    ) -> Result<AudioEndpoint<Self::AudioHandle, Self::StreamHandle>, RuntimeError>;
    fn open_output(
        &self,
        request: AudioRequest<'_>,
        generator_request: GeneratorRequest<'_>,
    ) -> Result<AudioEndpoint<Self::AudioHandle, Self::StreamHandle>, RuntimeError>;
    fn create_virtual_midi_output(&self, port_name: &str) -> Result<Self::MidiHandle, RuntimeError>;
}

pub struct SystemBackend;

impl RuntimeBackend for SystemBackend {
    type AudioHandle = Device;
    type StreamHandle = Stream;
    type MidiHandle = MidiOutputPort;

    fn open_input(
        &self,
        request: AudioRequest<'_>,
        decode_request: DecodeRequest,
        decode_status_handler: Option<SharedDecodeStatusHandler>,
    ) -> Result<AudioEndpoint<Self::AudioHandle, Self::StreamHandle>, RuntimeError> {
        let host = cpal::default_host();
        let device = find_device(&host, request.device_name)?;
        audio::open_input_stream(
            device,
            request.sample_rate,
            request.channel,
            decode_request,
            decode_status_handler,
        )
    }

    fn open_output(
        &self,
        request: AudioRequest<'_>,
        generator_request: GeneratorRequest<'_>,
    ) -> Result<AudioEndpoint<Self::AudioHandle, Self::StreamHandle>, RuntimeError> {
        let host = cpal::default_host();
        let device = find_device(&host, request.device_name)?;
        audio::open_output_stream(device, request.sample_rate, request.channel, generator_request)
    }

    fn create_virtual_midi_output(&self, port_name: &str) -> Result<Self::MidiHandle, RuntimeError> {
        midi::create_virtual_output(port_name)
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

#[derive(Debug)]
pub enum RuntimeError {
    Inventory(crate::startup::InventoryError),
    Preflight(crate::startup::StartupError),
    AudioEnumeration(String),
    AudioConfiguration(String),
    AudioStream(String),
    Ltc(String),
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
            Self::AudioStream(source) => {
                write!(f, "failed to open or start audio stream: {source}. {RETRY_HINT}")
            }
            Self::Ltc(source) => {
                write!(f, "failed to initialize LTC generator: {source}. {RETRY_HINT}")
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
    use crate::midi::MidiSink;
    use crate::startup::{AudioDeviceInfo, MidiEnvironment};

    #[derive(Default)]
    struct FakeBackend {
        fail_output: bool,
        fail_input: bool,
        fail_midi: bool,
    }

    impl RuntimeBackend for FakeBackend {
        type AudioHandle = String;
        type StreamHandle = FakeStream;
        type MidiHandle = MidiOutputPort<FakeMidiConnection>;

        fn open_input(
            &self,
            request: AudioRequest<'_>,
            _decode_request: DecodeRequest,
            _decode_status_handler: Option<SharedDecodeStatusHandler>,
        ) -> Result<AudioEndpoint<Self::AudioHandle, Self::StreamHandle>, RuntimeError> {
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
                stream: FakeStream,
                decode_status: Some(std::sync::Arc::new(std::sync::Mutex::new(
                    crate::ltc::DecodeStatus::default(),
                ))),
            })
        }

        fn open_output(
            &self,
            request: AudioRequest<'_>,
            _generator_request: GeneratorRequest<'_>,
        ) -> Result<AudioEndpoint<Self::AudioHandle, Self::StreamHandle>, RuntimeError> {
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
                stream: FakeStream,
                decode_status: None,
            })
        }

        fn create_virtual_midi_output(&self, port_name: &str) -> Result<Self::MidiHandle, RuntimeError> {
            if self.fail_midi {
                return Err(RuntimeError::Midi("backend unavailable".to_string()));
            }

            Ok(MidiOutputPort::new(
                port_name.to_string(),
                FakeMidiConnection { messages: Vec::new() },
            ))
        }
    }

    #[derive(Debug)]
    struct FakeStream;

    #[derive(Debug)]
    struct FakeMidiConnection {
        messages: Vec<Vec<u8>>,
    }

    impl MidiSink for FakeMidiConnection {
        fn send(&mut self, message: &[u8]) -> Result<(), String> {
            self.messages.push(message.to_vec());
            Ok(())
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

        assert_eq!(runtime.midi_port_name, "TapeSync MIDI Out");
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
