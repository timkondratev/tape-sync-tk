use crate::config::{AppConfig, Mode};
use cpal::traits::{DeviceTrait, HostTrait};
use midir::MidiOutput;
use std::fmt;

const RETRY_HINT: &str = "Fix the device or channel configuration, then try again.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub input_channels: u16,
    pub output_channels: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiEnvironment {
    pub backend_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInventory {
    pub audio_devices: Vec<AudioDeviceInfo>,
    pub midi: MidiEnvironment,
}

impl SystemInventory {
    pub fn gather() -> Result<Self, InventoryError> {
        let host = cpal::default_host();
        let devices = host
            .devices()
            .map_err(|source| InventoryError::audio_enumeration(source.to_string()))?;

        let mut audio_devices = Vec::new();
        for device in devices {
            let name = device
                .name()
                .map_err(|source| InventoryError::audio_enumeration(source.to_string()))?;
            let input_channels = device
                .supported_input_configs()
                .ok()
                .and_then(|configs| configs.map(|config| config.channels()).max())
                .unwrap_or(0);
            let output_channels = device
                .supported_output_configs()
                .ok()
                .and_then(|configs| configs.map(|config| config.channels()).max())
                .unwrap_or(0);

            audio_devices.push(AudioDeviceInfo {
                name,
                input_channels,
                output_channels,
            });
        }

        let midi = MidiEnvironment {
            backend_available: MidiOutput::new("TapeSync startup preflight").is_ok(),
        };

        Ok(Self {
            audio_devices,
            midi,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartupReport {
    pub total_latency_ms: f64,
    pub warnings: Vec<String>,
}

pub fn preflight(
    config: &AppConfig,
    inventory: &SystemInventory,
) -> Result<StartupReport, StartupError> {
    match config.mode {
        Mode::Generate => {
            let device = inventory
                .audio_devices
                .iter()
                .find(|device| device.name == config.audio.output_device)
                .ok_or_else(|| {
                    StartupError::missing_audio_device(
                        "output",
                        &config.audio.output_device,
                        &inventory.audio_devices,
                    )
                })?;
            ensure_channel_available(
                "output",
                config.audio.output_channel,
                device.output_channels,
                &device.name,
            )?;
        }
        Mode::Decode => {
            let device = inventory
                .audio_devices
                .iter()
                .find(|device| device.name == config.audio.input_device)
                .ok_or_else(|| {
                    StartupError::missing_audio_device(
                        "input",
                        &config.audio.input_device,
                        &inventory.audio_devices,
                    )
                })?;
            ensure_channel_available(
                "input",
                config.audio.input_channel,
                device.input_channels,
                &device.name,
            )?;
        }
    }

    if !inventory.midi.backend_available {
        return Err(StartupError {
            message: format!("MIDI output backend is unavailable. {RETRY_HINT}"),
        });
    }

    let total_latency_ms = config.total_latency_ms();
    let warnings = if total_latency_ms.abs() > 1000.0 {
        vec![format!(
            "total_latency_ms is {total_latency_ms}, which exceeds the soft warning threshold of 1000.0 ms"
        )]
    } else {
        Vec::new()
    };

    Ok(StartupReport {
        total_latency_ms,
        warnings,
    })
}

fn ensure_channel_available(
    direction: &str,
    configured_channel: u16,
    available_channels: u16,
    device_name: &str,
) -> Result<(), StartupError> {
    if available_channels == 0 || configured_channel >= available_channels {
        return Err(StartupError {
            message: format!(
                "Configured {direction} channel {configured_channel} is unavailable for device '{device_name}' (available channel count: {available_channels}). {RETRY_HINT}"
            ),
        });
    }

    Ok(())
}

#[derive(Debug)]
pub struct InventoryError {
    source: String,
}

impl InventoryError {
    fn audio_enumeration(source: String) -> Self {
        Self { source }
    }
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to gather startup inventory: {}", self.source)
    }
}

impl std::error::Error for InventoryError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupError {
    pub message: String,
}

impl StartupError {
    fn missing_audio_device(direction: &str, requested: &str, devices: &[AudioDeviceInfo]) -> Self {
        let available = if devices.is_empty() {
            "none".to_string()
        } else {
            devices
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };

        Self {
            message: format!(
                "Configured {direction} device '{requested}' was not found. Available audio devices: {available}. {RETRY_HINT}"
            ),
        }
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StartupError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn config_for_mode(mode: &str) -> AppConfig {
        let mut config = AppConfig::from_toml_str(
            r#"
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
audio_output = 400.0
tape_path = 400.0
decoder = 250.0
manual = 10.0

[midi]
port_name = "TapeSync MIDI Out"
send_mtc = false
send_clock = true
send_transport = true
"#,
        )
        .expect("config should parse");
        config.mode = match mode {
            "generate" => Mode::Generate,
            "decode" => Mode::Decode,
            _ => panic!("unsupported test mode"),
        };
        config
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

    #[test]
    fn generate_mode_fails_when_output_device_is_missing() {
        let mut config = config_for_mode("generate");
        config.audio.output_device = "Missing Output".to_string();

        let error = preflight(&config, &inventory()).expect_err("preflight should fail");
        assert!(
            error
                .message
                .contains("Configured output device 'Missing Output' was not found")
        );
        assert!(error.message.contains(RETRY_HINT));
    }

    #[test]
    fn decode_mode_fails_when_input_channel_is_out_of_bounds() {
        let mut config = config_for_mode("decode");
        config.audio.input_channel = 2;

        let error = preflight(&config, &inventory()).expect_err("preflight should fail");
        assert!(
            error
                .message
                .contains("Configured input channel 2 is unavailable")
        );
    }

    #[test]
    fn fails_when_midi_backend_is_unavailable() {
        let config = config_for_mode("generate");
        let mut inventory = inventory();
        inventory.midi.backend_available = false;

        let error = preflight(&config, &inventory).expect_err("midi should fail");
        assert!(error.message.contains("MIDI output backend is unavailable"));
        assert!(error.message.contains(RETRY_HINT));
    }

    #[test]
    fn returns_soft_warning_for_large_total_latency() {
        let config = config_for_mode("generate");
        let report = preflight(&config, &inventory()).expect("preflight should pass");

        assert!((report.total_latency_ms - 1060.0).abs() < f64::EPSILON);
        assert_eq!(report.warnings.len(), 1);
    }
}
