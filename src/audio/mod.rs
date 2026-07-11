use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfigRange};

use crate::runtime::RuntimeError;

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
}

pub fn open_input_stream(
    device: Device,
    sample_rate: u32,
    channel: u16,
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
    let stream = build_input_stream(&device, &config, supported.sample_format())?;
    stream
        .play()
        .map_err(|source| RuntimeError::AudioStream(source.to_string()))?;

    Ok(AudioEndpoint {
        device,
        sample_rate,
        channel,
        stream,
    })
}

pub fn open_output_stream(
    device: Device,
    sample_rate: u32,
    channel: u16,
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
    let stream = build_output_stream(&device, &config, supported.sample_format())?;
    stream
        .play()
        .map_err(|source| RuntimeError::AudioStream(source.to_string()))?;

    Ok(AudioEndpoint {
        device,
        sample_rate,
        channel,
        stream,
    })
}

pub fn supports_input(device: &Device, sample_rate: u32, channel: u16) -> Result<(), RuntimeError> {
    let ranges = device
        .supported_input_configs()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?;
    select_supported_config(ranges, sample_rate, channel, "input", &device_name(device)?)?;
    Ok(())
}

pub fn supports_output(device: &Device, sample_rate: u32, channel: u16) -> Result<(), RuntimeError> {
    let ranges = device
        .supported_output_configs()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))?;
    select_supported_config(ranges, sample_rate, channel, "output", &device_name(device)?)?;
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

fn supported_to_stream_config(range: &SupportedStreamConfigRange, sample_rate: u32) -> StreamConfig {
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
) -> Result<Stream, RuntimeError> {
    let err_fn = |error| eprintln!("audio input stream error: {error}");

    match sample_format {
        SampleFormat::F32 => device
            .build_input_stream(config, move |_data: &[f32], _| {}, err_fn, None)
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        SampleFormat::I16 => device
            .build_input_stream(config, move |_data: &[i16], _| {}, err_fn, None)
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        SampleFormat::U16 => device
            .build_input_stream(config, move |_data: &[u16], _| {}, err_fn, None)
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        other => Err(RuntimeError::AudioStream(format!(
            "unsupported input sample format: {other:?}"
        ))),
    }
}

fn build_output_stream(
    device: &Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
) -> Result<Stream, RuntimeError> {
    let err_fn = |error| eprintln!("audio output stream error: {error}");

    match sample_format {
        SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| data.fill(0.0),
                err_fn,
                None,
            )
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        SampleFormat::I16 => device
            .build_output_stream(
                config,
                move |data: &mut [i16], _| data.fill(0),
                err_fn,
                None,
            )
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        SampleFormat::U16 => device
            .build_output_stream(
                config,
                move |data: &mut [u16], _| data.fill(u16::MAX / 2),
                err_fn,
                None,
            )
            .map_err(|source| RuntimeError::AudioStream(source.to_string())),
        other => Err(RuntimeError::AudioStream(format!(
            "unsupported output sample format: {other:?}"
        ))),
    }
}

pub fn device_name(device: &Device) -> Result<String, RuntimeError> {
    device
        .name()
        .map_err(|source| RuntimeError::AudioConfiguration(source.to_string()))
}
