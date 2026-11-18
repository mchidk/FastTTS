//! Always-open microphone: downmix, resample to 16 kHz, keep a 250 ms pre-roll.

use anyhow::{anyhow, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, ErrorKind, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
};
use std::{
    collections::VecDeque,
    f64::consts::PI,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

/// Sample rate the speech model expects.
pub const RATE: usize = 16_000;
const PRE_ROLL: usize = RATE / 4;
/// The noise floor follows quiet moments at once but rises only 2 dB per
/// second, so speech cannot drag it up between pauses.
const NOISE_RISE_PER_SECOND: f32 = 0.230_258_5; // ln(10^(2/20))
const NOISE_START: f32 = 0.001;
const NOISE_MIN: f32 = 1e-6;

/// Loudest RMS since the HUD last looked. Every audio callback records into
/// it and the HUD takes the value once per frame, so no peak is missed.
#[derive(Default)]
pub struct Meter(AtomicU32);

impl Meter {
    fn record(&self, rms: f32) {
        // For non-negative floats the bit pattern orders like the value.
        self.0.fetch_max(rms.max(0.0).to_bits(), Ordering::Relaxed);
    }

    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }
}

/// Windowed-sinc resampler that keeps its state across callbacks, so the
/// stream has no seams and frequencies above 8 kHz cannot alias into speech.
struct Resampler {
    step: f64,
    half: usize,
    phases: usize,
    table: Vec<f32>,
    history: Vec<f32>,
    position: f64,
}

impl Resampler {
    fn new(from: usize, to: usize) -> Self {
        let step = from as f64 / to as f64;
        let cutoff = 0.45 * (to as f64 / from as f64).min(1.0); // cycles per input sample
        let half = ((8.0 / cutoff / 2.0).ceil() as usize).max(4);
        let phases = 256;
        let taps = 2 * half;
        let mut table = vec![0f32; (phases + 1) * taps];
        for phase in 0..=phases {
            let frac = phase as f64 / phases as f64;
            let row = &mut table[phase * taps..(phase + 1) * taps];
            let mut sum = 0.0;
            for (tap, weight) in row.iter_mut().enumerate() {
                let x = tap as f64 - (half as f64 - 1.0) - frac;
                let sinc = if x == 0.0 {
                    2.0 * cutoff
                } else {
                    (2.0 * PI * cutoff * x).sin() / (PI * x)
                };
                let window = 0.5 + 0.5 * (PI * x / half as f64).cos();
                *weight = (sinc * window) as f32;
                sum += sinc * window;
            }
            for weight in row.iter_mut() {
                *weight /= sum as f32;
            }
        }
        Self {
            step,
            half,
            phases,
            table,
            history: vec![0.0; half],
            position: half as f64,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.history.extend_from_slice(input);
        let taps = 2 * self.half;
        while (self.position as usize) + self.half < self.history.len() {
            let base = self.position as usize;
            let frac = self.position - base as f64;
            let phase = (frac * self.phases as f64).round() as usize;
            let row = &self.table[phase * taps..(phase + 1) * taps];
            let start = base + 1 - self.half;
            let window = &self.history[start..start + taps];
            out.push(window.iter().zip(row).map(|(x, w)| x * w).sum());
            self.position += self.step;
        }
        let consumed = (self.position as usize).saturating_sub(self.half);
        self.history.drain(..consumed);
        self.position -= consumed as f64;
    }
}

struct Capture {
    resampler: Option<Resampler>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    ring: VecDeque<f32>,
    pending: Vec<f32>,
    captured: usize,
    capturing: bool,
    noise: f32,
}

struct Shared {
    capture: Mutex<Capture>,
    meter: Arc<Meter>,
    lost: AtomicBool,
    max_samples: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Capture> {
        self.capture.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn push<T>(&self, data: &[T], channels: usize)
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let mut guard = self.lock();
        let capture = &mut *guard;
        capture.mono.clear();
        capture
            .mono
            .extend(data.chunks_exact(channels).map(|frame| {
                frame.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() / channels as f32
            }));
        let samples = match capture.resampler.as_mut() {
            Some(resampler) => {
                capture.resampled.clear();
                resampler.process(&capture.mono, &mut capture.resampled);
                &capture.resampled
            }
            None => &capture.mono,
        };
        if samples.is_empty() {
            return;
        }
        let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt();
        self.meter.record(rms);
        let rise = (NOISE_RISE_PER_SECOND * samples.len() as f32 / RATE as f32).exp();
        capture.noise = (capture.noise * rise).min(rms).max(NOISE_MIN);

        if capture.capturing {
            let room = self.max_samples.saturating_sub(capture.captured);
            let keep = samples.len().min(room);
            capture.pending.extend_from_slice(&samples[..keep]);
            capture.captured += keep;
        } else {
            capture.ring.extend(samples.iter());
            let excess = capture.ring.len().saturating_sub(PRE_ROLL);
            capture.ring.drain(..excess);
        }
    }
}

pub struct Recorder {
    shared: Arc<Shared>,
    device_name: String,
    stream: Option<Stream>,
}

impl Recorder {
    /// Open the microphone. Failure is not fatal: the recorder reports
    /// itself lost and the app retries.
    pub fn new(device_name: &str, max_seconds: u64) -> Self {
        let mut recorder = Self {
            shared: Arc::new(Shared {
                capture: Mutex::new(Capture {
                    resampler: None,
                    mono: Vec::new(),
                    resampled: Vec::new(),
                    ring: VecDeque::with_capacity(PRE_ROLL + 4096),
                    pending: Vec::new(),
                    captured: 0,
                    capturing: false,
                    noise: NOISE_START,
                }),
                meter: Arc::new(Meter::default()),
                lost: AtomicBool::new(true),
                max_samples: max_seconds as usize * RATE,
            }),
            device_name: device_name.to_string(),
            stream: None,
        };
        if let Err(err) = recorder.reopen() {
            tracing::warn!("microphone unavailable: {err:#}");
        }
        recorder
    }

    /// (Re)build the input stream, for start-up and after the device vanished.
    pub fn reopen(&mut self) -> Result<()> {
        self.stream = None;
        self.shared.lost.store(true, Ordering::Release);
        let device = select_device(&self.device_name)?;
        let supported = device
            .default_input_config()
            .context("could not read the microphone format")?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let rate = config.sample_rate as usize;
        let channels = config.channels as usize;
        anyhow::ensure!(
            channels > 0 && rate > 0,
            "the microphone reports no audio format"
        );
        {
            let mut capture = self.shared.lock();
            capture.resampler = (rate != RATE).then(|| Resampler::new(rate, RATE));
            capture.ring.clear();
        }
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, &self.shared),
            SampleFormat::I16 => build::<i16>(&device, &config, &self.shared),
            SampleFormat::I32 => build::<i32>(&device, &config, &self.shared),
            SampleFormat::U16 => build::<u16>(&device, &config, &self.shared),
            other => Err(anyhow!("unsupported microphone sample format {other:?}")),
        }?;
        stream.play().context("could not start the microphone")?;
        self.shared.lost.store(false, Ordering::Release);
        self.stream = Some(stream);
        tracing::info!("microphone open: {format:?}, {rate} Hz, {channels} channel(s)");
        Ok(())
    }

    /// True after the device disappeared; the app then calls `reopen`.
    pub fn is_lost(&self) -> bool {
        self.shared.lost.load(Ordering::Acquire)
    }

    /// Current room noise level (RMS).
    pub fn noise_floor(&self) -> f32 {
        self.shared.lock().noise
    }

    pub fn meter(&self) -> Arc<Meter> {
        Arc::clone(&self.shared.meter)
    }

    /// Start a take that begins with the pre-roll.
    pub fn start(&self) {
        let mut capture = self.shared.lock();
        let pre_roll: Vec<f32> = capture.ring.drain(..).collect();
        capture.captured = pre_roll.len();
        capture.pending = pre_roll;
        capture.capturing = true;
    }

    /// Samples captured since the last call.
    pub fn drain(&self) -> Vec<f32> {
        std::mem::take(&mut self.shared.lock().pending)
    }

    /// End the take and return its last samples.
    pub fn stop(&self) -> Vec<f32> {
        let mut capture = self.shared.lock();
        capture.capturing = false;
        std::mem::take(&mut capture.pending)
    }

    pub fn is_full(&self) -> bool {
        let capture = self.shared.lock();
        capture.capturing && capture.captured >= self.shared.max_samples
    }
}

fn build<T>(device: &Device, config: &StreamConfig, shared: &Arc<Shared>) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let data = Arc::clone(shared);
    let errors = Arc::clone(shared);
    device
        .build_input_stream(
            *config,
            move |samples: &[T], _| data.push(samples, channels),
            move |err| match err.kind() {
                ErrorKind::DeviceNotAvailable | ErrorKind::StreamInvalidated => {
                    tracing::warn!("microphone lost: {err}");
                    errors.lost.store(true, Ordering::Release);
                }
                _ => tracing::debug!("microphone: {err}"),
            },
            None,
        )
        .context("could not open the microphone stream")
}

fn select_device(name: &str) -> Result<Device> {
    let host = cpal::default_host();
    if name.is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("no microphone found"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: usize, seconds: f64) -> Vec<f32> {
        (0..(rate as f64 * seconds) as usize)
            .map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn resample_in_pieces(input: &[f32], from: usize, piece: usize) -> Vec<f32> {
        let mut resampler = Resampler::new(from, RATE);
        let mut out = Vec::new();
        for part in input.chunks(piece) {
            resampler.process(part, &mut out);
        }
        out
    }

    #[test]
    fn resampling_keeps_length_and_speech_band() {
        for from in [48_000, 44_100, 32_000] {
            let input = sine(1_000.0, from, 1.0);
            let out = resample_in_pieces(&input, from, 441);
            let expected = RATE as i64;
            assert!(
                (out.len() as i64 - expected).abs() < 64,
                "{from}: {}",
                out.len()
            );
            let body = &out[200..out.len() - 200];
            assert!((rms(body) - 0.3536).abs() < 0.01, "{from}: {}", rms(body));
        }
    }

    #[test]
    fn resampling_blocks_aliasing() {
        // 12 kHz cannot be represented at 16 kHz; without filtering it would
        // fold down to 4 kHz at full strength.
        let input = sine(12_000.0, 48_000, 1.0);
        let out = resample_in_pieces(&input, 48_000, 480);
        assert!(rms(&out[200..out.len() - 200]) < 0.01);
    }

    #[test]
    fn pieces_and_whole_give_the_same_signal() {
        let input = sine(440.0, 48_000, 0.5);
        let whole = resample_in_pieces(&input, 48_000, input.len());
        let pieces = resample_in_pieces(&input, 48_000, 333);
        assert_eq!(whole.len(), pieces.len());
        for (a, b) in whole.iter().zip(&pieces) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn meter_holds_the_peak_until_read() {
        let meter = Meter::default();
        meter.record(0.1);
        meter.record(0.4);
        meter.record(0.2);
        assert_eq!(meter.take(), 0.4);
        assert_eq!(meter.take(), 0.0);
    }

    #[test]
    fn capture_downmixes_and_caps_the_take() {
        let shared = Shared {
            capture: Mutex::new(Capture {
                resampler: None,
                mono: Vec::new(),
                resampled: Vec::new(),
                ring: VecDeque::new(),
                pending: Vec::new(),
                captured: 0,
                capturing: true,
                noise: NOISE_START,
            }),
            meter: Arc::new(Meter::default()),
            lost: AtomicBool::new(false),
            max_samples: 3,
        };
        shared.push(&[1.0f32, 0.0, 0.5, 0.5], 2);
        shared.push(&[0.2f32, 0.2, 1.0, 1.0], 2);
        assert_eq!(shared.lock().pending, vec![0.5, 0.5, 0.2]);
    }
}
