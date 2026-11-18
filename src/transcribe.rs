//! English speech-to-text with Cactus Whistle, embedded in the executable.

use anyhow::{Context, Result};
use cactus_rs::whistle::{Language, TranscribeOptions, Weights, Whistle};

/// The 16.9 MB Whistle model (Apache-2.0), see NOTICE.
static MODEL: &[u8] = include_bytes!("../models/whistle.cact");

/// Quiet takes are raised to this peak before transcription.
const TARGET_PEAK: f32 = 0.5;
/// Never amplify more than this (+20 dB), so room noise stays noise.
const MAX_GAIN: f32 = 10.0;

pub struct Transcriber {
    whistle: Whistle,
    options: TranscribeOptions,
}

impl Transcriber {
    pub fn load() -> Result<Self> {
        let weights = Weights::from_bytes(MODEL).context("the embedded model is damaged")?;
        let whistle = Whistle::builder(weights)
            .build()
            .context("could not start the Whistle engine")?;
        Ok(Self {
            whistle,
            options: TranscribeOptions::new().with_language(Language::En),
        })
    }

    /// Transcribe one chunk of 16 kHz mono audio.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if !crate::stream::has_speech(samples) {
            return Ok(String::new());
        }
        let audio = normalize(samples);
        let transcript = self
            .whistle
            .transcribe_with_options(&audio, &self.options)
            .context("transcription failed")?;
        Ok(transcript.text().trim().to_string())
    }
}

/// Bring quiet speech up to a consistent level.
fn normalize(samples: &[f32]) -> Vec<f32> {
    let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
    let gain = if peak > 0.0 {
        (TARGET_PEAK / peak).clamp(1.0, MAX_GAIN)
    } else {
        1.0
    };
    samples.iter().map(|x| x * gain).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::stream::{join, Chunker};
    use std::time::Instant;

    /// The 16 kHz mono 16-bit speech fixture in `tests/fixtures`.
    pub(crate) fn speech() -> Vec<f32> {
        let bytes = include_bytes!("../tests/fixtures/speech.wav");
        let data = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("data chunk")
            + 8;
        bytes[data..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect()
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace()
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|w| !w.is_empty())
            .collect()
    }

    /// Word error rate of `hypothesis` against `reference` (Levenshtein).
    fn wer(reference: &str, hypothesis: &str) -> f32 {
        let (r, h) = (words(reference), words(hypothesis));
        let mut row: Vec<usize> = (0..=h.len()).collect();
        for i in 1..=r.len() {
            let mut previous = row[0];
            row[0] = i;
            for j in 1..=h.len() {
                let current = row[j];
                let cost = usize::from(r[i - 1] != h[j - 1]);
                row[j] = (row[j] + 1).min(row[j - 1] + 1).min(previous + cost);
                previous = current;
            }
        }
        row[h.len()] as f32 / r.len() as f32
    }

    const REFERENCE: &str = "Please send the quarterly report to the finance team before \
        Friday. I will review the numbers on Monday morning, and then we can schedule a meeting \
        with the whole department to discuss the budget for next year.";

    /// One test owns the model: Whistle allows a single engine per process.
    #[test]
    fn transcribes_whole_streamed_and_quiet_speech() {
        let mut model = Transcriber::load().expect("model");

        let start = Instant::now();
        assert!(model.transcribe(&vec![0.0; 32_000]).unwrap().is_empty());
        assert!(
            start.elapsed().as_millis() < 20,
            "silence must skip the model"
        );

        let speech = speech();
        let start = Instant::now();
        let whole = crate::cleanup::clean_transcript(&model.transcribe(&speech).unwrap());
        let seconds = start.elapsed().as_secs_f32();
        let audio_seconds = speech.len() as f32 / 16_000.0;
        println!("whole ({seconds:.2}s for {audio_seconds:.1}s): {whole}");
        assert!(wer(REFERENCE, &whole) < 0.15, "whole: {whole}");

        // Stream exactly as the app does: 33 ms slices, default settings.
        let config = crate::config::Config::default();
        let mut chunker = Chunker::new(config.chunk_seconds, config.silence_ms);
        let mut chunks = Vec::new();
        for slice in speech.chunks(528) {
            chunker.push(slice, &mut chunks);
        }
        chunks.extend(chunker.finish());
        assert!(chunks.len() >= 2, "the fixture must exercise streaming");
        let tail = chunks.last().unwrap().audio.len() as f32 / 16_000.0;
        let parts: Vec<(String, bool)> = chunks
            .iter()
            .map(|c| (model.transcribe(&c.audio).unwrap(), c.continues))
            .collect();
        let streamed = crate::cleanup::clean_transcript(&join(&parts));
        println!(
            "streamed ({} chunks, {tail:.1}s tail): {streamed}",
            chunks.len()
        );
        assert!(wer(REFERENCE, &streamed) < 0.2, "streamed: {streamed}");

        // A quiet microphone (-26 dB) must still transcribe.
        let quiet: Vec<f32> = speech.iter().map(|x| x * 0.05).collect();
        let text = crate::cleanup::clean_transcript(&model.transcribe(&quiet).unwrap());
        println!("quiet: {text}");
        assert!(wer(REFERENCE, &text) < 0.2, "quiet: {text}");
    }
}
