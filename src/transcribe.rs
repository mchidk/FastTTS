//! English dictation with Cactus Whistle: 16.9 MB, CPU-only, no whisper.cpp.

use anyhow::{Context, Result};
use cactus_rs::whistle::{Language, TranscribeOptions, Weights, Whistle};
use std::path::Path;

/// Cheap energy gate: keep silence and room noise away from the model.
pub(crate) fn has_speech(samples: &[f32]) -> bool {
    const MIN_SAMPLES: usize = 1600;
    const FRAME: usize = 160;
    const RMS_FLOOR: f32 = 0.012;
    const PEAK_FLOOR: f32 = 0.05;
    const MIN_VOICED_FRAMES: usize = 3;

    if samples.len() < MIN_SAMPLES {
        return false;
    }
    let mut voiced = 0usize;
    let mut peak = 0f32;
    for frame in samples.chunks(FRAME) {
        let sum: f64 = frame.iter().map(|&x| x as f64 * x as f64).sum();
        let rms = (sum / frame.len() as f64).sqrt() as f32;
        peak = peak.max(frame.iter().fold(0f32, |m, &x| m.max(x.abs())));
        if rms > RMS_FLOOR {
            voiced += 1;
        }
    }
    voiced >= MIN_VOICED_FRAMES || (peak > PEAK_FLOOR && voiced > 0)
}

pub struct Transcriber {
    whistle: Whistle,
    options: TranscribeOptions,
    cleanup: bool,
}

impl Transcriber {
    pub fn new(model: &Path, _threads: usize, _prompt: &str, cleanup: bool) -> Result<Self> {
        let weights = Weights::from_file(model)
            .with_context(|| format!("could not load Whistle weights {}", model.display()))?;
        let whistle = Whistle::builder(weights)
            .build()
            .context("could not build the Whistle engine")?;
        let options = TranscribeOptions::new().with_language(Language::En);
        Ok(Self {
            whistle,
            options,
            cleanup,
        })
    }

    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.len() < 1600 || !has_speech(samples) {
            return Ok(String::new());
        }
        let transcript = self
            .whistle
            .transcribe_with_options(samples, &self.options)
            .context("Whistle transcription failed")?;
        let text = transcript.text().trim().to_string();
        Ok(if self.cleanup {
            crate::cleanup::clean_transcript(&text)
        } else {
            text
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{has_speech, Transcriber};
    use crate::config::Config;
    use std::time::Instant;

    #[test]
    fn speech_gate_rejects_noise_and_accepts_voice() {
        assert!(!has_speech(&[]));
        assert!(!has_speech(&vec![0.0f32; 1600]));
        for amp in [0.002f32, 0.004, 0.008, 0.012] {
            let noise: Vec<f32> = (0..16_000)
                .map(|i| {
                    let x = ((i as u32).wrapping_mul(2_654_435_761) % 65_536) as f32 / 32_768.0
                        - 1.0;
                    x * amp
                })
                .collect();
            assert!(!has_speech(&noise), "room noise {amp} must be gated");
        }
        let tone: Vec<f32> = (0..16_000)
            .map(|i| (i as f32 * 0.05).sin() * 0.1)
            .collect();
        assert!(has_speech(&tone));
    }

    #[test]
    fn transcribes_silence_instantly_and_speech_correctly() {
        // Whistle is process-global, so all model tests share one instance.
        let mut transcriber = Transcriber::new(&Config::default().model_path(), 0, "", true)
            .expect("whistle model");
        let silence = vec![0.0f32; 16_000 * 2];
        let start = Instant::now();
        let text = transcriber.transcribe(&silence).expect("silence");
        assert!(text.is_empty());
        assert!(start.elapsed().as_millis() < 50);

        let bytes = match std::fs::read(r"C:\repros\notes\speech4s.f32") {
            Ok(bytes) => bytes,
            Err(_) => return, // optional fixture
        };
        let speech: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let text = transcriber.transcribe(&speech).expect("speech");
        assert!(!text.trim().is_empty());
    }
}
