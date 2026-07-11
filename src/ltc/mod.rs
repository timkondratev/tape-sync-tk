use crate::config::Fps;
use std::fmt;
use std::str::FromStr;

const LTC_PEAK_AMPLITUDE: f32 = 0.501_187_2;
const SYNC_WORD: [bool; 16] = [
    false, false, true, true, true, true, true, true, true, true, true, true, true, true, false,
    true,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
}

impl Timecode {
    pub fn increment(self, fps: Fps) -> Self {
        let mut next = self;
        next.frames += 1;

        if next.frames >= fps.frame_count_base() {
            next.frames = 0;
            next.seconds += 1;
        }
        if next.seconds >= 60 {
            next.seconds = 0;
            next.minutes += 1;
        }
        if next.minutes >= 60 {
            next.minutes = 0;
            next.hours += 1;
        }
        if next.hours >= 24 {
            next.hours = 0;
        }

        next
    }

    pub fn encode_ltc_bits(self, fps: Fps) -> [bool; 80] {
        let mut bits = [false; 80];

        set_bcd_bits(&mut bits, 0, self.frames % 10, 4);
        set_bcd_bits(&mut bits, 8, self.frames / 10, 2);
        set_bcd_bits(&mut bits, 16, self.seconds % 10, 4);
        set_bcd_bits(&mut bits, 24, self.seconds / 10, 3);
        set_bcd_bits(&mut bits, 32, self.minutes % 10, 4);
        set_bcd_bits(&mut bits, 40, self.minutes / 10, 3);
        set_bcd_bits(&mut bits, 48, self.hours % 10, 4);
        set_bcd_bits(&mut bits, 56, self.hours / 10, 2);

        if matches!(fps, Fps::Fps29_97) {
            bits[10] = false;
        }

        bits[64..80].copy_from_slice(&SYNC_WORD);
        bits
    }
}

impl FromStr for Timecode {
    type Err = LtcError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut parts = input.split(':');
        let hours = parse_component(parts.next())?;
        let minutes = parse_component(parts.next())?;
        let seconds = parse_component(parts.next())?;
        let frames = parse_component(parts.next())?;

        if parts.next().is_some() || hours >= 24 || minutes >= 60 || seconds >= 60 {
            return Err(LtcError::InvalidTimecodeFormat(input.to_string()));
        }

        Ok(Self {
            hours,
            minutes,
            seconds,
            frames,
        })
    }
}

fn parse_component(part: Option<&str>) -> Result<u8, LtcError> {
    let value = part.ok_or_else(|| LtcError::InvalidTimecodeFormat("missing component".to_string()))?;
    if value.len() != 2 {
        return Err(LtcError::InvalidTimecodeFormat(value.to_string()));
    }

    value
        .parse::<u8>()
        .map_err(|_| LtcError::InvalidTimecodeFormat(value.to_string()))
}

fn set_bcd_bits(bits: &mut [bool; 80], offset: usize, value: u8, width: usize) {
    for index in 0..width {
        bits[offset + index] = ((value >> index) & 1) == 1;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GeneratorRequest<'a> {
    pub start: &'a str,
    pub fps: Fps,
}

#[derive(Debug)]
pub struct LtcGenerator {
    fps: Fps,
    current_timecode: Timecode,
    frame_bits: [bool; 80],
    bit_index: usize,
    second_half: bool,
    level: f32,
    samples_per_half_bit: f64,
    fractional_samples: f64,
    samples_remaining_in_half: usize,
}

impl LtcGenerator {
    pub fn new(request: GeneratorRequest<'_>, sample_rate: u32) -> Result<Self, LtcError> {
        let current_timecode = Timecode::from_str(request.start)?;
        if current_timecode.frames >= request.fps.frame_count_base() {
            return Err(LtcError::FrameOutOfRange {
                frames: current_timecode.frames,
                fps: request.fps,
            });
        }

        let frame_bits = current_timecode.encode_ltc_bits(request.fps);
        let mut generator = Self {
            fps: request.fps,
            current_timecode,
            frame_bits,
            bit_index: 0,
            second_half: false,
            level: -LTC_PEAK_AMPLITUDE,
            samples_per_half_bit: sample_rate as f64 / (request.fps.as_f64() * 160.0),
            fractional_samples: 0.0,
            samples_remaining_in_half: 0,
        };
        generator.start_bit();
        Ok(generator)
    }

    pub fn current_timecode(&self) -> Timecode {
        self.current_timecode
    }

    pub fn next_sample(&mut self) -> f32 {
        if self.samples_remaining_in_half == 0 {
            self.advance_half_bit();
        }

        self.samples_remaining_in_half -= 1;
        self.level
    }

    fn advance_half_bit(&mut self) {
        if self.second_half {
            self.bit_index += 1;
            self.second_half = false;

            if self.bit_index == self.frame_bits.len() {
                self.current_timecode = self.current_timecode.increment(self.fps);
                self.frame_bits = self.current_timecode.encode_ltc_bits(self.fps);
                self.bit_index = 0;
            }

            self.start_bit();
            return;
        }

        if self.frame_bits[self.bit_index] {
            self.level = -self.level;
        }
        self.second_half = true;
        self.samples_remaining_in_half = self.next_half_bit_sample_count();
    }

    fn start_bit(&mut self) {
        self.level = -self.level;
        self.samples_remaining_in_half = self.next_half_bit_sample_count();
    }

    fn next_half_bit_sample_count(&mut self) -> usize {
        self.fractional_samples += self.samples_per_half_bit;
        let whole_samples = self.fractional_samples.floor() as usize;
        self.fractional_samples -= whole_samples as f64;
        whole_samples.max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LtcError {
    InvalidTimecodeFormat(String),
    FrameOutOfRange { frames: u8, fps: Fps },
}

impl fmt::Display for LtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimecodeFormat(value) => {
                write!(f, "invalid timecode '{value}'; expected HH:MM:SS:FF")
            }
            Self::FrameOutOfRange { frames, fps } => write!(
                f,
                "timecode frame value {frames} is out of range for {} fps",
                fps.as_f64()
            ),
        }
    }
}

impl std::error::Error for LtcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_timecode() {
        let timecode = Timecode::from_str("01:02:03:04").expect("timecode should parse");
        assert_eq!(timecode.hours, 1);
        assert_eq!(timecode.minutes, 2);
        assert_eq!(timecode.seconds, 3);
        assert_eq!(timecode.frames, 4);
    }

    #[test]
    fn rejects_invalid_timecode_format() {
        let error = Timecode::from_str("1:2:3:4").expect_err("format should fail");
        assert!(error.to_string().contains("expected HH:MM:SS:FF"));
    }

    #[test]
    fn increments_with_fps_frame_base() {
        let next = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 29,
        }
        .increment(Fps::Fps29_97);

        assert_eq!(
            next,
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 1,
                frames: 0,
            }
        );
    }

    #[test]
    fn generator_produces_bipolar_waveform() {
        let mut generator = LtcGenerator::new(
            GeneratorRequest {
                start: "01:00:00:00",
                fps: Fps::Fps30,
            },
            44_100,
        )
        .expect("generator should initialize");

        let mut saw_positive = false;
        let mut saw_negative = false;
        for _ in 0..1024 {
            let sample = generator.next_sample();
            saw_positive |= sample > 0.0;
            saw_negative |= sample < 0.0;
        }

        assert!(saw_positive);
        assert!(saw_negative);
    }

    #[test]
    fn generator_advances_timecode() {
        let mut generator = LtcGenerator::new(
            GeneratorRequest {
                start: "01:00:00:00",
                fps: Fps::Fps30,
            },
            44_100,
        )
        .expect("generator should initialize");

        for _ in 0..1600 {
            let _ = generator.next_sample();
        }

        assert_ne!(
            generator.current_timecode(),
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            }
        );
    }
}