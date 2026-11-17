//! Persistent microphone capture with a short pre-roll.

use anyhow::{anyhow, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, Stream, StreamConfig,
};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};

const SAMPLE_RATE: u32 = 16_000;

#[derive(Default)]
struct Inner {
    ring: VecDeque<f32>,
    take: Vec<f32>,
    capturing: bool,
}

/// Shared state owned by Arc so cpal callbacks can be Send + 'static.
struct RecorderState {
    inner: Mutex<Inner>,
    active: AtomicBool,
    level: AtomicU32,
    device_rate: AtomicU32,
    max_samples: usize,
}

impl RecorderState {
    fn push_f32(&self, data: &[f32]) {
        let rms = (data.iter().map(|x| x * x).sum::<f32>() / data.len().max(1) as f32).sqrt();
        self.level.store(rms.to_bits(), Ordering::Relaxed);
        let pre_roll = (0.25 * self.device_rate.load(Ordering::Relaxed) as f64) as usize;
        let mut inner = self.inner.lock();
        for sample in data.iter().copied() {
            Self::push_sample(&mut inner, sample, pre_roll);
        }
    }

    fn push_i16(&self, data: &[i16]) {
        let mut rms: f32 = 0.0;
        let pre_roll = (0.25 * self.device_rate.load(Ordering::Relaxed) as f64) as usize;
        let mut inner = self.inner.lock();
        for value in data.iter() {
            let sample = *value as f32 / 32768.0;
            rms += sample * sample;
            Self::push_sample(&mut inner, sample, pre_roll);
        }
        if !data.is_empty() {
            self.level.store((rms / data.len() as f32).sqrt().to_bits(), Ordering::Relaxed);
        }
    }

    fn push_sample(inner: &mut Inner, sample: f32, pre_roll: usize) {
        if pre_roll > 0 {
            inner.ring.push_back(sample);
            while inner.ring.len() > pre_roll {
                inner.ring.pop_front();
            }
        }
        if inner.capturing {
            inner.take.push(sample);
        }
    }
}

pub struct Recorder {
    state: Arc<RecorderState>,
    stream: Mutex<Option<Stream>>,
}

impl Recorder {
    pub fn new(max_seconds: u64) -> Self {
        Self {
            state: Arc::new(RecorderState {
                inner: Mutex::new(Inner::default()),
                active: AtomicBool::new(false),
                level: AtomicU32::new(0),
                device_rate: AtomicU32::new(SAMPLE_RATE),
                max_samples: (max_seconds as usize) * SAMPLE_RATE as usize,
            }),
            stream: Mutex::new(None),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state.active.load(Ordering::Acquire)
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.state.level.load(Ordering::Relaxed))
    }

    pub fn open(&self, device_name: &str) -> Result<()> {
        let host = cpal::default_host();
        let device = select_device(&host, device_name)?;
        let (config, format) = select_config(&device)?;
        let state = Arc::clone(&self.state);

        let rate = config.sample_rate;
        self.state.device_rate.store(rate, Ordering::Relaxed);
        let stream = match format {
            SampleFormat::F32 => device
                .build_input_stream(
                    config.clone(),
                    move |data: &[f32], _| state.push_f32(data),
                    |err| tracing::warn!("microphone callback error: {err}"),
                    None,
                )
                .context("could not build the input stream")?,
            SampleFormat::I16 => device
                .build_input_stream(
                    config.clone(),
                    move |data: &[i16], _| state.push_i16(data),
                    |err| tracing::warn!("microphone callback error: {err}"),
                    None,
                )
                .context("could not build the input stream")?,
            other => return Err(anyhow!("unsupported microphone sample format: {other:?}")),
        };
        stream.play().context("could not start the input stream")?;
        *self.stream.lock() = Some(stream);
        tracing::info!("microphone stream open at {} Hz", config.sample_rate);
        Ok(())
    }

    pub fn close(&self) {
        if let Some(stream) = self.stream.lock().take() {
            let _ = stream.pause();
        }
    }

    pub fn start_take(&self) {
        let mut inner = self.state.inner.lock();
        inner.take = inner.ring.iter().copied().collect();
        inner.ring.clear();
        inner.capturing = true;
        self.state.active.store(true, Ordering::Release);
    }

    /// Return the samples captured since `cursor` (in device-rate samples),
    /// resampled to 16 kHz, and advance the cursor. Used for streaming chunks.
    pub fn new_chunk(&self, cursor: &mut usize) -> Vec<f32> {
        let inner = self.state.inner.lock();
        if *cursor > inner.take.len() {
            *cursor = inner.take.len();
        }
        let pending = &inner.take[*cursor..];
        *cursor = inner.take.len();
        self.convert(pending)
    }

    /// Stop the take and return only the tail since `cursor`, resampled.
    pub fn stop_take_from(&self, from: usize) -> Vec<f32> {
        let mut inner = self.state.inner.lock();
        inner.capturing = false;
        self.state.active.store(false, Ordering::Release);
        let mut samples = std::mem::take(&mut inner.take);
        let start = from.min(samples.len());
        let tail = samples.split_off(start);
        self.convert(&tail)
    }

    fn convert(&self, samples: &[f32]) -> Vec<f32> {
        let rate = self.state.device_rate.load(Ordering::Relaxed);
        let converted = if rate != SAMPLE_RATE && samples.len() >= 2 {
            resample(samples, rate, SAMPLE_RATE)
        } else {
            samples.to_vec()
        };
        converted.into_iter().take(self.state.max_samples).collect()
    }

    pub fn cancel(&self) {
        let mut inner = self.state.inner.lock();
        inner.capturing = false;
        inner.take.clear();
        self.state.active.store(false, Ordering::Release);
    }
}

fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.len() < 2 {
        return samples.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let count = (samples.len() as f64 / ratio) as usize;
    (0..count)
        .map(|index| {
            let position = index as f64 * ratio;
            let base = position as usize;
            let fraction = (position - base as f64) as f32;
            let a = samples[base];
            let b = samples.get(base + 1).copied().unwrap_or(a);
            a + (b - a) * fraction
        })
        .collect()
}

fn select_device(host: &cpal::Host, name: &str) -> Result<Device> {    if name.is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default microphone found"));
    }
    host.input_devices()?
        .find(|device| {
            device
                .description()
                .map(|desc| desc.name() == name)
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("microphone '{name}' was not found"))
}

fn select_config(device: &Device) -> Result<(StreamConfig, SampleFormat)> {
    let mut chosen: Option<cpal::SupportedStreamConfig> = None;
    for range in device.supported_input_configs()? {
        if range.min_sample_rate() <= SAMPLE_RATE && range.max_sample_rate() >= SAMPLE_RATE {
            if range.sample_format() == SampleFormat::F32 {
                chosen = Some(range.with_sample_rate(SAMPLE_RATE));
                break;
            }
            if range.sample_format() == SampleFormat::I16 && chosen.is_none() {
                chosen = Some(range.with_sample_rate(SAMPLE_RATE));
            }
        }
    }
    let config = match chosen {
        Some(config) => config,
        None => device
            .default_input_config()
            .context("could not read the microphone configuration")?,
    };
    let format = config.sample_format();
    tracing::info!("microphone format {format:?} at {} Hz", config.sample_rate());
    Ok((config.into(), format))
}

#[cfg(test)]
mod tests {
    use super::resample;

    #[test]
    fn resamples_48k_to_16k() {
        let source: Vec<f32> = (0..4800).map(|i| i as f32 / 4800.0).collect();
        let output = resample(&source, 48_000, 16_000);
        assert_eq!(output.len(), 1600);
        assert!((output[0] - 0.0).abs() < 0.001);
        assert!((output[800] - 0.5).abs() < 0.01);
        assert!((output[1599] - source[4797]).abs() < 0.01);
    }
}
