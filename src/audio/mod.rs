use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfigRange};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use crate::ltc::{
    DecodeMonitor, DecodeRequest, DecodeStatus, GeneratorRequest, LtcGenerator,
    SharedDecodeStatusHandler,
};
use crate::runtime::RuntimeError;

pub type SharedDecodeStatus = Arc<Mutex<DecodeStatus>>;

const DECODE_WORK_CHUNK_SAMPLES: usize = 1024;
const DECODE_WORKER_IDLE_WAIT: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub enum AudioRuntime<A = Device, S = Stream> {
    Generate { output: AudioEndpoint<A, S> },
    Decode { input: AudioEndpoint<A, S> },
}

#[derive(Debug)]
pub struct AudioEndpoint<A, S> {
    pub device: A,
    pub sample_rate: u32,
    pub channel: u16,
    pub stream: S,
    pub decode_status: Option<SharedDecodeStatus>,
    pub decode_worker: Option<DecodeWorkerRuntime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeWorkerStatus {
    pub running: bool,
    pub dropped_sample_count: u64,
}

#[derive(Debug, Default)]
struct DecodeWorkerHealth {
    running: AtomicBool,
    dropped_sample_count: AtomicU64,
}

#[derive(Debug)]
pub struct DecodeWorkerRuntime {
    shutdown: Arc<AtomicBool>,
    health: Arc<DecodeWorkerHealth>,
    worker: Option<JoinHandle<()>>,
}

impl DecodeWorkerRuntime {
    pub fn status(&self) -> DecodeWorkerStatus {
        DecodeWorkerStatus {
            running: self.health.running.load(Ordering::Relaxed),
            dropped_sample_count: self.health.dropped_sample_count.load(Ordering::Relaxed),
        }
    }
}

impl Drop for DecodeWorkerRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

pub fn open_input_stream(
    device: Device,
    sample_rate: u32,
    channel: u16,
    decode_request: DecodeRequest,
    decode_status_handler: Option<SharedDecodeStatusHandler>,
) -> Result<AudioEndpoint<Device, Stream>, RuntimeError> {
    let device_name = device_name(&device)?;
    let supported = select_supported_config(
        device
            .supported_input_configs()
            .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?,
        sample_rate,
        channel,
        "input",
        &device_name,
    )?;
    let config = supported_to_stream_config(&supported, sample_rate);
    let decode_status = Arc::new(Mutex::new(DecodeStatus::default()));
    let (producer, worker_thread, decode_worker) = spawn_decode_worker(
        decode_request,
        sample_rate,
        &decode_status,
        decode_status_handler,
    )?;
    let stream = build_input_stream(
        &device,
        &config,
        supported.sample_format(),
        channel,
        producer,
        worker_thread,
        Arc::clone(&decode_worker.health),
    )?;
    stream
        .play()
        .map_err(|source| RuntimeError::AudioStream(source.to_string()))?;

    Ok(AudioEndpoint {
        device,
        sample_rate,
        channel,
        stream,
        decode_status: Some(decode_status),
        decode_worker: Some(decode_worker),
    })
}

pub fn open_output_stream(
    device: Device,
    sample_rate: u32,
    channel: u16,
    generator_request: GeneratorRequest<'_>,
) -> Result<AudioEndpoint<Device, Stream>, RuntimeError> {
    let device_name = device_name(&device)?;
    let supported = select_supported_config(
        device
            .supported_output_configs()
            .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?,
        sample_rate,
        channel,
        "output",
        &device_name,
    )?;
    let config = supported_to_stream_config(&supported, sample_rate);
    let stream = build_output_stream(
        &device,
        &config,
        supported.sample_format(),
        channel,
        generator_request,
    )?;
    stream
        .play()
        .map_err(|source| RuntimeError::AudioStream(source.to_string()))?;

    Ok(AudioEndpoint {
        device,
        sample_rate,
        channel,
        stream,
        decode_status: None,
        decode_worker: None,
    })
}

pub fn supports_input(device: &Device, sample_rate: u32, channel: u16) -> Result<(), RuntimeError> {
    let ranges = device
        .supported_input_configs()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?;
    select_supported_config(ranges, sample_rate, channel, "input", &device_name(device)?)?;
    Ok(())
}

pub fn supports_output(
    device: &Device,
    sample_rate: u32,
    channel: u16,
) -> Result<(), RuntimeError> {
    let ranges = device
        .supported_output_configs()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?;
    select_supported_config(
        ranges,
        sample_rate,
        channel,
        "output",
        &device_name(device)?,
    )?;
    Ok(())
}

fn select_supported_config<I>(
    ranges: I,
    sample_rate: u32,
    channel: u16,
    direction: &str,
    device_name: &str,
) -> Result<SupportedStreamConfigRange, RuntimeError>
where
    I: Iterator<Item = SupportedStreamConfigRange>,
{
    ranges
        .into_iter()
        .find(|range| {
            range.channels() > channel
                && range.min_sample_rate().0 <= sample_rate
                && range.max_sample_rate().0 >= sample_rate
        })
        .ok_or_else(|| RuntimeError::UnsupportedAudioConfiguration {
            direction: direction.to_string(),
            device_name: device_name.to_string(),
            sample_rate,
            channel,
        })
}

fn supported_to_stream_config(
    range: &SupportedStreamConfigRange,
    sample_rate: u32,
) -> StreamConfig {
    StreamConfig {
        channels: range.channels(),
        sample_rate: SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    }
}

fn build_input_stream(
    device: &Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    channel: u16,
    producer: HeapProd<f32>,
    worker_thread: Thread,
    worker_health: Arc<DecodeWorkerHealth>,
) -> Result<Stream, RuntimeError> {
    let err_fn = |error| eprintln!("audio input stream error: {error}");
    let channel_count = config.channels as usize;
    let target_channel = channel as usize;

    match sample_format {
        SampleFormat::F32 => {
            let mut producer = producer;
            device
                .build_input_stream(
                    config,
                    move |data: &[f32], _| {
                        enqueue_f32_input(
                            data,
                            channel_count,
                            target_channel,
                            &mut producer,
                            &worker_thread,
                            &worker_health,
                        )
                    },
                    err_fn,
                    None,
                )
                .map_err(|source| RuntimeError::AudioStream(source.to_string()))
        }
        SampleFormat::I16 => {
            let mut producer = producer;
            device
                .build_input_stream(
                    config,
                    move |data: &[i16], _| {
                        enqueue_i16_input(
                            data,
                            channel_count,
                            target_channel,
                            &mut producer,
                            &worker_thread,
                            &worker_health,
                        )
                    },
                    err_fn,
                    None,
                )
                .map_err(|source| RuntimeError::AudioStream(source.to_string()))
        }
        SampleFormat::U16 => {
            let mut producer = producer;
            device
                .build_input_stream(
                    config,
                    move |data: &[u16], _| {
                        enqueue_u16_input(
                            data,
                            channel_count,
                            target_channel,
                            &mut producer,
                            &worker_thread,
                            &worker_health,
                        )
                    },
                    err_fn,
                    None,
                )
                .map_err(|source| RuntimeError::AudioStream(source.to_string()))
        }
        other => Err(RuntimeError::AudioStream(format!(
            "unsupported input sample format: {other:?}"
        ))),
    }
}

fn build_output_stream(
    device: &Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    channel: u16,
    generator_request: GeneratorRequest<'_>,
) -> Result<Stream, RuntimeError> {
    let err_fn = |error| eprintln!("audio output stream error: {error}");
    let mut generator = LtcGenerator::new(generator_request, config.sample_rate.0)
        .map_err(|source| RuntimeError::Ltc(source.to_string()))?;
    let channel_count = config.channels as usize;
    let target_channel = channel as usize;

    match sample_format {
        SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    render_f32_output(data, channel_count, target_channel, &mut generator)
                },
                err_fn,
                None,
            )
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        SampleFormat::I16 => device
            .build_output_stream(
                config,
                move |data: &mut [i16], _| {
                    render_i16_output(data, channel_count, target_channel, &mut generator)
                },
                err_fn,
                None,
            )
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        SampleFormat::U16 => device
            .build_output_stream(
                config,
                move |data: &mut [u16], _| {
                    render_u16_output(data, channel_count, target_channel, &mut generator)
                },
                err_fn,
                None,
            )
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        other => Err(RuntimeError::AudioStream(format!(
            "unsupported output sample format: {other:?}"
        ))),
    }
}

fn render_f32_output(
    data: &mut [f32],
    channel_count: usize,
    target_channel: usize,
    generator: &mut LtcGenerator,
) {
    data.fill(0.0);
    for frame in data.chunks_mut(channel_count) {
        if let Some(sample) = frame.get_mut(target_channel) {
            *sample = generator.next_sample();
        }
    }
}

fn enqueue_f32_input(
    data: &[f32],
    channel_count: usize,
    target_channel: usize,
    producer: &mut HeapProd<f32>,
    worker_thread: &Thread,
    worker_health: &DecodeWorkerHealth,
) {
    enqueue_input(
        data.chunks(channel_count)
            .filter_map(|frame| frame.get(target_channel).copied()),
        producer,
        worker_thread,
        worker_health,
    );
}

fn enqueue_i16_input(
    data: &[i16],
    channel_count: usize,
    target_channel: usize,
    producer: &mut HeapProd<f32>,
    worker_thread: &Thread,
    worker_health: &DecodeWorkerHealth,
) {
    enqueue_input(
        data.chunks(channel_count)
            .filter_map(|frame| frame.get(target_channel).copied())
            .map(|sample| sample as f32 / i16::MAX as f32),
        producer,
        worker_thread,
        worker_health,
    );
}

fn enqueue_u16_input(
    data: &[u16],
    channel_count: usize,
    target_channel: usize,
    producer: &mut HeapProd<f32>,
    worker_thread: &Thread,
    worker_health: &DecodeWorkerHealth,
) {
    enqueue_input(
        data.chunks(channel_count)
            .filter_map(|frame| frame.get(target_channel).copied())
            .map(|sample| (sample as f32 / u16::MAX as f32) * 2.0 - 1.0),
        producer,
        worker_thread,
        worker_health,
    );
}

fn enqueue_input(
    samples: impl Iterator<Item = f32>,
    producer: &mut HeapProd<f32>,
    worker_thread: &Thread,
    worker_health: &DecodeWorkerHealth,
) {
    let mut dropped = 0u64;
    for sample in samples {
        if producer.try_push(sample).is_err() {
            dropped += 1;
        }
    }
    if dropped > 0 {
        worker_health
            .dropped_sample_count
            .fetch_add(dropped, Ordering::Relaxed);
    }
    worker_thread.unpark();
}

fn spawn_decode_worker(
    decode_request: DecodeRequest,
    sample_rate: u32,
    decode_status: &SharedDecodeStatus,
    decode_status_handler: Option<SharedDecodeStatusHandler>,
) -> Result<(HeapProd<f32>, Thread, DecodeWorkerRuntime), RuntimeError> {
    let ring = HeapRb::<f32>::new((sample_rate as usize / 2).max(DECODE_WORK_CHUNK_SAMPLES));
    let (producer, consumer) = ring.split();
    let shutdown = Arc::new(AtomicBool::new(false));
    let health = Arc::new(DecodeWorkerHealth::default());
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_health = Arc::clone(&health);
    let decode_status = Arc::clone(decode_status);
    let worker = thread::Builder::new()
        .name("tape-sync-ltc-decode".to_string())
        .spawn(move || {
            run_decode_worker(
                consumer,
                DecodeMonitor::new(decode_request, sample_rate),
                decode_status,
                decode_status_handler,
                worker_shutdown,
                worker_health,
            )
        })
        .map_err(|source| {
            RuntimeError::AudioStream(format!("failed to start decode worker: {source}"))
        })?;
    health.running.store(true, Ordering::Relaxed);
    let worker_thread = worker.thread().clone();
    Ok((
        producer,
        worker_thread,
        DecodeWorkerRuntime {
            shutdown,
            health,
            worker: Some(worker),
        },
    ))
}

fn run_decode_worker(
    mut consumer: HeapCons<f32>,
    mut decode_monitor: DecodeMonitor,
    decode_status: SharedDecodeStatus,
    decode_status_handler: Option<SharedDecodeStatusHandler>,
    shutdown: Arc<AtomicBool>,
    health: Arc<DecodeWorkerHealth>,
) {
    let mut samples = [0.0; DECODE_WORK_CHUNK_SAMPLES];
    while !shutdown.load(Ordering::Relaxed) {
        let sample_count = consumer.pop_slice(&mut samples);
        if sample_count == 0 {
            thread::park_timeout(DECODE_WORKER_IDLE_WAIT);
            continue;
        }

        let status = decode_monitor.process_samples(&samples[..sample_count]);
        if let Ok(mut shared) = decode_status.lock() {
            *shared = status.clone();
        }
        if let Some(handler) = &decode_status_handler
            && let Ok(mut handler) = handler.lock()
        {
            handler.handle_status(&status);
        }
    }
    health.running.store(false, Ordering::Relaxed);
}

fn render_i16_output(
    data: &mut [i16],
    channel_count: usize,
    target_channel: usize,
    generator: &mut LtcGenerator,
) {
    data.fill(0);
    for frame in data.chunks_mut(channel_count) {
        if let Some(sample) = frame.get_mut(target_channel) {
            *sample = f32_to_i16(generator.next_sample());
        }
    }
}

fn render_u16_output(
    data: &mut [u16],
    channel_count: usize,
    target_channel: usize,
    generator: &mut LtcGenerator,
) {
    data.fill(u16::MAX / 2);
    for frame in data.chunks_mut(channel_count) {
        if let Some(sample) = frame.get_mut(target_channel) {
            *sample = f32_to_u16(generator.next_sample());
        }
    }
}

fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

fn f32_to_u16(sample: f32) -> u16 {
    (((sample.clamp(-1.0, 1.0) + 1.0) * 0.5) * u16::MAX as f32).round() as u16
}

pub fn device_name(device: &Device) -> Result<String, RuntimeError> {
    device
        .name()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Fps;
    use crate::test_alloc::count_allocations;

    fn generator() -> LtcGenerator {
        LtcGenerator::new(
            GeneratorRequest {
                start: "01:00:00:00",
                fps: Fps::Fps30,
            },
            44_100,
        )
        .expect("generator should initialize")
    }

    #[test]
    fn renders_only_target_channel() {
        let mut buffer = [0.0_f32; 8];
        let mut generator = generator();
        render_f32_output(&mut buffer, 2, 1, &mut generator);

        assert!(buffer.iter().step_by(2).all(|sample| *sample == 0.0));
        assert!(
            buffer
                .iter()
                .skip(1)
                .step_by(2)
                .any(|sample| *sample != 0.0)
        );
    }

    #[test]
    fn converts_generator_output_to_unsigned_pcm() {
        let mut buffer = [0_u16; 4];
        let mut generator = generator();
        render_u16_output(&mut buffer, 1, 0, &mut generator);

        assert!(buffer.iter().any(|sample| *sample != u16::MAX / 2));
    }

    #[test]
    fn callback_enqueues_target_channel_and_counts_overflow() {
        let ring = HeapRb::<f32>::new(2);
        let (mut producer, mut consumer) = ring.split();
        let health = DecodeWorkerHealth::default();

        enqueue_f32_input(
            &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6],
            2,
            1,
            &mut producer,
            &thread::current(),
            &health,
        );

        let mut selected = [0.0; 2];
        assert_eq!(consumer.pop_slice(&mut selected), 2);
        assert_eq!(selected, [0.2, 0.4]);
        assert_eq!(health.dropped_sample_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn input_callbacks_allocate_no_memory() {
        let f32_ring = HeapRb::<f32>::new(32);
        let (mut f32_producer, _f32_consumer) = f32_ring.split();
        let i16_ring = HeapRb::<f32>::new(32);
        let (mut i16_producer, _i16_consumer) = i16_ring.split();
        let u16_ring = HeapRb::<f32>::new(32);
        let (mut u16_producer, _u16_consumer) = u16_ring.split();
        let worker_thread = thread::current();
        let health = DecodeWorkerHealth::default();

        let allocation_count = count_allocations(|| {
            enqueue_f32_input(
                &[0.1, 0.2, 0.3, 0.4],
                2,
                1,
                &mut f32_producer,
                &worker_thread,
                &health,
            );
            enqueue_i16_input(
                &[100, 200, 300, 400],
                2,
                1,
                &mut i16_producer,
                &worker_thread,
                &health,
            );
            enqueue_u16_input(
                &[100, 200, 300, 400],
                2,
                1,
                &mut u16_producer,
                &worker_thread,
                &health,
            );
        });

        assert_eq!(allocation_count, 0);
    }

    #[test]
    fn sustained_callback_load_keeps_up_without_drops() {
        let ring = HeapRb::<f32>::new(1024);
        let (mut producer, mut consumer) = ring.split();
        let worker_thread = thread::current();
        let health = DecodeWorkerHealth::default();
        let interleaved = [0.25_f32; 512];
        let mut drained = [0.0_f32; 256];
        let mut total_samples = 0usize;

        for _ in 0..10_000 {
            enqueue_f32_input(&interleaved, 2, 1, &mut producer, &worker_thread, &health);
            total_samples += consumer.pop_slice(&mut drained);
        }

        assert_eq!(total_samples, 2_560_000);
        assert_eq!(health.dropped_sample_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn decode_worker_processes_samples_and_joins_cleanly() {
        let decode_status = Arc::new(Mutex::new(DecodeStatus::default()));
        let (mut producer, worker_thread, runtime) = spawn_decode_worker(
            DecodeRequest {
                fps: Fps::Fps30,
                ref_fps: Fps::Fps30,
                ref_bpm: 120.0,
                smoothing_alpha: 0.15,
                fps_estimate_window_size: 12,
                dropout_reset_windows: 8,
            },
            44_100,
            &decode_status,
            None,
        )
        .expect("decode worker should start");
        let health = Arc::clone(&runtime.health);
        let mut generator = generator();
        let samples = (0..44_100)
            .map(|_| generator.next_sample())
            .collect::<Vec<_>>();
        let pushed = producer.push_slice(&samples);
        worker_thread.unpark();

        let timeout = std::time::Instant::now() + Duration::from_secs(1);
        while decode_status
            .lock()
            .expect("decode status lock")
            .decoded_frame_count
            == 0
            && std::time::Instant::now() < timeout
        {
            thread::yield_now();
        }

        assert!(pushed > 0);
        assert!(
            decode_status
                .lock()
                .expect("decode status lock")
                .decoded_frame_count
                > 0
        );
        drop(runtime);
        assert!(!health.running.load(Ordering::Relaxed));
    }
}
