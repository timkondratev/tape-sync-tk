use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use crate::ltc::Timecode;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Generate,
    Decode,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Generate
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fps {
    Fps24,
    Fps25,
    Fps29_97,
    Fps30,
}

impl Fps {
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Fps24 => 24.0,
            Self::Fps25 => 25.0,
            Self::Fps29_97 => 29.97,
            Self::Fps30 => 30.0,
        }
    }

    pub fn frame_count_base(self) -> u8 {
        match self {
            Self::Fps24 => 24,
            Self::Fps25 => 25,
            Self::Fps29_97 | Self::Fps30 => 30,
        }
    }
}

impl<'de> Deserialize<'de> for Fps {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FpsVisitor;

        impl Visitor<'_> for FpsVisitor {
            type Value = Fps;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one of 24.0, 25.0, 29.97, 30.0")
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                parse_fps(value).map_err(E::custom)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_f64(value as f64)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_f64(value as f64)
            }
        }

        deserializer.deserialize_any(FpsVisitor)
    }
}

impl Serialize for Fps {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.as_f64())
    }
}

fn parse_fps(value: f64) -> Result<Fps, String> {
    match value {
        24.0 => Ok(Fps::Fps24),
        25.0 => Ok(Fps::Fps25),
        29.97 => Ok(Fps::Fps29_97),
        30.0 => Ok(Fps::Fps30),
        _ => Err(format!(
            "invalid fps value {value}; expected one of 24.0, 25.0, 29.97, 30.0"
        )),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    #[serde(skip)]
    pub mode: Mode,
    pub audio: AudioConfig,
    pub timecode: TimecodeConfig,
    pub tempo: TempoConfig,
    #[serde(default)]
    pub decode: DecodeConfig,
    pub latency_ms: LatencyConfig,
    pub midi: MidiConfig,
}

impl AppConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&raw)
    }

    pub fn from_toml_str(raw: &str) -> Result<Self, ConfigError> {
        let config: AppConfig = toml::from_str(raw).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let path = path.as_ref();
        let raw = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        fs::write(path, raw).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_range(
            "tempo.smoothing_alpha",
            self.tempo.smoothing_alpha,
            0.01,
            1.0,
        )?;
        validate_range(
            "decode.fps_estimate_window_frames",
            self.decode.fps_estimate_window_frames as f64,
            1.0,
            120.0,
        )?;
        validate_range(
            "decode.dropout_reset_windows",
            self.decode.dropout_reset_windows as f64,
            1.0,
            120.0,
        )?;
        validate_range(
            "latency_ms.audio_output",
            self.latency_ms.audio_output,
            -500.0,
            500.0,
        )?;
        validate_range(
            "latency_ms.tape_path",
            self.latency_ms.tape_path,
            -500.0,
            500.0,
        )?;
        validate_range("latency_ms.decoder", self.latency_ms.decoder, -500.0, 500.0)?;
        validate_range("latency_ms.manual", self.latency_ms.manual, -2000.0, 2000.0)?;

        if self.audio.sample_rate != 44_100 && self.audio.sample_rate != 48_000 {
            return Err(ConfigError::Validation {
                field: "audio.sample_rate",
                message: format!(
                    "unsupported sample rate {}; expected 44100 or 48000",
                    self.audio.sample_rate
                ),
            });
        }

        if self.midi.port_name.trim().is_empty() {
            return Err(ConfigError::Validation {
                field: "midi.port_name",
                message: "must not be empty".to_string(),
            });
        }

        let timecode =
            Timecode::from_str(&self.timecode.start).map_err(|source| ConfigError::Validation {
                field: "timecode.start",
                message: source.to_string(),
            })?;

        if timecode.frames >= self.timecode.ltc_fps.frame_count_base() {
            return Err(ConfigError::Validation {
                field: "timecode.start",
                message: format!(
                    "frame value exceeds allowed range for {} fps",
                    self.timecode.ltc_fps.as_f64()
                ),
            });
        }

        Ok(())
    }

    pub fn total_latency_ms(&self) -> f64 {
        self.latency_ms.audio_output
            + self.latency_ms.tape_path
            + self.latency_ms.decoder
            + self.latency_ms.manual
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Generate,
            audio: AudioConfig {
                sample_rate: 44_100,
                input_device: String::new(),
                input_channel: 0,
                output_device: String::new(),
                output_channel: 0,
            },
            timecode: TimecodeConfig {
                start: "01:00:00:00".to_string(),
                ltc_fps: Fps::Fps30,
            },
            tempo: TempoConfig {
                ref_bpm: 128.0,
                ref_fps: Fps::Fps30,
                smoothing_alpha: 0.15,
            },
            decode: DecodeConfig::default(),
            latency_ms: LatencyConfig {
                audio_output: 5.0,
                tape_path: 20.0,
                decoder: 10.0,
                manual: 0.0,
            },
            midi: MidiConfig {
                port_name: "TapeSync MIDI Out".to_string(),
                send_mtc: false,
                send_clock: true,
                send_transport: true,
            },
        }
    }
}

fn validate_range(field: &'static str, value: f64, min: f64, max: f64) -> Result<(), ConfigError> {
    if value < min || value > max {
        return Err(ConfigError::Validation {
            field,
            message: format!("must be within {min}..={max}; got {value}"),
        });
    }

    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub input_device: String,
    pub input_channel: u16,
    pub output_device: String,
    pub output_channel: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimecodeConfig {
    pub start: String,
    pub ltc_fps: Fps,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TempoConfig {
    pub ref_bpm: f64,
    pub ref_fps: Fps,
    pub smoothing_alpha: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DecodeConfig {
    pub fps_estimate_window_frames: usize,
    pub dropout_reset_windows: u32,
}

impl Default for DecodeConfig {
    fn default() -> Self {
        Self {
            fps_estimate_window_frames: 12,
            dropout_reset_windows: 8,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LatencyConfig {
    pub audio_output: f64,
    pub tape_path: f64,
    pub decoder: f64,
    pub manual: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MidiConfig {
    pub port_name: String,
    pub send_mtc: bool,
    pub send_clock: bool,
    pub send_transport: bool,
}

#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: String,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Validation {
        field: &'static str,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to read config at {path}: {source}"),
            Self::Parse(source) => write!(f, "failed to parse config: {source}"),
            Self::Serialize(source) => write!(f, "failed to serialize config: {source}"),
            Self::Validation { field, message } => {
                write!(f, "config validation failed for {field}: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> &'static str {
        r#"
[audio]
sample_rate = 44100
input_device = "Input A"
input_channel = 0
output_device = "Output A"
output_channel = 1

[timecode]
start = "01:00:00:00"
ltc_fps = 30.0

[tempo]
ref_bpm = 128.0
ref_fps = 30.0
smoothing_alpha = 0.15

[decode]
fps_estimate_window_frames = 12
dropout_reset_windows = 8

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
    }

    #[test]
    fn accepts_boundary_values() {
        let config = AppConfig::from_toml_str(valid_config()).expect("config should parse");
        assert_eq!(config.timecode.ltc_fps.as_f64(), 30.0);
        assert_eq!(config.tempo.ref_fps.as_f64(), 30.0);
        assert!((config.total_latency_ms() - 35.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_invalid_fps() {
        let raw = valid_config().replace("ltc_fps = 30.0", "ltc_fps = 28.0");
        let error = AppConfig::from_toml_str(&raw).expect_err("fps should be rejected");
        assert!(error.to_string().contains("invalid fps value 28"));
    }

    #[test]
    fn rejects_smoothing_alpha_below_minimum() {
        let raw = valid_config().replace("smoothing_alpha = 0.15", "smoothing_alpha = 0.009");
        let error = AppConfig::from_toml_str(&raw).expect_err("alpha should be rejected");
        assert!(error.to_string().contains("tempo.smoothing_alpha"));
    }

    #[test]
    fn rejects_decode_window_size_below_minimum() {
        let raw = valid_config().replace(
            "fps_estimate_window_frames = 12",
            "fps_estimate_window_frames = 0",
        );
        let error = AppConfig::from_toml_str(&raw).expect_err("window size should be rejected");
        assert!(
            error
                .to_string()
                .contains("decode.fps_estimate_window_frames")
        );
    }

    #[test]
    fn rejects_manual_latency_above_maximum() {
        let raw = valid_config().replace("manual = 0.0", "manual = 2000.1");
        let error = AppConfig::from_toml_str(&raw).expect_err("manual latency should fail");
        assert!(error.to_string().contains("latency_ms.manual"));
    }

    #[test]
    fn rejects_empty_midi_port_name() {
        let raw =
            valid_config().replace("port_name = \"TapeSync MIDI Out\"", "port_name = \"   \"");
        let error = AppConfig::from_toml_str(&raw).expect_err("empty port name should fail");
        assert!(error.to_string().contains("midi.port_name"));
    }

    #[test]
    fn rejects_timecode_frame_out_of_range_for_fps() {
        let raw = valid_config().replace("start = \"01:00:00:00\"", "start = \"01:00:00:30\"");
        let error = AppConfig::from_toml_str(&raw).expect_err("timecode start should fail");
        assert!(error.to_string().contains("timecode.start"));
    }

    #[test]
    fn example_config_parses_and_validates() {
        let config = AppConfig::from_toml_str(include_str!("../tape-sync.example.toml"))
            .expect("example config should stay valid");

        assert_eq!(config.audio.sample_rate, 44100);
        assert_eq!(config.timecode.ltc_fps.as_f64(), 30.0);
    }

    #[test]
    fn saved_config_round_trips() {
        let config = AppConfig::from_toml_str(valid_config()).expect("config should parse");
        let serialized = toml::to_string_pretty(&config).expect("config should serialize");
        let restored = AppConfig::from_toml_str(&serialized).expect("saved config should parse");

        assert!(!serialized.contains("mode ="));
        assert_eq!(restored.mode, Mode::Generate);
        assert_eq!(restored.audio.output_device, "Output A");
        assert_eq!(restored.audio.output_channel, 1);
        assert_eq!(restored.timecode.ltc_fps, Fps::Fps30);
    }

    #[test]
    fn legacy_mode_field_is_ignored() {
        let raw = format!("mode = \"decode\"\n{}", valid_config());
        let config = AppConfig::from_toml_str(&raw).expect("legacy config should parse");

        assert_eq!(config.mode, Mode::Generate);
    }
}
