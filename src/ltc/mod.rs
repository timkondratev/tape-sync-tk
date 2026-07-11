use crate::config::Fps;
use std::fmt;
use std::sync::{Arc, Mutex};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStatus {
    Unlocked,
    Locking,
    Locked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackDirection {
    Forward,
    Reverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDirection {
    Rising,
    Falling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeEvent {
    pub sample_index: usize,
    pub direction: EdgeDirection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodeStatus {
    pub lock_status: LockStatus,
    pub direction: PlaybackDirection,
    pub edge_count: usize,
    pub consecutive_valid_windows: u32,
    pub consecutive_invalid_windows: u32,
    pub current_timecode: Option<Timecode>,
    pub decoded_frame_count: u64,
    pub measured_fps: Option<f64>,
    pub smoothed_tempo_bpm: Option<f64>,
}

impl Default for DecodeStatus {
    fn default() -> Self {
        Self {
            lock_status: LockStatus::Unlocked,
            direction: PlaybackDirection::Forward,
            edge_count: 0,
            consecutive_valid_windows: 0,
            consecutive_invalid_windows: 0,
            current_timecode: None,
            decoded_frame_count: 0,
            measured_fps: None,
            smoothed_tempo_bpm: None,
        }
    }
}

pub trait DecodeStatusHandler: Send {
    fn handle_status(&mut self, status: &DecodeStatus);
}

pub type SharedDecodeStatusHandler = Arc<Mutex<Box<dyn DecodeStatusHandler>>>;

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

#[derive(Debug, Clone, Copy)]
pub struct DecodeRequest {
    pub fps: Fps,
    pub ref_fps: Fps,
    pub ref_bpm: f64,
    pub smoothing_alpha: f64,
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

#[derive(Debug)]
pub struct EdgeDetector {
    last_polarity: Option<bool>,
    deadband: f32,
}

#[derive(Debug)]
struct FrameDecoder {
    expected_half_bit_samples: f64,
    last_edge_sample: Option<usize>,
    pending_short_interval: bool,
    bits: Vec<bool>,
}

impl FrameDecoder {
    fn new(sample_rate: u32, fps: Fps) -> Self {
        Self {
            expected_half_bit_samples: sample_rate as f64 / (fps.as_f64() * 160.0),
            last_edge_sample: None,
            pending_short_interval: false,
            bits: Vec::with_capacity(96),
        }
    }

    fn push_edge(&mut self, edge_sample: usize) -> Option<[bool; 80]> {
        let previous_edge = self.last_edge_sample.replace(edge_sample)?;
        let interval = edge_sample.saturating_sub(previous_edge) as f64;
        let short_distance = (interval - self.expected_half_bit_samples).abs();
        let long_distance = (interval - self.expected_half_bit_samples * 2.0).abs();
        let tolerance = self.expected_half_bit_samples * 0.45;

        if short_distance <= tolerance {
            if self.pending_short_interval {
                self.pending_short_interval = false;
                return self.push_bit(true);
            }
            self.pending_short_interval = true;
            return None;
        }

        if long_distance <= tolerance {
            self.pending_short_interval = false;
            return self.push_bit(false);
        }

        self.pending_short_interval = false;
        self.bits.clear();
        None
    }

    fn push_bit(&mut self, bit: bool) -> Option<[bool; 80]> {
        self.bits.push(bit);
        if self.bits.len() > 80 {
            let excess = self.bits.len() - 80;
            self.bits.drain(0..excess);
        }

        if self.bits.len() == 80 && self.bits[64..80] == SYNC_WORD {
            let mut frame = [false; 80];
            frame.copy_from_slice(&self.bits[..80]);
            return Some(frame);
        }

        None
    }
}

impl Default for EdgeDetector {
    fn default() -> Self {
        Self {
            last_polarity: None,
            deadband: 0.01,
        }
    }
}

impl EdgeDetector {
    pub fn detect(&mut self, samples: &[f32]) -> Vec<EdgeEvent> {
        let mut edges = Vec::new();

        for (sample_index, sample) in samples.iter().copied().enumerate() {
            let polarity = if sample > self.deadband {
                Some(true)
            } else if sample < -self.deadband {
                Some(false)
            } else {
                None
            };

            let Some(polarity) = polarity else {
                continue;
            };

            match self.last_polarity {
                Some(previous) if previous != polarity => {
                    edges.push(EdgeEvent {
                        sample_index,
                        direction: if polarity {
                            EdgeDirection::Rising
                        } else {
                            EdgeDirection::Falling
                        },
                    });
                    self.last_polarity = Some(polarity);
                }
                None => {
                    self.last_polarity = Some(polarity);
                }
                _ => {}
            }
        }

        edges
    }
}

#[derive(Debug)]
pub struct LockTracker {
    lock_threshold: u32,
    unlock_threshold: u32,
    lock_status: LockStatus,
    consecutive_valid_windows: u32,
    consecutive_invalid_windows: u32,
}

impl Default for LockTracker {
    fn default() -> Self {
        Self::new(8, 4)
    }
}

impl LockTracker {
    pub fn new(lock_threshold: u32, unlock_threshold: u32) -> Self {
        Self {
            lock_threshold,
            unlock_threshold,
            lock_status: LockStatus::Unlocked,
            consecutive_valid_windows: 0,
            consecutive_invalid_windows: 0,
        }
    }

    pub fn observe_window(&mut self, has_activity: bool) -> DecodeStatus {
        if has_activity {
            self.consecutive_valid_windows += 1;
            self.consecutive_invalid_windows = 0;
            self.lock_status = match self.lock_status {
                LockStatus::Unlocked => LockStatus::Locking,
                LockStatus::Locking if self.consecutive_valid_windows >= self.lock_threshold => {
                    LockStatus::Locked
                }
                other => other,
            };
        } else {
            self.consecutive_invalid_windows += 1;
            self.consecutive_valid_windows = 0;
            if self.consecutive_invalid_windows >= self.unlock_threshold {
                self.lock_status = LockStatus::Unlocked;
            }
        }

        DecodeStatus {
            lock_status: self.lock_status,
            direction: PlaybackDirection::Forward,
            edge_count: 0,
            consecutive_valid_windows: self.consecutive_valid_windows,
            consecutive_invalid_windows: self.consecutive_invalid_windows,
            current_timecode: None,
            decoded_frame_count: 0,
            measured_fps: None,
            smoothed_tempo_bpm: None,
        }
    }
}

#[derive(Debug)]
pub struct DecodeMonitor {
    edge_detector: EdgeDetector,
    frame_decoder: FrameDecoder,
    lock_tracker: LockTracker,
    fps: Fps,
    ref_fps: Fps,
    ref_bpm: f64,
    smoothing_alpha: f64,
    sample_rate: u32,
    sample_offset: usize,
    decoded_frame_count: u64,
    last_timecode: Option<Timecode>,
    previous_frame_sample: Option<usize>,
    measured_fps: Option<f64>,
    smoothed_tempo_bpm: Option<f64>,
    direction: PlaybackDirection,
}

impl DecodeMonitor {
    pub fn new(request: DecodeRequest, sample_rate: u32) -> Self {
        Self {
            edge_detector: EdgeDetector::default(),
            frame_decoder: FrameDecoder::new(sample_rate, request.fps),
            lock_tracker: LockTracker::default(),
            fps: request.fps,
            ref_fps: request.ref_fps,
            ref_bpm: request.ref_bpm,
            smoothing_alpha: request.smoothing_alpha,
            sample_rate,
            sample_offset: 0,
            decoded_frame_count: 0,
            last_timecode: None,
            previous_frame_sample: None,
            measured_fps: None,
            smoothed_tempo_bpm: None,
            direction: PlaybackDirection::Forward,
        }
    }

    pub fn process_samples(&mut self, samples: &[f32]) -> DecodeStatus {
        let edges = self.edge_detector.detect(samples);
        let mut decoded_timecode = None;
        for edge in &edges {
            if let Some(bits) = self.frame_decoder.push_edge(self.sample_offset + edge.sample_index) {
                if let Ok(timecode) = decode_timecode(bits) {
                    let frame_sample = self.sample_offset + edge.sample_index;
                    self.update_timing_metrics(timecode, frame_sample);
                    self.decoded_frame_count += 1;
                    self.last_timecode = Some(timecode);
                    decoded_timecode = Some(timecode);
                }
            }
        }

        self.sample_offset += samples.len();

        let mut status = self.lock_tracker.observe_window(decoded_timecode.is_some());
        status.edge_count = edges.len();
        status.direction = self.direction;
        status.current_timecode = self.last_timecode;
        status.decoded_frame_count = self.decoded_frame_count;
        status.measured_fps = self.measured_fps;
        status.smoothed_tempo_bpm = self.smoothed_tempo_bpm;
        status
    }

    fn update_timing_metrics(&mut self, current_timecode: Timecode, frame_sample: usize) {
        if let Some(previous_timecode) = self.last_timecode {
            self.direction = infer_direction(previous_timecode, current_timecode, self.fps);
        }

        if let Some(previous_frame_sample) = self.previous_frame_sample {
            let sample_delta = frame_sample.saturating_sub(previous_frame_sample);
            if sample_delta > 0 {
                let measured_fps = self.sample_rate as f64 / sample_delta as f64;
                self.measured_fps = Some(measured_fps);

                let measured_tempo_bpm = self.ref_bpm * (measured_fps / self.ref_fps.as_f64());
                self.smoothed_tempo_bpm = Some(match self.smoothed_tempo_bpm {
                    Some(previous) => {
                        previous + self.smoothing_alpha * (measured_tempo_bpm - previous)
                    }
                    None => measured_tempo_bpm,
                });
            }
        }

        self.previous_frame_sample = Some(frame_sample);
    }
}

impl Default for DecodeMonitor {
    fn default() -> Self {
        Self::new(
            DecodeRequest {
                fps: Fps::Fps30,
                ref_fps: Fps::Fps30,
                ref_bpm: 120.0,
                smoothing_alpha: 0.15,
            },
            44_100,
        )
    }
}

fn infer_direction(previous: Timecode, current: Timecode, fps: Fps) -> PlaybackDirection {
    if current == previous.increment(fps) {
        PlaybackDirection::Forward
    } else if previous == current.increment(fps) {
        PlaybackDirection::Reverse
    } else if total_frames(current, fps) >= total_frames(previous, fps) {
        PlaybackDirection::Forward
    } else {
        PlaybackDirection::Reverse
    }
}

fn total_frames(timecode: Timecode, fps: Fps) -> i64 {
    (((timecode.hours as i64 * 60 + timecode.minutes as i64) * 60 + timecode.seconds as i64)
        * fps.frame_count_base() as i64)
        + timecode.frames as i64
}

fn decode_timecode(bits: [bool; 80]) -> Result<Timecode, LtcError> {
    if bits[64..80] != SYNC_WORD {
        return Err(LtcError::InvalidFrame("missing sync word".to_string()));
    }

    let frames = decode_bcd(&bits, 0, 4) + decode_bcd(&bits, 8, 2) * 10;
    let seconds = decode_bcd(&bits, 16, 4) + decode_bcd(&bits, 24, 3) * 10;
    let minutes = decode_bcd(&bits, 32, 4) + decode_bcd(&bits, 40, 3) * 10;
    let hours = decode_bcd(&bits, 48, 4) + decode_bcd(&bits, 56, 2) * 10;

    if hours >= 24 || minutes >= 60 || seconds >= 60 {
        return Err(LtcError::InvalidFrame("decoded BCD fields are out of range".to_string()));
    }

    Ok(Timecode {
        hours,
        minutes,
        seconds,
        frames,
    })
}

fn decode_bcd(bits: &[bool; 80], offset: usize, width: usize) -> u8 {
    let mut value = 0;
    for index in 0..width {
        if bits[offset + index] {
            value |= 1 << index;
        }
    }
    value
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
    InvalidFrame(String),
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
            Self::InvalidFrame(message) => write!(f, "invalid LTC frame: {message}"),
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

    #[test]
    fn edge_detector_reports_zero_crossings() {
        let mut detector = EdgeDetector::default();
        let edges = detector.detect(&[-0.5, -0.25, 0.3, 0.4, -0.2]);

        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].direction, EdgeDirection::Rising);
        assert_eq!(edges[1].direction, EdgeDirection::Falling);
    }

    #[test]
    fn lock_tracker_transitions_to_locked_and_back() {
        let mut tracker = LockTracker::new(2, 2);

        assert_eq!(tracker.observe_window(true).lock_status, LockStatus::Locking);
        assert_eq!(tracker.observe_window(true).lock_status, LockStatus::Locked);
        assert_eq!(tracker.observe_window(false).lock_status, LockStatus::Locked);
        assert_eq!(tracker.observe_window(false).lock_status, LockStatus::Unlocked);
    }

    #[test]
    fn decode_monitor_counts_edges_in_active_window() {
        let mut monitor = DecodeMonitor::new(
            DecodeRequest {
                fps: Fps::Fps30,
                ref_fps: Fps::Fps30,
                ref_bpm: 120.0,
                smoothing_alpha: 0.15,
            },
            44_100,
        );
        let status = monitor.process_samples(&[-0.5, 0.5, -0.5, 0.5]);

        assert_eq!(status.lock_status, LockStatus::Unlocked);
        assert!(status.edge_count >= 2);
    }

    #[test]
    fn decode_monitor_recovers_generated_frames() {
        let mut generator = LtcGenerator::new(
            GeneratorRequest {
                start: "01:00:00:00",
                fps: Fps::Fps30,
            },
            44_100,
        )
        .expect("generator should initialize");
        let mut monitor = DecodeMonitor::new(
            DecodeRequest {
                fps: Fps::Fps30,
                ref_fps: Fps::Fps30,
                ref_bpm: 120.0,
                smoothing_alpha: 0.15,
            },
            44_100,
        );

        let mut final_status = DecodeStatus::default();
        for _ in 0..60 {
            let samples = (0..256)
                .map(|_| generator.next_sample())
                .collect::<Vec<_>>();
            final_status = monitor.process_samples(&samples);
        }

        assert!(final_status.decoded_frame_count > 0);
        assert!(final_status.current_timecode.is_some());
        assert!(final_status.measured_fps.is_some());
        assert!(final_status.smoothed_tempo_bpm.is_some());
        assert!(matches!(
            final_status.lock_status,
            LockStatus::Locking | LockStatus::Locked
        ));
    }

    #[test]
    fn infers_reverse_direction_for_previous_frame() {
        let previous = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 1,
        };
        let current = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };

        assert_eq!(infer_direction(previous, current, Fps::Fps30), PlaybackDirection::Reverse);
    }
}