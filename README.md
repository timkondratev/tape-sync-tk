# Tape Sync TK

Tape Sync TK is a real-time tape-to-DAW synchronization tool.

It can run in two modes:

- Generate mode: emits SMPTE LTC audio so you can stripe tape with stable timecode.
- Decode mode: reads LTC from tape playback and drives your DAW by MIDI transport and MIDI clock.

The core goal is simple: during playback, tape is the timing master.

## What This App Is For

Tape Sync TK helps hybrid analog and digital setups where you want tape transport movement to control DAW timing.

Typical use cases:

- Stripe and chase workflow:
  - Print LTC to one tape track during setup.
  - Later play back tape and have DAW tempo and transport follow tape speed.
- Varispeed-aware sync:
  - Use tape speed changes creatively.
  - DAW clock follows decoded LTC rate so BPM scales with tape speed.
- Archival transfer with tempo follow:
  - Replay old striped tape sessions.
  - Keep DAW timeline and external MIDI devices aligned during transfer.
- Hardware-first sessions:
  - Let tape be the source of truth.
  - Keep software instruments and clocked hardware in sync via MIDI clock.

## Current Feature Set (MVP)

- LTC generator (bi-phase mark waveform) from configurable start timecode.
- LTC decoder with lock and direction state.
- Tempo recovery from measured LTC frame rate.
- MIDI Start, Stop, Continue, Song Position Pointer, and Clock emission.
- User-configurable latency compensation.
- Startup preflight checks for device and channel availability.

## Platform Notes

- This project is written in Rust.
- Virtual MIDI output is implemented for Unix targets. On macOS this works with CoreMIDI.
- Audio I/O is handled through CPAL and depends on host device support.

## Prerequisites

1. Install Rust toolchain (stable) with Cargo.
2. Ensure your system has usable audio input/output devices.
3. Ensure your DAW can receive MIDI clock and transport from a virtual MIDI port.

## Install

### Build from source

1. Clone this repository.
2. In the repository root, build release binary:

```bash
cargo build --release
```

3. Binary path:

- target/release/tape-sync-tk

### Optional: install into Cargo bin location

```bash
cargo install --path .
```

After this, you can run tape-sync-tk from your shell if Cargo bin is on PATH.

## Quick Start

### 1. Create your config

Copy the sample config and edit device names/channels:

```bash
cp tape-sync.example.toml tape-sync.toml
```

### 2. Set audio and mode

In tape-sync.toml:

- Set mode to generate or decode.
- Set sample_rate to 44100 or 48000.
- Set input_device/output_device to exact system device names.
- Set input_channel/output_channel as zero-based indices.

### 3. Run

Using default config path:

```bash
cargo run
```

Using custom config path:

```bash
cargo run -- --config path/to/your-config.toml
```

Stop with Ctrl-C.

## Main tested Use Case: Tascam PortaStudio + Ableton Live

This is the primary real-world workflow used to validate Tape Sync TK end-to-end.

### Setup goal

Use a Tascam PortaStudio as the transport master and have Ableton Live chase tape via LTC decode and MIDI sync.

### Recommended routing

1. Reserve one PortaStudio track for LTC only (commonly track 4 or track 8).
2. Connect your audio interface output to the PortaStudio input used for striping LTC.
3. Connect the PortaStudio LTC playback output to your audio interface input channel configured in Tape Sync TK.
4. In Ableton Live, select the Tape Sync TK virtual MIDI output port and enable external sync.

### Workflow

1. Run Tape Sync TK in generate mode and record LTC to the dedicated PortaStudio track.
2. Rewind tape and switch Tape Sync TK to decode mode.
3. Start playback on the PortaStudio.
4. Tape Sync TK decodes LTC and emits MIDI SPP/Start/Clock so Ableton Live follows tape position and speed.

### What to verify during testing

1. Lock state transitions to locked shortly after playback begins.
2. Ableton Live transport starts from decoded tape position after lock.
3. Clock remains stable during steady tape speed.
4. Varispeed changes on the PortaStudio produce matching tempo changes in Ableton Live.
5. Latency adjustments in [latency_ms] produce expected timing shifts.

## How To Use

### Generate mode (stripe tape)

Purpose: output LTC audio to a selected audio output/channel so you can record timecode onto tape.

1. Set mode = "generate".
2. Route output channel to the tape track you use for timecode.
3. Set timecode.start and timecode.ltc_fps for your session.
4. Run the app.
5. Record the generated LTC onto tape.

Tips:

- Keep LTC on a dedicated track.
- Avoid heavy processing on LTC path (EQ, compression, noise reduction) when possible.

### Decode mode (chase tape)

Purpose: read LTC from tape playback and emit MIDI sync for DAW/hardware.

1. Set mode = "decode".
2. Route LTC playback from tape into configured input device/channel.
3. In your DAW, select the configured virtual MIDI port name.
4. Enable external sync/clock receive in DAW.
5. Start Tape Sync TK, then start tape playback.

In decode mode, the app shows a live status line with lock state, direction, decoded frames, measured fps, and BPM estimate.

## Configuration Reference

The app reads TOML config. Default path is tape-sync.toml.

Example shape:

```toml
mode = "generate"

[audio]
sample_rate = 44100
input_device = "Replace with an available input device"
input_channel = 0
output_device = "Replace with an available output device"
output_channel = 0

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
```

### Key fields

- mode: generate or decode.
- audio.sample_rate: currently 44100 or 48000.
- timecode.start: HH:MM:SS:FF timecode start.
- timecode.ltc_fps and tempo.ref_fps: allowed values 24.0, 25.0, 29.97, 30.0.
- tempo.ref_bpm: reference BPM at reference speed.
- tempo.smoothing_alpha: 0.01 to 1.0.
- decode.fps_estimate_window_frames: 1 to 120.
- decode.dropout_reset_windows: 1 to 120.
- latency_ms values:
  - audio_output: -500.0 to 500.0
  - tape_path: -500.0 to 500.0
  - decoder: -500.0 to 500.0
  - manual: -2000.0 to 2000.0

The app computes:

- total_latency_ms = audio_output + tape_path + decoder + manual

If absolute total latency exceeds 1000 ms, startup prints a warning.

## Expected Runtime Behavior

- App performs startup preflight checks before opening streams.
- If configured device or channel is unavailable, startup fails with a clear message and available-device context.
- In decode mode:
  - lock acquisition triggers MIDI Song Position Pointer then Start.
  - lock loss triggers Stop.
  - reverse playback detection suppresses chase updates in MVP.

## Troubleshooting

### App says configured device was not found

- Verify exact input_device/output_device name in config.
- Recheck device availability in your operating system audio settings.
- Ensure the device is connected before launch.

### App says channel is unavailable

- Confirm selected channel index is zero-based.
- Reduce channel index to a valid value for the chosen device.

### No DAW sync in decode mode

- Confirm DAW is listening to the same virtual MIDI port name as midi.port_name.
- Confirm send_clock and send_transport are true.
- Confirm LTC signal level and routing into configured input channel.

### Unstable lock

- Check tape signal quality and head alignment.
- Reduce processing/noise reduction on LTC path.
- Tune decode.fps_estimate_window_frames and dropout_reset_windows for your material.

### Timing feels offset

- Adjust latency_ms fields to align DAW events with tape playback.
- Start with manual adjustment after setting known hardware path delays.

## Development

Run tests:

```bash
cargo test
```

Run with default config:

```bash
cargo run
```

## Roadmap Notes

This is an MVP-focused engine. Non-goals currently include DAW-specific APIs, plugin formats, auto tempo-map extraction from arbitrary audio, and multi-machine tape chase.

## License

This project is licensed under the MIT License.

See the LICENSE file for full text.
