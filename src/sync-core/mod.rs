use crate::config::Fps;
use crate::ltc::{
    DecodeStatus, DecodeStatusHandler, LockStatus, PlaybackDirection, Timecode, forward_frame_delta,
};
use crate::midi::MidiTransport;
use crate::runtime::RuntimeError;

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
        let fps = if measured_fps > 0.0 {
            measured_fps
        } else {
            30.0
        };
        let seconds = timecode_seconds(timecode, fps) + self.latency_ms / 1000.0
            - timecode_seconds(self.anchor_timecode, fps);
        let beats = seconds.max(0.0) * self.ref_bpm / 60.0;
        let spp = (beats * 4.0).floor();
        spp.clamp(0.0, 0x3FFF as f64) as u16
    }
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
