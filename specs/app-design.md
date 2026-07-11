# TapeSync Implementation Spec

## 1. Goal

TapeSync is a standalone real-time app that:

1. Generates LTC audio for recording to tape.
2. Decodes LTC from tape playback.
3. Drives a DAW with MIDI transport and MIDI clock.
4. Compensates configured latency and tape speed drift.

Tape is the timing master during playback.

## 2. Scope and Non-Goals

In scope (MVP):

- LTC generator.
- LTC decoder with lock status.
- Tempo recovery from measured LTC rate.
- MIDI Start/Stop/Continue, Song Position Pointer (SPP), and MIDI Clock.
- User-configurable latency offset.

Out of scope (MVP):

- DAW-specific API integration.
- Plugin formats.
- Auto tempo-map extraction from audio.
- Multi-machine tape chase.

## 3. Operating Modes

TapeSync runs in one mode at a time.

- Generate mode: produce LTC audio to selected output device/channel.
- Decode mode: read LTC from selected input device/channel and emit MIDI.

Mode switching behavior:

- Switching modes stops current audio and MIDI streams cleanly.
- No shared running graph between modes in MVP.

## 4. Core Definitions

- LTC frame rate (`ltc_fps`): one of `24`, `25`, `29.97`, `30`.
- Reference tempo (`ref_bpm`): project BPM at reference tape speed.
- Reference LTC speed (`ref_fps`): LTC fps at which `ref_bpm` was established.
- Measured LTC speed (`meas_fps`): decoder-estimated current LTC fps.
- Speed ratio: `ratio = meas_fps / ref_fps`.
- Current tempo: `tempo_bpm = ref_bpm * ratio`.

## 5. Resolved Ambiguities and Defaults

These defaults remove under-specification and should be implemented unless changed by config.

- Timecode format: SMPTE LTC with Manchester (bi-phase mark) encoding.
- `29.97` is non-drop-frame in MVP.
- Audio sample rate default: `44100 Hz`.
- Supported sample rates in MVP: at least `44100 Hz` and `48000 Hz`.
- Audio channel count: output mono LTC signal; input reads one selected mono channel.
- Generator output level default: `-6 dBFS` peak target.
- Decoder lock threshold default: lock after `8` consecutive valid LTC frames.
- Decoder unlock threshold default: unlock after `4` consecutive invalid/missed frames.
- MIDI clock smoothing: first-order low-pass on tempo estimate with configurable alpha (default `0.15`).
- Latency units: milliseconds, signed where noted.
- SPP mapping anchor (MVP): timecode `01:00:00:00` maps to bar 1 beat 1.
- Reverse-direction behavior (MVP): decoder may report reverse, but transport/clock emission ignores reverse playback and does not chase while reverse is detected.
- Device resolution behavior on startup failure: if configured audio/MIDI device name/channel is unavailable, emit a clear related error and prompt the user to try again after fixing device availability/config.

Validation defaults (MVP):

- `smoothing_alpha` allowed range: `0.01..=1.0` (recommended default `0.15`).
- `latency_ms.audio_output` allowed range: `-500.0..=500.0`.
- `latency_ms.tape_path` allowed range: `-500.0..=500.0`.
- `latency_ms.decoder` allowed range: `-500.0..=500.0`.
- `latency_ms.manual` allowed range: `-2000.0..=2000.0`.
- `total_latency_ms` soft warning threshold: warn when `|total_latency_ms| > 1000.0`.

Clock scheduler precision contract (MVP):

- Target mean MIDI clock interval error: within `+-0.5 ms` over any 10-second steady-tempo window.
- Short-term jitter target (95th percentile absolute interval error): `<= 2.0 ms`.
- Long-term drift target: cumulative emitted clock count error `<= 1` clock tick per 60 seconds at steady tempo.

## 6. Configuration Model

Minimal config keys required for coding:

```toml
mode = "generate" # or "decode"

[audio]
sample_rate = 44100
input_device = "..."
input_channel = 0
output_device = "..."
output_channel = 0

# Implementations must not hard-code 44100; timing and DSP must use runtime sample_rate.

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
```

Config typing and validation requirements:

- FPS values are numeric (`ltc_fps`, `ref_fps`) and must be one of: `24.0`, `25.0`, `29.97`, `30.0`.
- `audio.input_channel` and `audio.output_channel` are zero-based integers and must be within the selected device channel range.
- If configured device names/channels are unavailable at startup, fail startup for that mode with a related error and prompt the user to try again.

Derived value:

- `total_latency_ms = audio_output + tape_path + decoder + manual`.

## 7. Functional Requirements

### 7.1 LTC Generator

Inputs:

- `ltc_fps`, `start`, `sample_rate`, output routing.

Behavior:

- Generate continuous LTC frames from `start` onward.
- Encode each frame as LTC bi-phase mark waveform.
- Stream real-time audio buffer to configured output.
- Maintain monotonic frame increment at nominal fps.

Acceptance:

- Generated WAV decodes correctly in at least one external LTC decoder.
- Timecode increments frame-accurately for 10 minutes without discontinuity.

### 7.2 LTC Decoder

Inputs:

- Audio stream from selected input channel.

Pipeline:

1. Pre-filter suitable for LTC band content.
2. Edge/zero-crossing extraction.
3. Bit timing recovery.
4. Manchester decode.
5. LTC frame parse and validation.

Implementation requirement:

- Decoder filter/timing logic must be derived from `sample_rate` (no fixed 48 kHz coefficients).
- Decoder must remain locked across practical tape varispeed pitch range, not only near nominal speed.

Implementation note (MVP):

- Bit-timing recovery uses adaptive half-bit tracking from observed edge intervals, with bounded speed ratios, instead of a fixed nominal timing tolerance.

Outputs:

- `current_timecode`
- `meas_fps`
- `direction` (`forward` or `reverse`)
- `lock_status` (`locked` or `unlocked`)

Acceptance:

- Correctly recovers frame sequence from clean LTC test file.
- Reports lock/unlock transitions according to thresholds.

### 7.3 Tempo Recovery

Given decoded frame timing:

- Estimate `meas_fps` over a short sliding window.
- Compute `ratio = meas_fps / ref_fps`.
- Compute `tempo_bpm = ref_bpm * ratio`.
- Apply smoothing before MIDI clock scheduling.

Implementation note (MVP):

- During startup acquisition (`unlocked`/`locking`), tempo readout uses current measured tempo directly; once `locked`, configured smoothing is applied. Unlock/dropout behavior remains unchanged.

Acceptance:

- If tape speed changes by `+5%`, output clock converges near `ref_bpm * 1.05`.

### 7.4 MIDI Synchronization

Port:

- Create virtual MIDI output named from config.

Transport rules:

- On transition `unlocked -> locked`: send `Start` (or `Continue` if policy selected later; MVP uses `Start`).
- On transition `locked -> unlocked`: send `Stop`.
- Send `SPP` on lock acquisition based on decoded position and fixed MVP anchor mapping.

SPP mapping policy (MVP):

- Fixed anchor pair: LTC `01:00:00:00` corresponds to musical bar 1 beat 1.
- Assume constant tempo from `tempo.ref_bpm` at reference speed (then scaled by measured speed ratio).
- Convert decoded LTC position relative to anchor into MIDI beat position, then into SPP units (16th-note steps; 6 clocks per step).
- If decoded position is before anchor, clamp SPP to `0`.
- On lock acquisition, emit `SPP` first, then `Start`.

Timing rules:

- Send MIDI Clock at 24 PPQN derived from smoothed `tempo_bpm`.
- Optional MTC is disabled by default in MVP.
- During detected reverse direction, suppress Start/SPP/Clock chase updates (ignore reverse for MVP).

Acceptance:

- DAW external sync follows tempo and transport from TapeSync.

### 7.5 Latency Compensation

Apply timeline offset before generating MIDI events:

- `effective_time = decoded_time + total_latency_ms`

Notes:

- Positive latency shifts emitted MIDI later in wall-clock time while referencing future tape position.
- Keep sign convention consistent across UI/config and engine.

Acceptance:

- Configured offset changes transport/clock alignment by expected amount.

## 8. Runtime Architecture (MVP)

Recommended Rust modules:

- `src/audio/` for I/O wrappers.
- `src/ltc/` for encoder/decoder/parser.
- `src/sync_core/` for tempo recovery, smoothing, latency application.
- `src/midi/` for output and scheduling.
- `src/main.rs` for orchestration and mode lifecycle.

Concurrency model:

- Audio callback thread: capture/playback buffers.
- Decode/generate worker: LTC processing.
- MIDI scheduler thread: timestamped MIDI emission.
- Control thread/UI: config changes and status.

Use lock-free queues/channels between real-time and non-real-time paths.

## 9. State Machine

Decoder state:

- `UNLOCKED`
- `LOCKING`
- `LOCKED`

Transitions:

- `UNLOCKED -> LOCKING`: first valid frame received.
- `LOCKING -> LOCKED`: lock threshold reached.
- `LOCKED -> UNLOCKED`: unlock threshold reached.

MIDI side effects:

- Enter `LOCKED`: send `SPP`, then `Start`.
- Exit `LOCKED`: send `Stop`.

## 10. MVP Test Plan

1. Generator validation
- Render LTC to WAV and decode externally.

2. Sample-rate coverage
- Run generator and decoder tests at `44100 Hz` and `48000 Hz`.

3. Decoder validation
- Feed known LTC WAV and verify recovered frame numbers.

4. Tempo follow
- Simulate rate changes (`0.95x`, `1.00x`, `1.05x`) and verify MIDI clock frequency tracks.

5. Transport behavior
- Verify Start/Stop/SPP emitted on lock transitions.

6. Latency offset
- Verify changing `total_latency_ms` shifts output timing predictably.

7. Startup device resolution
- Verify unavailable configured audio/MIDI device or channel yields related startup error and user retry prompt.

8. Reverse-direction handling
- Verify reverse detection does not emit chase transport/clock updates in MVP.

9. Clock precision contract
- Verify scheduler meets interval error, jitter, and drift targets under steady-tempo simulation.

## 11. Open Items (Explicitly Deferred)

- Drop-frame support details for `29.97 DF`.
- Advanced reverse playback policy beyond ignore/suppress behavior.
- UI polish beyond basic status + config editing.

These are intentionally deferred; they do not block MVP coding.

## 12. Testing Requirements

This section converts validation intent into required test artifacts.

Test categories:

- Unit tests (automated): LTC frame encode/decode primitives, timecode increment math, tempo ratio math, latency offset math.
- Integration tests (automated): decode pipeline lock/unlock behavior, LTC-to-MIDI transport transitions, MIDI clock tempo tracking.
- Fixture tests (automated): deterministic LTC WAV fixtures for clean and mildly speed-varied input.
- Hardware tests (manual): tape machine playback capture and DAW chase verification.

Fixture artifact specifics (MVP):

- Fixture directory: `tests/fixtures/ltc/`.
- Required generated fixtures per sample rate (`44100`, `48000`):
	- `clean_forward_10min_30fps.wav`
	- `speed_0p95x_2min_30fps.wav`
	- `speed_1p00x_2min_30fps.wav`
	- `speed_1p05x_2min_30fps.wav`
	- `dropout_bursts_30fps.wav` (short muted/corrupted regions for lock/unlock testing)
- Fixture metadata file: `tests/fixtures/ltc/manifest.toml` containing expected start/end timecode, nominal fps profile, and expected lock regions.

Suggested numeric pass criteria (MVP):

- T-03 frame recovery: `>= 99.9%` correct frame index recovery on clean fixtures.
- T-04 lock/unlock transitions: transitions occur within `+-1` frame of threshold-derived expectation.
- T-05 tempo follow: within `+-1.0%` of target tempo within `2.0 s` after each speed step.
- T-07 clock rate correctness: average emitted clock rate error `<= 0.2%` during steady sections.
- T-08 latency shift: measured MIDI event timing shift within `+-5 ms` of configured `total_latency_ms` in simulation.
- T-09 sample-rate coverage: T-01/T-03/T-05/T-07 must pass at both `44100` and `48000`.

Test implementation guidance:

- Prefer deterministic generated fixtures from the project LTC generator plus manifest-encoded expectations.
- Keep hardware-captured files optional adjunct fixtures; do not make CI depend on hardware recordings.

Requirement-to-test traceability (minimum):

- R-7.1 LTC Generator -> T-01 generator WAV decode, T-02 10-minute continuity.
- R-7.2 LTC Decoder -> T-03 frame recovery, T-04 lock/unlock threshold transitions.
- R-7.3 Tempo Recovery -> T-05 tempo follow at `0.95x`, `1.00x`, `1.05x`.
- R-7.4 MIDI Sync -> T-06 Start/Stop/SPP transition behavior, T-07 clock rate correctness.
- R-7.5 Latency Compensation -> T-08 timing shift equals configured `total_latency_ms`.
- R-5/6 sample-rate requirements -> T-09 run critical generator/decoder paths at `44100 Hz` and `48000 Hz`.
- Startup device resolution behavior -> T-10 unavailable device/channel startup error and retry prompt.
- Reverse-ignore policy -> T-11 reverse detection suppresses chase output in MVP.
- Clock precision contract -> T-12 interval/jitter/drift checks under steady tempo.

CI expectations:

- Unit, integration, and fixture tests must run in CI on each PR.
- Hardware tests are not required in CI; they run as release-candidate manual validation.

## 13. Definition of Done (MVP)

An MVP implementation is complete only if all conditions below are true:

1. All functional requirements in Section 7 are implemented.
2. Tests T-01 through T-12 exist and pass.
3. CI passes for the automated test categories in Section 12.
4. Manual hardware validation has a recorded result (pass or documented issue).
5. No known blocker remains for LTC lock, MIDI transport, or MIDI clock generation.
