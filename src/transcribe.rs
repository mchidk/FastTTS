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

    /// Transcribe one chunk of 16 kHz mono audio. The chunker has already
    /// checked that it holds speech.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
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
    use crate::stream::{join, voice_threshold, Chunker};
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
    fn transcribes_whole_streamed_and_soft_speech() {
        let mut model = Transcriber::load().expect("model");
        let speech = speech();

        let start = Instant::now();
        let whole = crate::cleanup::clean_transcript(&model.transcribe(&speech).unwrap());
        let seconds = start.elapsed().as_secs_f32();
        let audio_seconds = speech.len() as f32 / 16_000.0;
        println!("whole ({seconds:.2}s for {audio_seconds:.1}s): {whole}");
        assert!(wer(REFERENCE, &whole) < 0.15, "whole: {whole}");

        // A processed laptop mic array: -90 dBFS room, so the adaptive
        // threshold settles at its floor.
        let laptop = voice_threshold(0.000_03);
        let (streamed, chunks) = stream_take(&mut model, &speech, laptop);
        println!("streamed ({chunks} chunks): {streamed}");
        assert!(chunks >= 2, "the fixture must exercise streaming");
        assert!(wer(REFERENCE, &streamed) < 0.2, "streamed: {streamed}");

        // Soft speech on that mic, measured around -48 dBFS. The old fixed
        // -42 dBFS gate threw these takes away as silence.
        for (gain, label) in [(0.03f32, "-48 dBFS"), (0.015, "-54 dBFS")] {
            let soft: Vec<f32> = speech.iter().map(|x| x * gain).collect();
            let (old, _) = stream_take(&mut model, &soft, voice_threshold(1.0));
            let (text, chunks) = stream_take(&mut model, &soft, laptop);
            println!("soft {label} ({chunks} chunks): {text} | old gate: [{old}]");
            assert!(wer(REFERENCE, &text) < 0.2, "soft {label}: {text}");
        }

        // Silence never reaches the model.
        let (text, chunks) = stream_take(&mut model, &vec![0.0; 48_000], laptop);
        assert!(text.is_empty() && chunks == 0);

        long_dictation(&mut model, &speech, laptop);
    }

    /// A minute of soft speech with only short breaths, replayed on the real
    /// timeline: chunks reach the model while the key is held, and the wait
    /// after release is whatever the model has not finished by then.
    fn long_dictation(model: &mut Transcriber, speech: &[f32], threshold: f32) {
        let mut audio = Vec::new();
        for _ in 0..5 {
            audio.extend(speech.iter().map(|x| x * 0.03));
            audio.extend(std::iter::repeat_n(0.0, 4_800)); // 0.3 s breath
        }
        // Let go right on the last word: the worst case for the wait.
        let last_word = audio.iter().rposition(|x| x.abs() > 0.002).unwrap_or(0);
        audio.truncate(last_word + 1);
        let config = crate::config::Config::default();
        let mut chunker = Chunker::new(config.chunk_seconds, config.silence_ms, threshold);
        let mut busy_until = 0f32; // model timeline, seconds of audio time
        let mut parts = Vec::new();
        let mut longest = 0f32;
        let mut transcribe = |chunk: crate::stream::Chunk, now: f32, busy: &mut f32| {
            longest = longest.max(chunk.audio.len() as f32 / 16_000.0);
            let start = Instant::now();
            let text = model.transcribe(&chunk.audio).unwrap();
            *busy = busy.max(now) + start.elapsed().as_secs_f32();
            parts.push((text, chunk.continues));
        };
        for (i, slice) in audio.chunks(528).enumerate() {
            let now = (i + 1) as f32 * 528.0 / 16_000.0;
            let mut ready = Vec::new();
            chunker.push(slice, &mut ready);
            for chunk in ready {
                transcribe(chunk, now, &mut busy_until);
            }
        }
        let end = audio.len() as f32 / 16_000.0;
        if let Some(tail) = chunker.finish() {
            transcribe(tail, end, &mut busy_until);
        }
        let wait = (busy_until - end).max(0.0);
        let text = crate::cleanup::clean_transcript(&join(&parts));
        let reference = [REFERENCE; 5].join(" ");
        let error = wer(&reference, &text);
        println!(
            "long ({end:.0}s, {} chunks, longest {longest:.1}s): wait after release {:.0} ms, WER {:.1}%",
            parts.len(),
            wait * 1000.0,
            error * 100.0
        );
        assert!(parts.len() >= 8, "a minute must stream in many chunks");
        assert!(longest <= config.chunk_seconds + 0.05);
        assert!(wait < 0.6, "release must not wait for the whole minute");
        assert!(error < 0.1, "long: {text}");
    }

    /// Run audio through the chunker and model exactly as a take does:
    /// 33 ms slices, default settings.
    fn stream_take(model: &mut Transcriber, audio: &[f32], threshold: f32) -> (String, usize) {
        let config = crate::config::Config::default();
        let mut chunker = Chunker::new(config.chunk_seconds, config.silence_ms, threshold);
        let mut chunks = Vec::new();
        for slice in audio.chunks(528) {
            chunker.push(slice, &mut chunks);
        }
        chunks.extend(chunker.finish());
        let parts: Vec<(String, bool)> = chunks
            .iter()
            .map(|c| (model.transcribe(&c.audio).unwrap(), c.continues))
            .collect();
        (
            crate::cleanup::clean_transcript(&join(&parts)),
            chunks.len(),
        )
    }
}
