# TapeSync Algorithm Hardening Specification

## 1. Purpose

This specification hardens LTC decoding, tape-speed estimation, and MIDI Clock generation for long recording and playback sessions.

The primary operating assumption is that a correctly maintained tape transport is stable over short intervals. Its speed may creep gradually over minutes or hours, but users are unlikely to make sudden varispeed changes while recording. The algorithm must therefore reject frame-level timing anomalies while following genuine slow drift without accumulating musical phase error.

## 2. Observed Failure

A real tape workflow produced all of the following:

- Ableton Live's displayed tempo fluctuated continuously during recording.
- Reported tempo sometimes differed by approximately 10%.
- Beats recorded to tape played back with non-uniform timing.

The last symptom is consistent with recording Ableton while it follows an irregular external MIDI Clock. Once that timing is recorded to tape, stable tape playback reproduces the irregular performance.

## 3. Findings in the Current Implementation

### 3.1 MIDI Clock is emitted in LTC-frame bursts

`DecodeSyncBridge` accumulates the clocks attributable to a decoded frame, then sends all whole ticks immediately from the decode callback. At 126 BPM, MIDI Clock requires 50.4 ticks per second while 30 fps LTC produces only 30 frame events per second. The resulting pattern contains one- and two-tick bursts separated by approximately 33 ms instead of evenly spaced ticks approximately 19.84 ms apart.

A DAW estimating tempo from recent MIDI tick intervals can interpret this quantization and burst delivery as large tempo changes even when the average tick count is correct.

### 3.2 Missed frames are interpreted as tape slowdown

The decoder estimates instantaneous FPS as:

`sample_rate / samples_between_successful_decodes`

This assumes adjacent successful decodes always represent adjacent LTC frame numbers. If one frame is lost, the sample interval doubles while the numerator remains one frame, so the instantaneous estimate falls from approximately 30 fps to approximately 15 fps.

At 126 BPM with smoothing alpha 0.15, one such sample changes the estimate to:

`126 + 0.15 * (63 - 126) = 116.55 BPM`

This is a 7.5% single-update error. Multiple misses or repeated reacquisition can exceed 10%.

### 3.3 The FPS window does not protect tempo from individual misses

`measured_fps` is averaged over `decode.fps_estimate_window_frames`, but `smoothed_tempo_bpm` is calculated from `last_instantaneous_fps`. Increasing the FPS window therefore stabilizes the status FPS value without equivalently stabilizing the tempo used for MIDI Clock.

### 3.4 Lost decoded frames also lose musical time

Clock progression uses the increase in `decoded_frame_count`, not the increase in decoded LTC frame position. A gap from frame 10 to frame 12 increments the successful-decode counter by one and advances clock by one frame. The missing frame is never recovered, causing cumulative clock and phase error over long playback.

### 3.5 Real-time audio work can cause additional losses

The input callback currently extracts channels and filters samples into newly allocated vectors. It also reaches synchronous MIDI transmission through mutex-protected handlers. Allocation, lock contention, or MIDI backend delay can stall the input callback and produce the same decode gaps that destabilize tempo.

### 3.6 Accepted frames lack continuity qualification

Decoded frames are checked for sync word and legal BCD ranges, but a plausible non-sequential frame can still reach direction, speed, and transport logic. Tape noise, crosstalk, clipping, or a partial false decode can therefore become a timing observation instead of an outlier.

### 3.7 Tests do not observe event timing

Current MIDI tests verify message bytes and ordering only. They do not record emission timestamps, interval jitter, rolling BPM, cumulative phase, callback stalls, or recovery after missing frames. Existing decoder tests establish that frames can be recovered but do not impose a stability bound.

### 3.8 MIDI errors are hidden

`DecodeStatusHandler::handle_status` discards the result of `handle_status_result`. Persistent MIDI failures can therefore appear as tempo or synchronization instability without an actionable runtime error.

### 3.9 Dropout reset uses callback count as a time unit

The timing reset threshold counts audio callbacks that contain no complete decoded frame. Audio callback size is selected by the backend and is unrelated to LTC frame duration. With 128-sample callbacks at 44.1 kHz, eight callbacks span approximately 23 ms, which is shorter than one 30 fps LTC frame. A valid speed estimate can therefore be erased before the next frame arrives. Dropout and lock thresholds must be measured in samples or monotonic elapsed time.

## 4. Design Goals

1. Keep estimated BPM within 1% of the known source tempo during steady and slowly drifting playback.
2. Keep MIDI musical phase within 1% of one quarter note after initial acquisition during uninterrupted playback.
3. Preserve elapsed tape time across missing decoded frames.
4. Emit evenly spaced MIDI Clock ticks independently of the LTC frame callback cadence.
5. Hold the latest trustworthy speed through brief decode losses instead of treating losses as abrupt speed changes.
6. Follow gradual tape-speed creep without requiring support for abrupt intentional varispeed changes.
7. Keep allocation, blocking locks, MIDI I/O, and logging out of the audio callback.
8. Make decoder health, estimator confidence, queue overruns, and scheduler timing observable.

## 5. Definitions and Metrics

For reference tempo `B_ref`, reference LTC rate `F_ref`, and true tape speed ratio `r(t)`:

- Target LTC rate: `F_target(t) = F_ref * r(t)`.
- Target tempo: `B_target(t) = B_ref * r(t)`.
- MIDI Clock rate: `C_target(t) = 24 * B_target(t) / 60` ticks per second.
- Ideal quarter-note duration: `Q(t) = 60 / B_target(t)` seconds.
- Relative BPM error: `abs(B_est - B_target) / B_target`.
- Phase error: elapsed wall-clock difference between an emitted tick and the corresponding ideal tick from the source-speed profile.
- Normalized phase error: `abs(phase_error) / Q(t)`.

Unless a test explicitly covers acquisition or recovery, the measurement interval begins after a five-second warm-up.

### 5.1 Acceptance thresholds

During stable or slowly drifting playback:

- Rolling BPM error over every one-second window: at most 1%.
- 99th-percentile rolling BPM error over 24-tick windows: at most 1%.
- Normalized absolute phase error: at most 1% of one quarter note.
- Mean MIDI tick-rate error over every ten-second window: at most 0.2%.
- 95th-percentile absolute tick-interval error: at most 2 ms.
- Cumulative clock-count error over 60 minutes: at most one tick after accounting for the configured phase offset.
- No two scheduled clock ticks may intentionally share the same deadline.

At 126 BPM, the 1% phase bound is approximately 4.76 ms. Hardware validation may separately report interface and OS scheduling jitter, but deterministic scheduler tests must meet the bound above.

## 6. Hardened Algorithm

### 6.1 Separate real-time stages

Use the following ownership model:

1. The audio callback copies the configured input channel into a preallocated bounded single-producer/single-consumer buffer and returns.
2. A decode worker owns filtering, edge detection, LTC frame recovery, and frame qualification.
3. A timing estimator consumes qualified frame observations containing decoded timecode and absolute audio sample position.
4. A MIDI scheduler owns the MIDI connection and emits timestamped or deadline-driven clock events independently of decode callback frequency.
5. The UI reads snapshots and counters without participating in the timing path.

Queue overflow must increment a visible counter. It must never silently overwrite timing data.

### 6.2 Use an unwrapped LTC frame ordinal

Convert each timecode into an unwrapped frame ordinal. Handle the 24-hour wrap explicitly. For each accepted observation, retain:

- frame ordinal;
- absolute input sample position;
- direction;
- decode confidence or rejection reason.

For two observations, compute both:

- `delta_frames = current_ordinal - previous_ordinal`;
- `delta_samples = current_sample - previous_sample`.

Estimate interval speed as:

`fps = sample_rate * delta_frames / delta_samples`

A forward gap from frame 10 to frame 12 must therefore count as two elapsed frames. A repeated frame, negative jump while forward, implausibly large jump, or invalid sample delta must be rejected as a timing observation.

### 6.3 Estimate slow drift robustly

Use qualified `(sample_position, frame_ordinal)` observations over a time-based rolling horizon, not a fixed count of successful callbacks. The initial target horizon is five seconds, configurable for experiments between two and ten seconds.

Estimate the slope with robust linear regression or a median-of-slopes estimator. The estimator must:

- weight elapsed source time rather than callback count;
- tolerate isolated missing frames and rejected observations;
- reject slope outliers outside the supported physical tape-speed range;
- expose confidence from observation count, span, residuals, and gap density;
- retain the last confident estimate during a short dropout;
- slew the published speed slowly enough that one frame cannot cause a 1% BPM step.

Because sudden speed changes are not a target workflow, stability takes priority over sub-second response. A real linear speed drift of 1% over ten minutes should be followed within the 1% BPM bound. Step-varispeed response is a diagnostic test, not an acceptance-critical behavior.

### 6.4 Clock phase model

Maintain a continuous musical phase driven by the estimated tape-speed curve. A decoded frame updates the phase reference; it does not directly emit MIDI Clock.

The scheduler must:

- calculate successive 24 PPQN deadlines from continuous phase;
- distribute ticks evenly between LTC frames;
- update future intervals with a bounded slew when the speed estimate changes;
- never catch up by sending multiple immediate ticks;
- preserve fractional phase across estimator updates;
- use a monotonic clock for scheduling;
- apply configured latency as a phase offset, not as a tempo change.

Where the MIDI backend supports timestamped delivery, prefer backend timestamps. Otherwise use a dedicated scheduler thread with monotonic deadline waiting and record actual send times for diagnostics.

### 6.5 Dropout and reacquisition policy

A decode loss must not immediately imply a speed change.

- Short dropout: free-run MIDI Clock from the last confident speed estimate.
- Reacquisition: compare decoded LTC position with predicted source phase.
- Small phase error: correct gradually with a bounded phase loop.
- Large discontinuity: stop, reposition with SPP, and restart/continue according to transport policy.
- Never compress missed clock time into a burst.

Initial policy values to validate in simulation:

- speed-estimate holdover: 500 ms;
- large-discontinuity threshold: one quarter note or 500 ms, whichever is smaller;
- gradual phase-correction horizon: at least two seconds;
- unlock/transport-stop decision based on elapsed sample time and confidence, not audio callback count.

These values must be tuned from tests before becoming public configuration.

### 6.6 Lock and continuity

Distinguish these concepts:

- Signal lock: valid LTC waveform and frames are being recovered.
- Timing confidence: enough qualified observations exist for a trustworthy speed slope.
- Scheduler holdover: clock is temporarily free-running after signal loss.
- Transport discontinuity: decoded position is too far from predicted position for gradual correction.

Do not reset a trustworthy tempo estimate merely because a small number of audio buffers contain no complete LTC frame. Buffer sizes vary and are not a stable unit of tape time.

### 6.7 Diagnostics

Expose at least:

- accepted and rejected frame counts;
- missing-frame count inferred from ordinal gaps;
- last frame ordinal and sample position;
- estimated FPS, BPM, and estimator confidence;
- signal-lock and holdover states;
- audio queue overrun count;
- scheduler late-tick count;
- rolling tick interval mean, p95 error, and maximum error;
- cumulative phase error;
- last MIDI error.

Diagnostics must be rate-limited outside real-time threads.

## 7. Comprehensive Automated Test Suite

### 7.1 Deterministic simulation harness

Add a virtual-time harness that connects:

`LTC source -> impairment model -> chunked audio input -> decoder -> estimator -> scheduler -> timestamped MIDI sink`

The harness must not use wall-clock sleeps. It advances a deterministic monotonic clock and records every MIDI message with its scheduled and actual virtual timestamp. Use seeded random generators so failures reproduce exactly.

Run critical scenarios at 44.1 kHz and 48 kHz, 24/25/29.97/30 fps, and representative tempos including 60, 126, and 200 BPM.

### 7.2 Unit tests

1. **Frame ordinal conversion**
   - Adjacent frames, second/minute/hour boundaries, and 24-hour wrap.
   - 29.97 non-drop-frame behavior remains explicit.

2. **Missing-frame-aware interval estimate**
   - Gaps of 1, 2, 5, and 30 frames produce the same FPS when sample elapsed time matches the gap.
   - Repeated and backward frames are rejected in forward mode.

3. **Robust slope estimator**
   - Exact constant speed.
   - Quantized edge positions.
   - Single timing outlier.
   - Clustered outliers below the supported rejection limit.
   - Sparse observations with valid ordinal gaps.
   - Five-second rolling-window expiry.

4. **Slow-drift estimator**
   - Linear drift of +1% and -1% over 10, 30, and 60 minutes.
   - Smooth sinusoidal drift of 0.5% amplitude with periods of 30 seconds and 5 minutes.
   - Assert one-second rolling BPM error and estimator slew bounds.

5. **Clock scheduler**
   - Constant BPM produces strictly increasing, evenly spaced deadlines.
   - Fractional periods do not accumulate count or phase error.
   - Tempo updates preserve phase and never create duplicate deadlines.
   - Latency changes phase only.

6. **Holdover and reacquisition**
   - Dropouts of 1 frame, 2 frames, 100 ms, 250 ms, and 500 ms.
   - No tempo collapse during holdover.
   - Small reacquisition errors converge without clock bursts.
   - Large discontinuities trigger the documented transport policy.

7. **Queue and error behavior**
   - Audio queue overflow is counted and observable.
   - MIDI errors are retained and surfaced.
   - Scheduler lateness is measured.

### 7.3 Decoder impairment matrix

Generate sample-domain LTC and apply deterministic impairments independently and in selected combinations:

- remove every Nth complete frame for N = 10, 30, and 100;
- random frame loss rates of 0.1%, 1%, and 5%;
- burst losses of 2, 5, and 15 frames;
- Gilbert-Elliott clustered-loss sequences;
- white noise at multiple signal-to-noise ratios;
- amplitude from -24 dBFS through -3 dBFS;
- hard clipping;
- DC offset;
- polarity inversion;
- low- and high-frequency rolloff representative of tape paths;
- adjacent-track crosstalk mixed with program audio;
- sample chunk sizes of 32, 64, 128, 256, 512, and irregular alternating sizes.

For deliberate whole-frame removal, elapsed sample time and timecode ordinal must both advance. Tests must prove that losses do not appear as proportional tape slowdown.

### 7.4 End-to-end speed profiles

Required deterministic profiles:

1. **Stable long play**: 60 minutes at 1.000x.
2. **Slow positive creep**: linear 1.000x to 1.010x over 60 minutes.
3. **Slow negative creep**: linear 1.000x to 0.990x over 60 minutes.
4. **Warm-up drift**: exponential approach from 0.995x to 1.000x over 15 minutes.
5. **Low wow/flutter**: 0.1% sinusoidal modulation superimposed on slow creep.
6. **Scattered loss**: stable speed with one missing frame every 30 seconds.
7. **Burst loss**: stable speed with a five-frame dropout every five minutes.
8. **Combined realistic profile**: slow 1% drift, low wow/flutter, noise, crosstalk, and 0.1% random frame loss.
9. **Diagnostic speed step**: instantaneous +/-5% step, reported for characterization but not used as a release gate unless it creates unsafe bursts or phase discontinuities.

For profiles 1 through 8, enforce the Section 5.1 BPM and phase thresholds outside declared acquisition and large-discontinuity intervals.

### 7.5 Timestamp-based MIDI assertions

The fake MIDI sink must record timestamps, not only bytes. For each run calculate:

- adjacent tick intervals;
- rolling BPM over 24 ticks and one second;
- mean, p95, p99, and maximum interval error;
- tick count against integrated ideal phase;
- absolute and normalized phase error over time;
- duplicate or non-monotonic deadlines;
- acquisition and post-dropout recovery time;
- transport/SPP ordering and timestamps.

A test must specifically reproduce the current 126 BPM/30 fps implementation. It should fail by detecting one/two-tick bursts, then pass with the independent scheduler.

### 7.6 Audio-callback contract tests

Instrument a test build to assert that the input callback:

- performs no heap allocation after initialization;
- performs no MIDI I/O;
- performs no blocking mutex acquisition;
- has bounded work proportional only to input sample count;
- reports queue overflow without blocking.

Where allocator instrumentation is platform-dependent, run it in a dedicated CI job and keep structural unit tests for all platforms.

### 7.7 Recorded fixture and hardware tests

Automated fixture tests should include short sanitized captures from the actual tape/interface path when redistribution is acceptable. Generated fixtures remain the reproducible CI baseline.

Before release, perform a hardware test with the AudioBox 1818 VSL and the target tape machine:

1. Stripe at 30 fps and reference 126 BPM.
2. Capture at least 30 minutes of returned LTC audio for offline analysis.
3. Run Decode while recording virtual MIDI timestamps through a loopback monitor.
4. Record metronome or beat transients from Ableton to another tape channel.
5. Compare transient spacing, MIDI phase, decoded ordinal, and raw LTC speed offline.
6. Repeat once from a cold start and once after transport warm-up.

Hardware pass criteria:

- No unexplained 10% BPM excursions.
- Rolling BPM error remains within 1% of independently measured LTC speed.
- Digital MIDI phase meets the 1% bound; any larger end-to-end audio offset is stable, measured, and attributable to configured or external latency.
- Recorded beat spacing follows the independently measured gradual tape-speed curve without frame-rate oscillation.

## 8. Rollout Plan

### Phase 0: Capture and reproduce

- Add timestamped MIDI sink, virtual-time source, and metrics without changing production behavior.
- Encode the 126 BPM/30 fps burst pattern and single-missed-frame tempo drop as failing regression tests.
- Capture a real returned-LTC sample and current runtime status from the reported setup.

Exit gate: failures reproduce burst timing and approximately 7.5-10% estimator excursions deterministically.

### Phase 1: Correct frame accounting and estimation

- Add unwrapped frame ordinals and qualify continuity.
- Replace one-frame interval math with `delta_frames / delta_samples`.
- Drive tempo from the robust time-based estimator.
- Add confidence and rejection diagnostics.

Exit gate: estimator tests and impairment tests pass the 1% BPM bound; MIDI burst tests may still fail.

### Phase 2: Introduce the independent MIDI scheduler

- Move clock generation to continuous phase and monotonic deadlines.
- Add holdover and bounded phase correction.
- Preserve existing Start/Stop/Continue/SPP behavior unless a discontinuity requires explicit repositioning.

Exit gate: timestamp tests meet BPM, interval, count, and 1% phase requirements in virtual time.

### Phase 3: Isolate the audio callback

- Introduce the preallocated audio queue and decode worker.
- Move allocation, status publication, MIDI handling, and logging off the callback.
- Surface queue overruns and MIDI errors.

Exit gate: callback contract tests pass and all existing functional tests remain green.

### Phase 4: Opt-in field validation

- Keep the old clock path available behind a temporary runtime/config feature switch.
- Default development builds used for hardware testing to the hardened path.
- Log summary metrics for both paths where shadow calculation is possible, but emit MIDI from only one path.
- Run the 30-minute hardware protocol and compare captured results.

Exit gate: two hardware runs meet the release thresholds with no transport regression.

### Phase 5: Default-on release

- Make the hardened estimator and scheduler the default.
- Retain the legacy switch for one release as an emergency fallback.
- Document the changed dropout/holdover behavior and diagnostic fields.
- Recommend release builds for real-time use.

Exit gate: CI matrix, long deterministic simulations, and release-candidate hardware tests pass.

### Phase 6: Remove legacy path

- Remove the old frame-triggered clock implementation and temporary switch after one stable release.
- Keep regression fixtures and timestamp metrics permanently.

## 9. Rollback and Compatibility

- Configuration files must remain readable throughout rollout.
- New tuning fields should have conservative defaults and should not be exposed in the UI until hardware validation shows that users need them.
- The temporary legacy switch must select the entire old timing path; hybrid estimator/scheduler combinations are not supported rollback modes.
- A scheduler failure must stop clock cleanly, surface the error, and avoid uncontrolled bursts.
- Existing MIDI port naming and Ableton routing remain unchanged.

## 10. Definition of Done

Hardening is complete when:

1. All acceptance-critical profiles meet the 1% BPM and phase bounds.
2. Lost frames advance elapsed source time correctly and do not create proportional tempo drops.
3. MIDI Clock ticks are independently scheduled and never deliberately burst at LTC frame boundaries.
4. The audio callback performs no allocation, MIDI I/O, or blocking synchronization.
5. Timing and decoder failures are observable instead of discarded.
6. Existing transport, SPP, reverse suppression, sample-rate, and configuration tests pass.
7. At least two long hardware runs satisfy the recorded validation protocol.
8. The legacy timing path has completed its fallback period and is removed.
