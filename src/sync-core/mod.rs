use crate::config::Fps;
use crate::ltc::{
    DecodeStatus, DecodeStatusHandler, LockStatus, PlaybackDirection, Timecode, forward_frame_delta,
};
use crate::midi::MidiTransport;
use crate::runtime::RuntimeError;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const INACTIVE_SCHEDULER_POLL: Duration = Duration::from_millis(100);
const LATE_TICK_THRESHOLD: Duration = Duration::from_millis(2);
const CLOCK_HOLDOVER: Duration = Duration::from_millis(500);
const PHASE_CORRECTION_HORIZON: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncUpdate {
    pub lock_status: LockStatus,
    pub direction: PlaybackDirection,
    pub song_position_pointer: u16,
    pub emit_clock_tick: bool,
}

pub struct SyncEngine<P> {
    midi: P,
    send_clock: bool,
    send_transport: bool,
    last_lock_status: LockStatus,
}

impl<P: MidiTransport> SyncEngine<P> {
    pub fn new(midi: P, send_clock: bool, send_transport: bool) -> Self {
        Self {
            midi,
            send_clock,
            send_transport,
            last_lock_status: LockStatus::Unlocked,
        }
    }

    pub fn apply_update(&mut self, update: SyncUpdate) -> Result<(), RuntimeError> {
        let forward = update.direction == PlaybackDirection::Forward;

        if self.send_transport
            && forward
            && self.last_lock_status != LockStatus::Locked
            && update.lock_status == LockStatus::Locked
        {
            self.midi
                .send_song_position_pointer(update.song_position_pointer)?;
            if update.song_position_pointer == 0 {
                self.midi.send_start()?;
            } else {
                self.midi.send_continue()?;
            }
        }

        if self.send_transport
            && self.last_lock_status == LockStatus::Locked
            && update.lock_status == LockStatus::Unlocked
        {
            self.midi.send_stop()?;
        }

        if self.send_clock
            && forward
            && update.lock_status == LockStatus::Locked
            && update.emit_clock_tick
        {
            self.midi.send_clock()?;
        }

        self.last_lock_status = update.lock_status;
        Ok(())
    }

    pub fn into_midi(self) -> P {
        self.midi
    }
}

pub struct DecodeSyncBridge<P> {
    engine: SyncEngine<P>,
    ref_bpm: f64,
    ltc_fps: Fps,
    anchor_timecode: Timecode,
    latency_ms: f64,
    last_decoded_frame_count: u64,
    last_timecode: Option<Timecode>,
    clock_accumulator: f64,
    last_lock_status: LockStatus,
}

#[derive(Debug, Clone, Copy)]
struct SchedulerUpdate {
    sync: SyncUpdate,
    tempo_bpm: f64,
    phase_on_lock: f64,
    decoded_frame_count: u64,
    source_clock_position: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
enum PhaseDecision {
    Initialize,
    Slew {
        error_seconds: f64,
        correction_seconds: f64,
    },
    Discontinuity {
        error_seconds: f64,
    },
}

#[derive(Debug, Clone, Copy)]
struct PhaseObservation {
    at: Instant,
    tracked_clock_position: f64,
    clocks_per_second: f64,
}

#[derive(Debug, Default)]
struct SourcePhaseTracker {
    previous: Option<PhaseObservation>,
}

impl SourcePhaseTracker {
    fn observe(
        &mut self,
        now: Instant,
        source_clock_position: f64,
        tempo_bpm: f64,
    ) -> PhaseDecision {
        let clocks_per_second = tempo_bpm * 24.0 / 60.0;
        if !source_clock_position.is_finite()
            || !clocks_per_second.is_finite()
            || clocks_per_second <= 0.0
        {
            return PhaseDecision::Initialize;
        }

        let Some(previous) = self.previous else {
            self.previous = Some(PhaseObservation {
                at: now,
                tracked_clock_position: source_clock_position,
                clocks_per_second,
            });
            return PhaseDecision::Initialize;
        };

        let elapsed = now.saturating_duration_since(previous.at);
        let predicted_clock_position =
            previous.tracked_clock_position + elapsed.as_secs_f64() * previous.clocks_per_second;
        let error_clocks = source_clock_position - predicted_clock_position;
        let error_seconds = error_clocks / clocks_per_second;
        let discontinuity_threshold = (60.0 / tempo_bpm).min(0.5);

        if error_seconds.abs() >= discontinuity_threshold {
            self.previous = Some(PhaseObservation {
                at: now,
                tracked_clock_position: source_clock_position,
                clocks_per_second,
            });
            return PhaseDecision::Discontinuity { error_seconds };
        }

        let correction_fraction =
            (elapsed.as_secs_f64() / PHASE_CORRECTION_HORIZON.as_secs_f64()).clamp(0.0, 1.0);
        let correction_clocks = error_clocks * correction_fraction;
        self.previous = Some(PhaseObservation {
            at: now,
            tracked_clock_position: predicted_clock_position + correction_clocks,
            clocks_per_second,
        });
        PhaseDecision::Slew {
            error_seconds,
            correction_seconds: correction_clocks / clocks_per_second,
        }
    }
}

#[derive(Debug, Default)]
struct SchedulerHoldover {
    last_decoded_frame_count: u64,
    last_frame_at: Option<Instant>,
    pending_unlock: Option<SyncUpdate>,
}

impl SchedulerHoldover {
    fn observe(
        &mut self,
        update: SchedulerUpdate,
        now: Instant,
        currently_locked: bool,
    ) -> Option<SyncUpdate> {
        if update.decoded_frame_count > self.last_decoded_frame_count {
            self.last_decoded_frame_count = update.decoded_frame_count;
            self.last_frame_at = Some(now);
        }

        let within_holdover = self
            .last_frame_at
            .is_some_and(|last_frame| now.saturating_duration_since(last_frame) < CLOCK_HOLDOVER);
        if currently_locked
            && update.sync.lock_status == LockStatus::Unlocked
            && update.sync.direction == PlaybackDirection::Forward
            && within_holdover
        {
            self.pending_unlock = Some(update.sync);
            return None;
        }

        self.pending_unlock = None;
        Some(update.sync)
    }

    fn take_expired(&mut self, now: Instant) -> Option<SyncUpdate> {
        let deadline = self.last_frame_at? + CLOCK_HOLDOVER;
        if now < deadline {
            return None;
        }
        self.pending_unlock.take()
    }

    fn wait_duration(&self, now: Instant) -> Option<Duration> {
        self.pending_unlock.and_then(|_| {
            self.last_frame_at
                .map(|last_frame| (last_frame + CLOCK_HOLDOVER).saturating_duration_since(now))
        })
    }

    fn is_active(&self) -> bool {
        self.pending_unlock.is_some()
    }
}

#[derive(Debug)]
struct ClockTimeline {
    period: Duration,
    next_tick: Option<Instant>,
}

impl Default for ClockTimeline {
    fn default() -> Self {
        Self {
            period: Duration::ZERO,
            next_tick: None,
        }
    }
}

impl ClockTimeline {
    fn update(&mut self, now: Instant, active: bool, tempo_bpm: f64, phase_on_lock: f64) {
        if !active || !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
            self.next_tick = None;
            return;
        }

        let new_period = Duration::from_secs_f64(60.0 / (tempo_bpm * 24.0));
        self.next_tick = Some(match self.next_tick {
            Some(next_tick) if !self.period.is_zero() => {
                let remaining_fraction = next_tick.saturating_duration_since(now).as_secs_f64()
                    / self.period.as_secs_f64();
                now + new_period.mul_f64(remaining_fraction.clamp(0.0, 1.0))
            }
            _ => now + new_period.mul_f64((1.0 - phase_on_lock).clamp(f64::EPSILON, 1.0)),
        });
        self.period = new_period;
    }

    fn take_due(&mut self, now: Instant) -> Option<Instant> {
        let deadline = self.next_tick?;
        if deadline > now {
            return None;
        }

        let following = deadline + self.period;
        self.next_tick = Some(if following <= now {
            now + self.period
        } else {
            following
        });
        Some(deadline)
    }

    fn wait_duration(&self, now: Instant) -> Option<Duration> {
        self.next_tick
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn slew_phase(&mut self, now: Instant, correction_seconds: f64) {
        let Some(next_tick) = self.next_tick else {
            return;
        };
        let maximum_shift = self.period.as_secs_f64() / 2.0;
        let shift = (-correction_seconds).clamp(-maximum_shift, maximum_shift);
        self.next_tick = Some(if shift < 0.0 {
            next_tick
                .checked_sub(Duration::from_secs_f64(-shift))
                .unwrap_or(now)
                .max(now)
        } else {
            next_tick + Duration::from_secs_f64(shift)
        });
    }
}

struct ScheduledDecodeSyncBridge {
    sender: SyncSender<SchedulerUpdate>,
    health: Arc<SchedulerHealth>,
    ref_bpm: f64,
    ltc_fps: Fps,
    anchor_timecode: Timecode,
    latency_ms: f64,
    last_sent_frame_count: u64,
}

impl DecodeStatusHandler for ScheduledDecodeSyncBridge {
    fn handle_status(&mut self, status: &DecodeStatus) {
        let tempo_bpm = status.smoothed_tempo_bpm.unwrap_or(self.ref_bpm);
        let has_new_frame = status.decoded_frame_count > self.last_sent_frame_count;
        self.last_sent_frame_count = status.decoded_frame_count;
        let source_clock_position = if has_new_frame {
            status.current_timecode.map(|timecode| {
                musical_clock_position(
                    timecode,
                    self.anchor_timecode,
                    self.ltc_fps.as_f64(),
                    self.ref_bpm,
                    self.latency_ms,
                )
            })
        } else {
            None
        };
        let song_position_pointer = status
            .current_timecode
            .map(|timecode| {
                song_position_pointer(
                    timecode,
                    self.anchor_timecode,
                    self.ltc_fps.as_f64(),
                    self.ref_bpm,
                    self.latency_ms,
                )
            })
            .unwrap_or(0);
        let update = SchedulerUpdate {
            sync: SyncUpdate {
                lock_status: status.lock_status,
                direction: status.direction,
                song_position_pointer,
                emit_clock_tick: false,
            },
            tempo_bpm,
            phase_on_lock: source_clock_position
                .map(|position| position.rem_euclid(1.0))
                .unwrap_or_else(|| latency_phase(self.latency_ms, tempo_bpm)),
            decoded_frame_count: status.decoded_frame_count,
            source_clock_position,
        };

        match self.sender.try_send(update) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.health
                    .dropped_update_count
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.health.disconnected.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerStatus {
    pub running: bool,
    pub disconnected: bool,
    pub holdover_active: bool,
    pub dropped_update_count: u64,
    pub late_tick_count: u64,
    pub phase_error_micros: i64,
    pub discontinuity_count: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
struct SchedulerHealth {
    running: AtomicBool,
    disconnected: AtomicBool,
    holdover_active: AtomicBool,
    dropped_update_count: AtomicU64,
    late_tick_count: AtomicU64,
    phase_error_micros: AtomicI64,
    discontinuity_count: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl SchedulerHealth {
    fn snapshot(&self) -> SchedulerStatus {
        SchedulerStatus {
            running: self.running.load(Ordering::Relaxed),
            disconnected: self.disconnected.load(Ordering::Relaxed),
            holdover_active: self.holdover_active.load(Ordering::Relaxed),
            dropped_update_count: self.dropped_update_count.load(Ordering::Relaxed),
            late_tick_count: self.late_tick_count.load(Ordering::Relaxed),
            phase_error_micros: self.phase_error_micros.load(Ordering::Relaxed),
            discontinuity_count: self.discontinuity_count.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|error| error.clone()),
        }
    }

    fn record_error(&self, error: RuntimeError) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = Some(error.to_string());
        }
    }
}

#[derive(Debug)]
pub struct SchedulerRuntime {
    shutdown: Arc<AtomicBool>,
    health: Arc<SchedulerHealth>,
    worker: Option<JoinHandle<()>>,
}

impl SchedulerRuntime {
    pub fn status(&self) -> SchedulerStatus {
        self.health.snapshot()
    }
}

impl Drop for SchedulerRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn spawn_scheduled_decode_sync_handler<P>(
    midi: P,
    ref_bpm: f64,
    ltc_fps: Fps,
    anchor_timecode: Timecode,
    latency_ms: f64,
    send_clock: bool,
    send_transport: bool,
) -> Result<(crate::ltc::SharedDecodeStatusHandler, SchedulerRuntime), RuntimeError>
where
    P: MidiTransport + Send + 'static,
{
    let (sender, receiver) = sync_channel(1);
    let shutdown = Arc::new(AtomicBool::new(false));
    let health = Arc::new(SchedulerHealth::default());
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_health = Arc::clone(&health);
    let worker = thread::Builder::new()
        .name("tape-sync-midi-clock".to_string())
        .spawn(move || {
            run_scheduler(
                receiver,
                SyncEngine::new(midi, send_clock, send_transport),
                worker_shutdown,
                worker_health,
            )
        })
        .map_err(|source| {
            RuntimeError::Midi(format!("failed to start clock scheduler: {source}"))
        })?;
    health.running.store(true, Ordering::Relaxed);

    let handler = Arc::new(Mutex::new(Box::new(ScheduledDecodeSyncBridge {
        sender,
        health: Arc::clone(&health),
        ref_bpm,
        ltc_fps,
        anchor_timecode,
        latency_ms,
        last_sent_frame_count: 0,
    }) as Box<dyn DecodeStatusHandler>));
    let runtime = SchedulerRuntime {
        shutdown,
        health,
        worker: Some(worker),
    };
    Ok((handler, runtime))
}

fn run_scheduler<P: MidiTransport>(
    receiver: Receiver<SchedulerUpdate>,
    mut engine: SyncEngine<P>,
    shutdown: Arc<AtomicBool>,
    health: Arc<SchedulerHealth>,
) {
    let mut timeline = ClockTimeline::default();
    let mut holdover = SchedulerHoldover::default();
    let mut phase_tracker = SourcePhaseTracker::default();
    let mut current_update = SyncUpdate {
        lock_status: LockStatus::Unlocked,
        direction: PlaybackDirection::Forward,
        song_position_pointer: 0,
        emit_clock_tick: false,
    };

    while !shutdown.load(Ordering::Relaxed) {
        let now = Instant::now();
        if let Some(expired_unlock) = holdover.take_expired(now) {
            current_update = expired_unlock;
            health.holdover_active.store(false, Ordering::Relaxed);
            if let Err(error) = engine.apply_update(current_update) {
                health.record_error(error);
                break;
            }
            timeline.update(now, false, 0.0, 0.0);
        }
        if let Some(deadline) = timeline.take_due(now) {
            if now.saturating_duration_since(deadline) > LATE_TICK_THRESHOLD {
                health.late_tick_count.fetch_add(1, Ordering::Relaxed);
            }
            if let Err(error) = engine.apply_update(SyncUpdate {
                emit_clock_tick: true,
                ..current_update
            }) {
                health.record_error(error);
                break;
            }
        }

        let now = Instant::now();
        let clock_wait = timeline.wait_duration(now);
        let holdover_wait = holdover.wait_duration(now);
        let wait = match (clock_wait, holdover_wait) {
            (Some(clock), Some(holdover)) => Some(clock.min(holdover)),
            (Some(clock), None) => Some(clock),
            (None, Some(holdover)) => Some(holdover),
            (None, None) => None,
        };
        let received = match wait {
            Some(wait) => receiver.recv_timeout(wait).map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => TryRecvError::Empty,
                std::sync::mpsc::RecvTimeoutError::Disconnected => TryRecvError::Disconnected,
            }),
            None => receiver
                .recv_timeout(INACTIVE_SCHEDULER_POLL)
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => TryRecvError::Empty,
                    std::sync::mpsc::RecvTimeoutError::Disconnected => TryRecvError::Disconnected,
                }),
        };

        match received {
            Ok(update) => {
                let now = Instant::now();
                let Some(sync_update) = holdover.observe(
                    update,
                    now,
                    current_update.lock_status == LockStatus::Locked,
                ) else {
                    health.holdover_active.store(true, Ordering::Relaxed);
                    continue;
                };
                health
                    .holdover_active
                    .store(holdover.is_active(), Ordering::Relaxed);
                let phase_decision = update
                    .source_clock_position
                    .map(|position| phase_tracker.observe(now, position, update.tempo_bpm));
                let was_locked = current_update.lock_status == LockStatus::Locked;
                if let Some(PhaseDecision::Discontinuity { error_seconds }) = phase_decision {
                    health.phase_error_micros.store(
                        (error_seconds * 1_000_000.0).round() as i64,
                        Ordering::Relaxed,
                    );
                    if was_locked && sync_update.lock_status == LockStatus::Locked {
                        let stop = SyncUpdate {
                            lock_status: LockStatus::Unlocked,
                            emit_clock_tick: false,
                            ..sync_update
                        };
                        if let Err(error) = engine.apply_update(stop) {
                            health.record_error(error);
                            break;
                        }
                        timeline.update(now, false, 0.0, 0.0);
                        health.discontinuity_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
                current_update = sync_update;
                if let Err(error) = engine.apply_update(current_update) {
                    health.record_error(error);
                    break;
                }
                let active = current_update.lock_status == LockStatus::Locked
                    && current_update.direction == PlaybackDirection::Forward;
                timeline.update(now, active, update.tempo_bpm, update.phase_on_lock);
                match phase_decision {
                    Some(PhaseDecision::Slew {
                        error_seconds,
                        correction_seconds,
                    }) => {
                        health.phase_error_micros.store(
                            (error_seconds * 1_000_000.0).round() as i64,
                            Ordering::Relaxed,
                        );
                        timeline.slew_phase(now, correction_seconds);
                    }
                    Some(PhaseDecision::Initialize) => {
                        health.phase_error_micros.store(0, Ordering::Relaxed);
                    }
                    Some(PhaseDecision::Discontinuity { .. }) | None => {}
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }
    }
    health.running.store(false, Ordering::Relaxed);
    health.disconnected.store(true, Ordering::Relaxed);
}

impl<P: MidiTransport> DecodeSyncBridge<P> {
    pub fn new(
        engine: SyncEngine<P>,
        ref_bpm: f64,
        ltc_fps: Fps,
        anchor_timecode: Timecode,
        latency_ms: f64,
    ) -> Self {
        Self {
            engine,
            ref_bpm,
            ltc_fps,
            anchor_timecode,
            latency_ms,
            last_decoded_frame_count: 0,
            last_timecode: None,
            clock_accumulator: 0.0,
            last_lock_status: LockStatus::Unlocked,
        }
    }

    pub fn into_engine(self) -> SyncEngine<P> {
        self.engine
    }

    fn handle_status_result(&mut self, status: &DecodeStatus) -> Result<(), RuntimeError> {
        let has_new_frame = status.decoded_frame_count > self.last_decoded_frame_count;
        let new_frames = if has_new_frame {
            match (self.last_timecode, status.current_timecode) {
                (Some(previous), Some(current)) => {
                    forward_frame_delta(previous, current, self.ltc_fps) as u64
                }
                _ => 0,
            }
        } else {
            0
        };
        self.last_decoded_frame_count = status.decoded_frame_count;
        if has_new_frame {
            self.last_timecode = status.current_timecode;
        }
        let lock_acquisition =
            self.last_lock_status != LockStatus::Locked && status.lock_status == LockStatus::Locked;

        if status.direction == PlaybackDirection::Reverse {
            self.clock_accumulator = 0.0;
            self.last_lock_status = status.lock_status;
            self.engine.apply_update(SyncUpdate {
                lock_status: status.lock_status,
                direction: status.direction,
                song_position_pointer: 0,
                emit_clock_tick: false,
            })?;
            return Ok(());
        }

        let measured_fps = status.measured_fps.unwrap_or(0.0);
        let smoothed_tempo_bpm = status.smoothed_tempo_bpm.unwrap_or(self.ref_bpm);

        if lock_acquisition {
            self.clock_accumulator = latency_phase(self.latency_ms, smoothed_tempo_bpm);
        }

        self.clock_accumulator +=
            clocks_for_frame_delta(new_frames, measured_fps, smoothed_tempo_bpm);
        let mut clock_ticks = self.clock_accumulator.floor() as u32;
        self.clock_accumulator -= clock_ticks as f64;

        let song_position_pointer = status
            .current_timecode
            .map(|timecode| self.song_position_pointer(timecode, measured_fps))
            .unwrap_or(0);

        self.engine.apply_update(SyncUpdate {
            lock_status: status.lock_status,
            direction: status.direction,
            song_position_pointer,
            emit_clock_tick: clock_ticks > 0,
        })?;

        self.last_lock_status = status.lock_status;

        while clock_ticks > 1 {
            self.engine.apply_update(SyncUpdate {
                lock_status: status.lock_status,
                direction: status.direction,
                song_position_pointer,
                emit_clock_tick: true,
            })?;
            clock_ticks -= 1;
        }

        Ok(())
    }

    fn song_position_pointer(&self, timecode: Timecode, measured_fps: f64) -> u16 {
        song_position_pointer(
            timecode,
            self.anchor_timecode,
            measured_fps,
            self.ref_bpm,
            self.latency_ms,
        )
    }
}

fn song_position_pointer(
    timecode: Timecode,
    anchor_timecode: Timecode,
    measured_fps: f64,
    ref_bpm: f64,
    latency_ms: f64,
) -> u16 {
    let clock_position =
        musical_clock_position(timecode, anchor_timecode, measured_fps, ref_bpm, latency_ms);
    let spp = (clock_position / 6.0).floor();
    spp.clamp(0.0, 0x3FFF as f64) as u16
}

fn musical_clock_position(
    timecode: Timecode,
    anchor_timecode: Timecode,
    ltc_fps: f64,
    ref_bpm: f64,
    latency_ms: f64,
) -> f64 {
    let fps = if ltc_fps > 0.0 { ltc_fps } else { 30.0 };
    let seconds = timecode_seconds(timecode, fps) + latency_ms / 1000.0
        - timecode_seconds(anchor_timecode, fps);
    seconds.max(0.0) * ref_bpm * 24.0 / 60.0
}

fn latency_phase(latency_ms: f64, tempo_bpm: f64) -> f64 {
    if latency_ms == 0.0 || tempo_bpm <= 0.0 {
        return 0.0;
    }

    let latency_clocks = (latency_ms / 1000.0) * (tempo_bpm * 24.0 / 60.0);
    (-latency_clocks).rem_euclid(1.0)
}

fn clocks_for_frame_delta(frame_delta: u64, measured_fps: f64, smoothed_tempo_bpm: f64) -> f64 {
    if frame_delta == 0 || measured_fps <= 0.0 || smoothed_tempo_bpm <= 0.0 {
        return 0.0;
    }

    let elapsed_seconds = frame_delta as f64 / measured_fps;
    elapsed_seconds * (smoothed_tempo_bpm * 24.0 / 60.0)
}

impl<P: MidiTransport> DecodeStatusHandler for DecodeSyncBridge<P> {
    fn handle_status(&mut self, status: &DecodeStatus) {
        let _ = self.handle_status_result(status);
    }
}

fn timecode_seconds(timecode: Timecode, ltc_fps: f64) -> f64 {
    ((timecode.hours as f64 * 60.0 + timecode.minutes as f64) * 60.0 + timecode.seconds as f64)
        + timecode.frames as f64 / ltc_fps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::{MidiOutputPort, MidiSink};
    use std::sync::atomic::AtomicBool;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeConnection {
        messages: Vec<Vec<u8>>,
    }

    impl MidiSink for FakeConnection {
        fn send(&mut self, message: &[u8]) -> Result<(), String> {
            self.messages.push(message.to_vec());
            Ok(())
        }
    }

    struct SchedulerTestTransport {
        dropped: Arc<AtomicBool>,
        fail_transport: bool,
    }

    #[derive(Clone)]
    struct RecordingTransport {
        messages: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl MidiTransport for RecordingTransport {
        fn send_start(&mut self) -> Result<(), RuntimeError> {
            self.messages
                .lock()
                .expect("messages lock")
                .push(vec![0xFA]);
            Ok(())
        }

        fn send_stop(&mut self) -> Result<(), RuntimeError> {
            self.messages
                .lock()
                .expect("messages lock")
                .push(vec![0xFC]);
            Ok(())
        }

        fn send_continue(&mut self) -> Result<(), RuntimeError> {
            self.messages
                .lock()
                .expect("messages lock")
                .push(vec![0xFB]);
            Ok(())
        }

        fn send_clock(&mut self) -> Result<(), RuntimeError> {
            self.messages
                .lock()
                .expect("messages lock")
                .push(vec![0xF8]);
            Ok(())
        }

        fn send_song_position_pointer(&mut self, position: u16) -> Result<(), RuntimeError> {
            self.messages.lock().expect("messages lock").push(vec![
                0xF2,
                (position & 0x7F) as u8,
                ((position >> 7) & 0x7F) as u8,
            ]);
            Ok(())
        }
    }

    impl Drop for SchedulerTestTransport {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    impl MidiTransport for SchedulerTestTransport {
        fn send_start(&mut self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn send_stop(&mut self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn send_continue(&mut self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn send_clock(&mut self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn send_song_position_pointer(&mut self, _position: u16) -> Result<(), RuntimeError> {
            if self.fail_transport {
                Err(RuntimeError::Midi("scheduler test failure".to_string()))
            } else {
                Ok(())
            }
        }
    }

    fn engine() -> SyncEngine<MidiOutputPort<FakeConnection>> {
        SyncEngine::new(
            MidiOutputPort::new(
                "TapeSync MIDI Out".to_string(),
                FakeConnection {
                    messages: Vec::new(),
                },
            ),
            true,
            true,
        )
    }

    #[test]
    fn sends_spp_then_start_on_lock() {
        let mut engine = engine();

        engine
            .apply_update(SyncUpdate {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Forward,
                song_position_pointer: 0x1234,
                emit_clock_tick: false,
            })
            .expect("lock update should send transport");

        let connection = engine.into_midi().into_inner();
        assert_eq!(
            connection.messages,
            vec![vec![0xF2, 0x34, 0x24], vec![0xFB]]
        );
    }

    #[test]
    fn sends_spp_then_start_on_lock_when_position_is_zero() {
        let mut engine = engine();

        engine
            .apply_update(SyncUpdate {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Forward,
                song_position_pointer: 0,
                emit_clock_tick: false,
            })
            .expect("lock update should send transport");

        let connection = engine.into_midi().into_inner();
        assert_eq!(
            connection.messages,
            vec![vec![0xF2, 0x00, 0x00], vec![0xFA]]
        );
    }

    #[test]
    fn sends_stop_on_unlock() {
        let mut engine = engine();
        engine
            .apply_update(SyncUpdate {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Forward,
                song_position_pointer: 0,
                emit_clock_tick: false,
            })
            .expect("lock update should succeed");
        engine
            .apply_update(SyncUpdate {
                lock_status: LockStatus::Unlocked,
                direction: PlaybackDirection::Forward,
                song_position_pointer: 0,
                emit_clock_tick: false,
            })
            .expect("unlock update should send stop");

        let connection = engine.into_midi().into_inner();
        assert_eq!(connection.messages.last(), Some(&vec![0xFC]));
    }

    #[test]
    fn sends_clock_only_when_locked_and_forward() {
        let mut engine = engine();
        engine
            .apply_update(SyncUpdate {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Forward,
                song_position_pointer: 0,
                emit_clock_tick: true,
            })
            .expect("clock update should succeed");
        engine
            .apply_update(SyncUpdate {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Reverse,
                song_position_pointer: 0,
                emit_clock_tick: true,
            })
            .expect("reverse update should be suppressed, not fail");

        let connection = engine.into_midi().into_inner();
        assert_eq!(
            connection.messages,
            vec![vec![0xF2, 0x00, 0x00], vec![0xFA], vec![0xF8]]
        );
    }

    #[test]
    fn clock_timeline_spaces_126_bpm_ticks_between_30_fps_updates() {
        let start = Instant::now();
        let mut timeline = ClockTimeline::default();
        timeline.update(start, true, 126.0, 0.0);
        let mut actual_tick_times = Vec::new();
        let mut next_frame = Duration::ZERO;

        for milliseconds in 0..=10_000 {
            let elapsed = Duration::from_millis(milliseconds);
            let now = start + elapsed;
            if timeline.take_due(now).is_some() {
                actual_tick_times.push(elapsed);
            }
            if elapsed >= next_frame {
                timeline.update(now, true, 126.0, 0.0);
                next_frame += Duration::from_secs_f64(1.0 / 30.0);
            }
        }

        let ideal_period = 60.0 / (126.0 * 24.0);
        assert!((actual_tick_times.len() as isize - 504).abs() <= 1);
        assert!(
            actual_tick_times
                .windows(2)
                .all(|ticks| ticks[0] < ticks[1])
        );

        let maximum_interval_error = actual_tick_times
            .windows(2)
            .map(|ticks| ((ticks[1] - ticks[0]).as_secs_f64() - ideal_period).abs())
            .fold(0.0, f64::max);
        assert!(maximum_interval_error <= 0.001);

        for ticks in actual_tick_times.windows(25) {
            let quarter_note_seconds = (ticks[24] - ticks[0]).as_secs_f64();
            let measured_bpm = 60.0 / quarter_note_seconds;
            assert!((measured_bpm - 126.0).abs() / 126.0 <= 0.01);
        }

        for (index, actual) in actual_tick_times.iter().enumerate() {
            let ideal = ideal_period * (index + 1) as f64;
            let phase_error = (actual.as_secs_f64() - ideal).abs();
            let quarter_note = 60.0 / 126.0;
            assert!(phase_error / quarter_note <= 0.01);
        }
    }

    #[test]
    fn late_scheduler_iteration_drops_missed_ticks_instead_of_bursting() {
        let start = Instant::now();
        let mut timeline = ClockTimeline::default();
        timeline.update(start, true, 120.0, 0.0);
        let late = start + Duration::from_millis(100);

        assert!(timeline.take_due(late).is_some());
        assert!(timeline.take_due(late).is_none());
        assert_eq!(
            timeline.wait_duration(late),
            Some(Duration::from_secs_f64(60.0 / (120.0 * 24.0)))
        );
    }

    #[test]
    fn phase_tracker_has_no_error_at_steady_speed() {
        let start = Instant::now();
        let mut tracker = SourcePhaseTracker::default();
        assert!(matches!(
            tracker.observe(start, 0.0, 120.0),
            PhaseDecision::Initialize
        ));

        let decision = tracker.observe(start + Duration::from_secs(1), 48.0, 120.0);
        assert!(matches!(
            decision,
            PhaseDecision::Slew {
                error_seconds,
                correction_seconds,
            } if error_seconds.abs() < 1e-12 && correction_seconds.abs() < 1e-12
        ));
    }

    #[test]
    fn small_phase_error_converges_over_multiple_observations() {
        let start = Instant::now();
        let mut tracker = SourcePhaseTracker::default();
        let _ = tracker.observe(start, 0.0, 120.0);

        let first_error = match tracker.observe(start + Duration::from_secs(1), 48.24, 120.0) {
            PhaseDecision::Slew { error_seconds, .. } => error_seconds,
            other => panic!("expected slew, got {other:?}"),
        };
        let second_error = match tracker.observe(start + Duration::from_secs(2), 96.24, 120.0) {
            PhaseDecision::Slew { error_seconds, .. } => error_seconds,
            other => panic!("expected slew, got {other:?}"),
        };

        assert!(second_error.abs() < first_error.abs());
        assert!(first_error.abs() < 0.01);
    }

    #[test]
    fn quarter_note_phase_jump_is_a_discontinuity() {
        let start = Instant::now();
        let mut tracker = SourcePhaseTracker::default();
        let _ = tracker.observe(start, 0.0, 120.0);

        let decision = tracker.observe(start + Duration::from_secs(1), 72.0, 120.0);
        assert!(matches!(
            decision,
            PhaseDecision::Discontinuity { error_seconds }
                if (error_seconds - 0.5).abs() < 1e-12
        ));
    }

    #[test]
    fn phase_slew_is_bounded_to_half_a_clock_period() {
        let start = Instant::now();
        let mut timeline = ClockTimeline::default();
        timeline.update(start, true, 120.0, 0.0);
        let before = timeline.next_tick.expect("next tick should exist");

        timeline.slew_phase(start, 1.0);

        let after = timeline.next_tick.expect("next tick should remain");
        let maximum_shift = timeline.period.div_f64(2.0);
        assert!(before.saturating_duration_since(after) <= maximum_shift);
    }

    #[test]
    fn song_position_uses_nominal_ltc_rate() {
        let anchor = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let one_second_later = Timecode {
            seconds: 1,
            ..anchor
        };

        assert_eq!(
            song_position_pointer(one_second_later, anchor, 30.0, 120.0, 0.0),
            8
        );
    }

    fn scheduler_update(lock_status: LockStatus, decoded_frame_count: u64) -> SchedulerUpdate {
        SchedulerUpdate {
            sync: SyncUpdate {
                lock_status,
                direction: PlaybackDirection::Forward,
                song_position_pointer: 0,
                emit_clock_tick: false,
            },
            tempo_bpm: 120.0,
            phase_on_lock: 0.0,
            decoded_frame_count,
            source_clock_position: None,
        }
    }

    #[test]
    fn short_decode_loss_holds_clock_and_reacquires_without_unlock() {
        let start = Instant::now();
        let mut holdover = SchedulerHoldover::default();
        let locked = scheduler_update(LockStatus::Locked, 10);
        assert_eq!(holdover.observe(locked, start, false), Some(locked.sync));

        let unlocked = scheduler_update(LockStatus::Unlocked, 10);
        assert_eq!(
            holdover.observe(unlocked, start + Duration::from_millis(150), true),
            None
        );
        assert!(holdover.is_active());
        assert_eq!(
            holdover.take_expired(start + Duration::from_millis(499)),
            None
        );

        let reacquired = scheduler_update(LockStatus::Locked, 11);
        assert_eq!(
            holdover.observe(reacquired, start + Duration::from_millis(250), true),
            Some(reacquired.sync)
        );
        assert!(!holdover.is_active());
        assert_eq!(
            holdover.take_expired(start + Duration::from_millis(600)),
            None
        );
    }

    #[test]
    fn decode_loss_expires_once_after_500_ms() {
        let start = Instant::now();
        let mut holdover = SchedulerHoldover::default();
        let locked = scheduler_update(LockStatus::Locked, 10);
        let _ = holdover.observe(locked, start, false);
        let unlocked = scheduler_update(LockStatus::Unlocked, 10);
        assert_eq!(
            holdover.observe(unlocked, start + Duration::from_millis(150), true),
            None
        );

        assert_eq!(
            holdover.take_expired(start + Duration::from_millis(500)),
            Some(unlocked.sync)
        );
        assert_eq!(
            holdover.take_expired(start + Duration::from_millis(750)),
            None
        );
    }

    #[test]
    fn scheduler_runtime_joins_and_drops_midi_transport() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (handler, runtime) = spawn_scheduled_decode_sync_handler(
            SchedulerTestTransport {
                dropped: Arc::clone(&dropped),
                fail_transport: false,
            },
            120.0,
            Fps::Fps30,
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            0.0,
            true,
            true,
        )
        .expect("scheduler should start");

        drop(runtime);
        assert!(dropped.load(Ordering::Relaxed));
        drop(handler);
    }

    #[test]
    fn scheduler_surfaces_midi_transport_errors() {
        let (handler, runtime) = spawn_scheduled_decode_sync_handler(
            SchedulerTestTransport {
                dropped: Arc::new(AtomicBool::new(false)),
                fail_transport: true,
            },
            120.0,
            Fps::Fps30,
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            0.0,
            true,
            true,
        )
        .expect("scheduler should start");
        handler
            .lock()
            .expect("handler lock should succeed")
            .handle_status(&DecodeStatus {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Forward,
                current_timecode: Some(Timecode {
                    hours: 1,
                    minutes: 0,
                    seconds: 0,
                    frames: 0,
                }),
                measured_fps: Some(30.0),
                smoothed_tempo_bpm: Some(120.0),
                ..DecodeStatus::default()
            });

        let timeout = Instant::now() + Duration::from_secs(1);
        while runtime.status().last_error.is_none() && Instant::now() < timeout {
            thread::yield_now();
        }

        let status = runtime.status();
        assert!(!status.running);
        assert!(status.disconnected);
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("scheduler test failure"))
        );
    }

    #[test]
    fn large_phase_jump_stops_repositions_and_continues_without_clock_burst() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let anchor = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let (handler, runtime) = spawn_scheduled_decode_sync_handler(
            RecordingTransport {
                messages: Arc::clone(&messages),
            },
            120.0,
            Fps::Fps30,
            anchor,
            0.0,
            true,
            true,
        )
        .expect("scheduler should start");

        let send_status = |timecode, decoded_frame_count| {
            handler
                .lock()
                .expect("handler lock")
                .handle_status(&DecodeStatus {
                    lock_status: LockStatus::Locked,
                    direction: PlaybackDirection::Forward,
                    current_timecode: Some(timecode),
                    decoded_frame_count,
                    measured_fps: Some(30.0),
                    smoothed_tempo_bpm: Some(120.0),
                    ..DecodeStatus::default()
                });
        };
        send_status(anchor, 1);
        let first_timeout = Instant::now() + Duration::from_secs(1);
        while messages.lock().expect("messages lock").len() < 2 && Instant::now() < first_timeout {
            thread::yield_now();
        }

        send_status(
            Timecode {
                seconds: 1,
                ..anchor
            },
            2,
        );
        let second_timeout = Instant::now() + Duration::from_secs(1);
        while runtime.status().discontinuity_count == 0 && Instant::now() < second_timeout {
            thread::yield_now();
        }

        let recorded = messages.lock().expect("messages lock").clone();
        let reposition = recorded
            .windows(3)
            .find(|messages| messages[0] == [0xFC] && messages[1][0] == 0xF2);
        assert_eq!(
            reposition,
            Some(&[vec![0xFC], vec![0xF2, 0x08, 0x00], vec![0xFB]][..])
        );
        assert_eq!(runtime.status().discontinuity_count, 1);
    }

    #[test]
    fn decode_sync_bridge_emits_clock_from_frame_progress() {
        let mut bridge = DecodeSyncBridge::new(
            engine(),
            120.0,
            Fps::Fps30,
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            500.0,
        );

        bridge
            .handle_status_result(&DecodeStatus {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Forward,
                edge_count: 0,
                consecutive_valid_windows: 8,
                consecutive_invalid_windows: 0,
                current_timecode: Some(Timecode {
                    hours: 1,
                    minutes: 0,
                    seconds: 0,
                    frames: 15,
                }),
                decoded_frame_count: 15,
                measured_fps: Some(30.0),
                smoothed_tempo_bpm: Some(120.0),
            })
            .expect("bridge should acquire lock");
        bridge
            .handle_status_result(&DecodeStatus {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Forward,
                edge_count: 0,
                consecutive_valid_windows: 9,
                consecutive_invalid_windows: 0,
                current_timecode: Some(Timecode {
                    hours: 1,
                    minutes: 0,
                    seconds: 0,
                    frames: 16,
                }),
                decoded_frame_count: 16,
                measured_fps: Some(30.0),
                smoothed_tempo_bpm: Some(120.0),
            })
            .expect("bridge should emit clock from frame progress");

        let connection = bridge.into_engine().into_midi().into_inner();
        assert_eq!(connection.messages.first(), Some(&vec![0xF2, 0x08, 0x00]));
        assert!(
            connection
                .messages
                .iter()
                .any(|message| message == &vec![0xF8])
        );
    }

    #[test]
    fn decode_sync_bridge_preserves_clock_time_across_a_missing_frame() {
        let mut bridge = DecodeSyncBridge::new(
            engine(),
            120.0,
            Fps::Fps30,
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            0.0,
        );

        for (decoded_frame_count, frames) in [(1, 10), (2, 12)] {
            bridge
                .handle_status_result(&DecodeStatus {
                    lock_status: LockStatus::Locked,
                    direction: PlaybackDirection::Forward,
                    edge_count: 0,
                    consecutive_valid_windows: 8,
                    consecutive_invalid_windows: 0,
                    current_timecode: Some(Timecode {
                        hours: 1,
                        minutes: 0,
                        seconds: 0,
                        frames,
                    }),
                    decoded_frame_count,
                    measured_fps: Some(30.0),
                    smoothed_tempo_bpm: Some(120.0),
                })
                .expect("bridge update should succeed");
        }

        let connection = bridge.into_engine().into_midi().into_inner();
        let clock_count = connection
            .messages
            .iter()
            .filter(|message| message.as_slice() == [0xF8])
            .count();
        assert_eq!(clock_count, 3);
    }

    #[test]
    fn decode_sync_bridge_suppresses_reverse_chase_updates() {
        let mut bridge = DecodeSyncBridge::new(
            engine(),
            120.0,
            Fps::Fps30,
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            500.0,
        );

        bridge
            .handle_status_result(&DecodeStatus {
                lock_status: LockStatus::Locked,
                direction: PlaybackDirection::Reverse,
                edge_count: 0,
                consecutive_valid_windows: 8,
                consecutive_invalid_windows: 0,
                current_timecode: Some(Timecode {
                    hours: 1,
                    minutes: 0,
                    seconds: 0,
                    frames: 10,
                }),
                decoded_frame_count: 10,
                measured_fps: Some(30.0),
                smoothed_tempo_bpm: Some(120.0),
            })
            .expect("reverse update should be suppressed");

        let connection = bridge.into_engine().into_midi().into_inner();
        assert!(connection.messages.is_empty());
    }
}
