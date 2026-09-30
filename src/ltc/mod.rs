use crate::config::Fps;
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

const LTC_PEAK_AMPLITUDE: f32 = 0.501_187_2;
const SYNC_WORD: [bool; 16] = [
    false, false, true, true, true, true, true, true, true, true, true, true, true, true, false,
    true,
];

const DEFAULT_FPS_ESTIMATE_WINDOW_SIZE: usize = 12;
const DEFAULT_DROPOUT_RESET_WINDOWS: u32 = 8;

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
    let value =
        part.ok_or_else(|| LtcError::InvalidTimecodeFormat("missing component".to_string()))?;
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
    pub fps_estimate_window_size: usize,
    pub dropout_reset_windows: u32,
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
    nominal_half_bit_samples: f64,
    expected_half_bit_samples: f64,
    last_edge_sample: Option<usize>,
    last_edge_direction: Option<EdgeDirection>,
    pending_short_interval: bool,
    bits: Vec<bool>,
}

const MIN_TRACKED_SPEED_RATIO: f64 = 0.4;
const MAX_TRACKED_SPEED_RATIO: f64 = 2.5;
const SHORT_INTERVAL_MIN_RATIO: f64 = 0.55;
const SHORT_INTERVAL_MAX_RATIO: f64 = 1.45;
const LONG_INTERVAL_MIN_RATIO: f64 = 1.55;
const LONG_INTERVAL_MAX_RATIO: f64 = 2.45;
const TIMING_TRACK_ALPHA: f64 = 0.12;

#[derive(Debug)]
struct LtcPreFilter {
    alpha: f32,
    previous_input: f32,
    previous_output: f32,
}

impl LtcPreFilter {
    fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate.max(1) as f32;
        let cutoff_hz = 20.0;
        let dt = 1.0 / sample_rate;
        let rc = 1.0 / (2.0 * PI * cutoff_hz);
        let alpha = rc / (rc + dt);

        Self {
            alpha,
            previous_input: 0.0,
            previous_output: 0.0,
        }
    }

    fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut filtered = Vec::with_capacity(samples.len());
        for sample in samples.iter().copied() {
            let output = self.alpha * (self.previous_output + sample - self.previous_input);
            self.previous_input = sample;
            self.previous_output = output;
            filtered.push(output);
        }

        filtered
    }
}

impl FrameDecoder {
    fn new(sample_rate: u32, fps: Fps) -> Self {
        let half_bit_samples = sample_rate as f64 / (fps.as_f64() * 160.0);
        Self {
            nominal_half_bit_samples: half_bit_samples,
            expected_half_bit_samples: half_bit_samples,
            last_edge_sample: None,
            last_edge_direction: None,
            pending_short_interval: false,
            bits: Vec::with_capacity(96),
        }
    }

    fn push_edge(&mut self, edge_sample: usize, direction: EdgeDirection) -> Option<[bool; 80]> {
        if self.last_edge_direction == Some(direction) {
            self.reset_timing();
            return None;
        }

        self.last_edge_direction = Some(direction);
        let previous_edge = self.last_edge_sample.replace(edge_sample)?;
        let interval = edge_sample.saturating_sub(previous_edge) as f64;
        let ratio = interval / self.expected_half_bit_samples;

        if (SHORT_INTERVAL_MIN_RATIO..=SHORT_INTERVAL_MAX_RATIO).contains(&ratio) {
            self.track_half_bit(interval);
            if self.pending_short_interval {
                self.pending_short_interval = false;
                return self.push_bit(true);
            }
            self.pending_short_interval = true;
            return None;
        }

        if (LONG_INTERVAL_MIN_RATIO..=LONG_INTERVAL_MAX_RATIO).contains(&ratio) {
            self.track_half_bit(interval / 2.0);
            self.pending_short_interval = false;
            return self.push_bit(false);
        }

        self.reset_timing();
        None
    }

    fn reset_timing(&mut self) {
        self.last_edge_sample = None;
        self.last_edge_direction = None;
        self.pending_short_interval = false;
        self.bits.clear();
    }

    fn track_half_bit(&mut self, observed_half_bit_samples: f64) {
        let min_half = self.nominal_half_bit_samples / MAX_TRACKED_SPEED_RATIO;
        let max_half = self.nominal_half_bit_samples / MIN_TRACKED_SPEED_RATIO;
        let clamped = observed_half_bit_samples.clamp(min_half, max_half);
        self.expected_half_bit_samples +=
            TIMING_TRACK_ALPHA * (clamped - self.expected_half_bit_samples);
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

    pub fn status(&self) -> DecodeStatus {
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
    pre_filter: LtcPreFilter,
    frame_decoder: FrameDecoder,
    lock_tracker: LockTracker,
    fps: Fps,
    ref_fps: Fps,
    ref_bpm: f64,
    smoothing_alpha: f64,
    sample_rate: u32,
    nominal_samples_per_frame: f64,
    sample_offset: usize,
    decoded_frame_count: u64,
    last_timecode: Option<Timecode>,
    previous_frame_sample: Option<usize>,
    last_instantaneous_fps: Option<f64>,
    recent_fps_samples: VecDeque<f64>,
    fps_estimate_window_size: usize,
    dropout_reset_windows: u32,
    measured_fps: Option<f64>,
    smoothed_tempo_bpm: Option<f64>,
    direction: PlaybackDirection,
    windows_without_decoded_frame: u32,
    samples_since_lock_activity: f64,
}

impl DecodeMonitor {
    pub fn new(request: DecodeRequest, sample_rate: u32) -> Self {
        Self {
            edge_detector: EdgeDetector::default(),
            pre_filter: LtcPreFilter::new(sample_rate),
            frame_decoder: FrameDecoder::new(sample_rate, request.fps),
            lock_tracker: LockTracker::default(),
            fps: request.fps,
            ref_fps: request.ref_fps,
            ref_bpm: request.ref_bpm,
            smoothing_alpha: request.smoothing_alpha,
            sample_rate,
            nominal_samples_per_frame: sample_rate as f64 / request.fps.as_f64(),
            sample_offset: 0,
            decoded_frame_count: 0,
            last_timecode: None,
            previous_frame_sample: None,
            last_instantaneous_fps: None,
            recent_fps_samples: VecDeque::with_capacity(request.fps_estimate_window_size),
            fps_estimate_window_size: request.fps_estimate_window_size,
            dropout_reset_windows: request.dropout_reset_windows,
            measured_fps: None,
            smoothed_tempo_bpm: None,
            direction: PlaybackDirection::Forward,
            windows_without_decoded_frame: 0,
            samples_since_lock_activity: 0.0,
        }
    }

    pub fn process_samples(&mut self, samples: &[f32]) -> DecodeStatus {
        let filtered_samples = self.pre_filter.process(samples);
        let edges = self.edge_detector.detect(&filtered_samples);
        let mut decoded_timecode = None;
        for edge in &edges {
            if let Some(bits) = self
                .frame_decoder
                .push_edge(self.sample_offset + edge.sample_index, edge.direction)
            {
                if let Ok(timecode) = decode_timecode(bits, self.fps) {
                    let frame_sample = self.sample_offset + edge.sample_index;
                    self.update_timing_metrics(timecode, frame_sample);
                    self.decoded_frame_count += 1;
                    self.last_timecode = Some(timecode);
                    decoded_timecode = Some(timecode);
                }
            }
        }

        if decoded_timecode.is_some() {
            self.windows_without_decoded_frame = 0;
        } else {
            self.windows_without_decoded_frame += 1;
            if self.windows_without_decoded_frame >= self.dropout_reset_windows {
                self.recent_fps_samples.clear();
                self.last_instantaneous_fps = None;
                self.measured_fps = None;
                self.smoothed_tempo_bpm = None;
            }
        }

        self.sample_offset += samples.len();

        let mut status = self.observe_lock_activity(decoded_timecode.is_some(), samples.len());
        self.update_smoothed_tempo(status.lock_status);
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
                let instantaneous_fps = self.sample_rate as f64 / sample_delta as f64;
                self.last_instantaneous_fps = Some(instantaneous_fps);
                self.recent_fps_samples.push_back(instantaneous_fps);
                while self.recent_fps_samples.len() > self.fps_estimate_window_size {
                    self.recent_fps_samples.pop_front();
                }

                let measured_fps = self.recent_fps_samples.iter().sum::<f64>()
                    / self.recent_fps_samples.len() as f64;
                self.measured_fps = Some(measured_fps);
            }
        }

        self.previous_frame_sample = Some(frame_sample);
    }

    fn observe_lock_activity(
        &mut self,
        decoded_frame_in_chunk: bool,
        processed_samples: usize,
    ) -> DecodeStatus {
        if decoded_frame_in_chunk {
            self.samples_since_lock_activity = 0.0;
            return self.lock_tracker.observe_window(true);
        }

        self.samples_since_lock_activity += processed_samples as f64;
        if self.samples_since_lock_activity >= self.nominal_samples_per_frame {
            self.samples_since_lock_activity -= self.nominal_samples_per_frame;
            return self.lock_tracker.observe_window(false);
        }

        self.lock_tracker.status()
    }

    fn update_smoothed_tempo(&mut self, lock_status: LockStatus) {
        let instantaneous_fps = match self.last_instantaneous_fps {
            Some(value) => value,
            None => return,
        };

        let instantaneous_tempo_bpm = self.ref_bpm * (instantaneous_fps / self.ref_fps.as_f64());

        self.smoothed_tempo_bpm = Some(match lock_status {
            // During acquisition, prefer immediate tempo readout to avoid startup ramp.
            LockStatus::Unlocked | LockStatus::Locking => instantaneous_tempo_bpm,
            LockStatus::Locked => match self.smoothed_tempo_bpm {
                Some(previous) => {
                    previous + self.smoothing_alpha * (instantaneous_tempo_bpm - previous)
                }
                None => instantaneous_tempo_bpm,
            },
        });
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
                fps_estimate_window_size: DEFAULT_FPS_ESTIMATE_WINDOW_SIZE,
                dropout_reset_windows: DEFAULT_DROPOUT_RESET_WINDOWS,
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

fn decode_timecode(bits: [bool; 80], fps: Fps) -> Result<Timecode, LtcError> {
    if bits[64..80] != SYNC_WORD {
        return Err(LtcError::InvalidFrame("missing sync word".to_string()));
    }

    let frame_units = decode_bcd(&bits, 0, 4);
    let frame_tens = decode_bcd(&bits, 8, 2);
    let second_units = decode_bcd(&bits, 16, 4);
    let second_tens = decode_bcd(&bits, 24, 3);
    let minute_units = decode_bcd(&bits, 32, 4);
    let minute_tens = decode_bcd(&bits, 40, 3);
    let hour_units = decode_bcd(&bits, 48, 4);
    let hour_tens = decode_bcd(&bits, 56, 2);

    validate_bcd_digit(frame_units, 9, "frame units")?;
    validate_bcd_digit(frame_tens, 2, "frame tens")?;
    validate_bcd_digit(second_units, 9, "second units")?;
    validate_bcd_digit(second_tens, 5, "second tens")?;
    validate_bcd_digit(minute_units, 9, "minute units")?;
    validate_bcd_digit(minute_tens, 5, "minute tens")?;
    validate_bcd_digit(hour_units, 9, "hour units")?;
    validate_bcd_digit(hour_tens, 2, "hour tens")?;

    let frames = frame_units + frame_tens * 10;
    let seconds = second_units + second_tens * 10;
    let minutes = minute_units + minute_tens * 10;
    let hours = hour_units + hour_tens * 10;

    if hours >= 24 || minutes >= 60 || seconds >= 60 {
        return Err(LtcError::InvalidFrame(
            "decoded BCD fields are out of range".to_string(),
        ));
    }

    if frames >= fps.frame_count_base() {
        return Err(LtcError::InvalidFrame(format!(
            "decoded frame value {frames} is out of range for {} fps",
            fps.as_f64()
        )));
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

fn validate_bcd_digit(value: u8, maximum: u8, field: &'static str) -> Result<(), LtcError> {
    if value > maximum {
        return Err(LtcError::InvalidFrame(format!(
            "{field} digit {value} is out of range"
        )));
    }

    Ok(())
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
    fn frame_decoder_rejects_repeated_edge_direction_glitches() {
        let mut decoder = FrameDecoder::new(44_100, Fps::Fps30);

        assert!(decoder.push_edge(0, EdgeDirection::Rising).is_none());
        assert!(decoder.push_edge(11, EdgeDirection::Falling).is_none());
        assert!(decoder.pending_short_interval);

        assert!(decoder.push_edge(22, EdgeDirection::Falling).is_none());
        assert!(!decoder.pending_short_interval);
        assert!(decoder.bits.is_empty());
        assert!(decoder.last_edge_sample.is_none());
        assert!(decoder.last_edge_direction.is_none());
    }

    #[test]
    fn frame_decoder_tracks_faster_playback_speed() {
        let mut decoder = FrameDecoder::new(44_100, Fps::Fps30);

        assert!(decoder.push_edge(0, EdgeDirection::Rising).is_none());

        // About 1.33x speed: short interval around 0.75x nominal.
        let short_interval = 7;
        let mut sample = 0usize;
        let mut direction = EdgeDirection::Rising;
        for _ in 0..48 {
            sample += short_interval;
            direction = match direction {
                EdgeDirection::Rising => EdgeDirection::Falling,
                EdgeDirection::Falling => EdgeDirection::Rising,
            };
            let _ = decoder.push_edge(sample, direction);
        }

        assert!(
            decoder.expected_half_bit_samples < decoder.nominal_half_bit_samples,
            "expected half-bit should move lower at faster playback"
        );
    }

    #[test]
    fn lock_tracker_transitions_to_locked_and_back() {
        let mut tracker = LockTracker::new(2, 2);

        assert_eq!(
            tracker.observe_window(true).lock_status,
            LockStatus::Locking
        );
        assert_eq!(tracker.observe_window(true).lock_status, LockStatus::Locked);
        assert_eq!(
            tracker.observe_window(false).lock_status,
            LockStatus::Locked
        );
        assert_eq!(
            tracker.observe_window(false).lock_status,
            LockStatus::Unlocked
        );
    }

    #[test]
    fn decode_monitor_counts_edges_in_active_window() {
        let mut monitor = DecodeMonitor::new(
            DecodeRequest {
                fps: Fps::Fps30,
                ref_fps: Fps::Fps30,
                ref_bpm: 120.0,
                smoothing_alpha: 0.15,
                fps_estimate_window_size: DEFAULT_FPS_ESTIMATE_WINDOW_SIZE,
                dropout_reset_windows: DEFAULT_DROPOUT_RESET_WINDOWS,
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
                fps_estimate_window_size: DEFAULT_FPS_ESTIMATE_WINDOW_SIZE,
                dropout_reset_windows: DEFAULT_DROPOUT_RESET_WINDOWS,
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
    fn decode_monitor_reaches_locked_with_small_audio_callbacks() {
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
                fps_estimate_window_size: DEFAULT_FPS_ESTIMATE_WINDOW_SIZE,
                dropout_reset_windows: DEFAULT_DROPOUT_RESET_WINDOWS,
            },
            44_100,
        );

        let mut final_status = DecodeStatus::default();
        for _ in 0..500 {
            let samples = (0..128)
                .map(|_| generator.next_sample())
                .collect::<Vec<_>>();
            final_status = monitor.process_samples(&samples);
            if final_status.lock_status == LockStatus::Locked {
                break;
            }
        }

        assert_eq!(final_status.lock_status, LockStatus::Locked);
    }

    #[test]
    fn startup_acquisition_bypasses_smoothing_until_locked() {
        let mut monitor = DecodeMonitor::new(
            DecodeRequest {
                fps: Fps::Fps30,
                ref_fps: Fps::Fps30,
                ref_bpm: 120.0,
                smoothing_alpha: 0.01,
                fps_estimate_window_size: DEFAULT_FPS_ESTIMATE_WINDOW_SIZE,
                dropout_reset_windows: DEFAULT_DROPOUT_RESET_WINDOWS,
            },
            44_100,
        );

        monitor.last_instantaneous_fps = Some(45.0);
        monitor.smoothed_tempo_bpm = Some(100.0);
        monitor.update_smoothed_tempo(LockStatus::Locking);

        let expected = 120.0 * (45.0 / 30.0);
        let actual = monitor
            .smoothed_tempo_bpm
            .expect("tempo should be set during acquisition");
        assert!((actual - expected).abs() < 1e-9);
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

        assert_eq!(
            infer_direction(previous, current, Fps::Fps30),
            PlaybackDirection::Reverse
        );
    }

    #[test]
    fn clears_timing_metrics_after_decode_dropout() {
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
                fps_estimate_window_size: DEFAULT_FPS_ESTIMATE_WINDOW_SIZE,
                dropout_reset_windows: DEFAULT_DROPOUT_RESET_WINDOWS,
            },
            44_100,
        );

        for _ in 0..40 {
            let samples = (0..256)
                .map(|_| generator.next_sample())
                .collect::<Vec<_>>();
            let _ = monitor.process_samples(&samples);
        }
        assert!(monitor.measured_fps.is_some());

        for _ in 0..DEFAULT_DROPOUT_RESET_WINDOWS {
            let _ = monitor.process_samples(&[0.0; 256]);
        }

        assert!(monitor.measured_fps.is_none());
        assert!(monitor.smoothed_tempo_bpm.is_none());
    }

    #[test]
    fn decode_timecode_rejects_frame_values_out_of_range_for_fps() {
        let bits = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 29,
        }
        .encode_ltc_bits(Fps::Fps30);

        let error = decode_timecode(bits, Fps::Fps24).expect_err("frame should be rejected");
        assert!(error.to_string().contains("out of range for 24 fps"));
    }

    #[test]
    fn decode_timecode_rejects_invalid_bcd_digits() {
        let mut bits = Timecode {
            hours: 1,
            minutes: 2,
            seconds: 3,
            frames: 4,
        }
        .encode_ltc_bits(Fps::Fps30);

        bits[1] = true;
        bits[3] = true;

        let error = decode_timecode(bits, Fps::Fps30).expect_err("invalid BCD should fail");
        assert!(error.to_string().contains("frame units digit"));
    }
}
