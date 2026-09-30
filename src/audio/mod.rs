use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfigRange};
use std::sync::{Arc, Mutex};

use crate::ltc::{
    DecodeMonitor, DecodeRequest, DecodeStatus, GeneratorRequest, LtcGenerator,
    SharedDecodeStatusHandler,
};
use crate::runtime::RuntimeError;

pub type SharedDecodeStatus = Arc<Mutex<DecodeStatus>>;

type SharedDecodeMonitor = Arc<Mutex<DecodeMonitor>>;

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
    let decode_monitor = Arc::new(Mutex::new(DecodeMonitor::new(decode_request, sample_rate)));
    let stream = build_input_stream(
        &device,
        &config,
        supported.sample_format(),
        channel,
        Arc::clone(&decode_status),
        decode_monitor,
        decode_status_handler,
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
    decode_status: SharedDecodeStatus,
    decode_monitor: SharedDecodeMonitor,
    decode_status_handler: Option<SharedDecodeStatusHandler>,
) -> Result<Stream, RuntimeError> {
    let err_fn = |error| eprintln!("audio input stream error: {error}");
    let channel_count = config.channels as usize;
    let target_channel = channel as usize;

    match sample_format {
        SampleFormat::F32 => {
            let decode_status = Arc::clone(&decode_status);
            let decode_monitor = Arc::clone(&decode_monitor);
            let decode_status_handler = decode_status_handler.clone();
            device
                .build_input_stream(
                    config,
                    move |data: &[f32], _| {
                        process_f32_input(
                            data,
                            channel_count,
                            target_channel,
                            &decode_status,
                            &decode_monitor,
                            decode_status_handler.as_ref(),
                        )
                    },
                    err_fn,
                    None,
                )
                .map_err(|source| RuntimeError::AudioStream(source.to_string()))
        }
        SampleFormat::I16 => {
            let decode_status = Arc::clone(&decode_status);
            let decode_monitor = Arc::clone(&decode_monitor);
            let decode_status_handler = decode_status_handler.clone();
            device
                .build_input_stream(
                    config,
                    move |data: &[i16], _| {
                        process_i16_input(
                            data,
                            channel_count,
                            target_channel,
                            &decode_status,
                            &decode_monitor,
                            decode_status_handler.as_ref(),
                        )
                    },
                    err_fn,
                    None,
                )
                .map_err(|source| RuntimeError::AudioStream(source.to_string()))
        }
        SampleFormat::U16 => {
            let decode_status = Arc::clone(&decode_status);
            let decode_monitor = Arc::clone(&decode_monitor);
            let decode_status_handler = decode_status_handler.clone();
            device
                .build_input_stream(
                    config,
                    move |data: &[u16], _| {
                        process_u16_input(
                            data,
                            channel_count,
                            target_channel,
                            &decode_status,
                            &decode_monitor,
                            decode_status_handler.as_ref(),
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

fn process_f32_input(
    data: &[f32],
    channel_count: usize,
    target_channel: usize,
    decode_status: &SharedDecodeStatus,
    decode_monitor: &SharedDecodeMonitor,
    decode_status_handler: Option<&SharedDecodeStatusHandler>,
) {
    let selected = extract_f32_channel(data, channel_count, target_channel);
    update_decode_status(
        decode_status,
        decode_monitor,
        decode_status_handler,
        &selected,
    );
}

fn process_i16_input(
    data: &[i16],
    channel_count: usize,
    target_channel: usize,
    decode_status: &SharedDecodeStatus,
    decode_monitor: &SharedDecodeMonitor,
    decode_status_handler: Option<&SharedDecodeStatusHandler>,
) {
    let selected = data
        .chunks(channel_count)
        .filter_map(|frame| frame.get(target_channel).copied())
        .map(|sample| sample as f32 / i16::MAX as f32)
        .collect::<Vec<_>>();
    update_decode_status(
        decode_status,
        decode_monitor,
        decode_status_handler,
        &selected,
    );
}

fn process_u16_input(
    data: &[u16],
    channel_count: usize,
    target_channel: usize,
    decode_status: &SharedDecodeStatus,
    decode_monitor: &SharedDecodeMonitor,
    decode_status_handler: Option<&SharedDecodeStatusHandler>,
) {
    let selected = data
        .chunks(channel_count)
        .filter_map(|frame| frame.get(target_channel).copied())
        .map(|sample| (sample as f32 / u16::MAX as f32) * 2.0 - 1.0)
        .collect::<Vec<_>>();
    update_decode_status(
        decode_status,
        decode_monitor,
        decode_status_handler,
        &selected,
    );
}

fn update_decode_status(
    decode_status: &SharedDecodeStatus,
    decode_monitor: &SharedDecodeMonitor,
    decode_status_handler: Option<&SharedDecodeStatusHandler>,
    samples: &[f32],
) {
    let status = if let Ok(mut monitor) = decode_monitor.lock() {
        monitor.process_samples(samples)
    } else {
        DecodeStatus::default()
    };

    if let Ok(mut shared) = decode_status.lock() {
        *shared = status.clone();
    }

    if let Some(handler) = decode_status_handler
        && let Ok(mut handler) = handler.lock()
    {
        handler.handle_status(&status);
    }
}

fn extract_f32_channel(data: &[f32], channel_count: usize, target_channel: usize) -> Vec<f32> {
    data.chunks(channel_count)
        .filter_map(|frame| frame.get(target_channel).copied())
        .collect()
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
    fn extracts_requested_input_channel() {
        let selected = extract_f32_channel(&[0.1, 0.2, 0.3, 0.4], 2, 1);
        assert_eq!(selected, vec![0.2, 0.4]);
    }
}
