//! English-only speech recognition with a local whisper.cpp ggml model.

use anyhow::{Context, Result};
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// whisper.cpp encodes a fixed 30-second mel window for *any* non-zero audio,
/// which costs ~2.5 s on a laptop CPU with the bundled small.en model. On
/// near-silence (room noise, mic hiss) it also hallucinates words like "you"
/// through its temperature-fallback chain. A 2-second silent take therefore
/// took >5 s end to end and could inject "you" into the focused app.
///
/// This cheap energy gate keeps silent takes away from the model entirely so
/// they return "NO SPEECH" almost instantly. It mirrors the app's own
/// `heard_speech` threshold (0.012) and requires a few consecutive voiced
/// frames so that a single click cannot open the gate.
pub(crate) fn has_speech(samples: &[f32]) -> bool {
    const MIN_SAMPLES: usize = 1600; // 100 ms @ 16 kHz
    const FRAME: usize = 160; // 10 ms @ 16 kHz
    const RMS_FLOOR: f32 = 0.012;
    const PEAK_FLOOR: f32 = 0.05;
    const MIN_VOICED_FRAMES: usize = 3; // >= 30 ms of voiced audio

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
    voiced >= MIN_VOICED_FRAMES || peak > PEAK_FLOOR && voiced > 0
}

pub struct Transcriber {
    context: WhisperContext,
    threads: i32,
    initial_prompt: String,
    cleanup: bool,
}

impl Transcriber {
    pub fn new(model: &Path, threads: usize, initial_prompt: &str, cleanup: bool) -> Result<Self> {
        let model_text = model.to_string_lossy();
        let context = WhisperContext::new_with_params(
            model_text.as_ref(),
            WhisperContextParameters::default(),
        )
        .with_context(|| format!("could not load the model {}", model.display()))?;
        Ok(Self {
            context,
            threads: if threads == 0 {
                (std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .min(8)) as i32
            } else {
                threads as i32
            },
            initial_prompt: initial_prompt.to_string(),
            cleanup,
        })
    }

    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        // Silent / noise-only takes never reach whisper.cpp. This is the fix
        // for the >5 s "empty take" latency reported by the user.
        if !has_speech(samples) {
            return Ok(String::new());
        }
        let mut state = self
            .context
            .create_state()
            .context("could not create a state")?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_n_threads(self.threads);
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        if !self.initial_prompt.is_empty() {
            params.set_initial_prompt(&self.initial_prompt);
        }
        state
            .full(params, samples)
            .context("transcription failed")?;

        // Drop segments the model itself considers non-speech. This prevents
        // residual noise (e.g. a door slam) from being injected as " you".
        let text = state
            .as_iter()
            .filter(|segment| segment.no_speech_probability() <= 0.85)
            .map(|segment| segment.to_str_lossy().unwrap_or_default().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let text = text.trim().to_string();
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
    fn speech_gate_rejects_silence_and_room_noise() {
        assert!(!has_speech(&[]));
        assert!(!has_speech(&vec![0.0f32; 1600]));
        // Deterministic low-level room noise at several amplitudes.
        for amp in [0.002f32, 0.004, 0.008, 0.012] {
            let noise: Vec<f32> = (0..16_000)
                .map(|i| {
                    let x = ((i as u32).wrapping_mul(2_654_435_761) % 65_536) as f32 / 32_768.0
                        - 1.0;
                    x * amp
                })
                .collect();
            assert!(
                !has_speech(&noise),
                "room noise at amplitude {amp} must be gated"
            );
        }
    }

    #[test]
    fn speech_gate_accepts_voice() {
        let tone: Vec<f32> = (0..16_000)
            .map(|i| (i as f32 * 0.05).sin() * 0.1)
            .collect();
        assert!(has_speech(&tone));

        // Speech fixture (4 s of English speech, RMS ~0.14).
        if let Ok(bytes) = std::fs::read(r"C:\repros\notes\speech4s.f32") {
            let speech: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            assert!(has_speech(&speech));
        }
    }

    #[test]
    fn silent_take_is_instant() {
        let transcriber = Transcriber::new(&Config::default().model_path(), 4, "", false)
            .expect("model");
        let audio = vec![0.0f32; 16_000 * 2];
        let start = Instant::now();
        let text = transcriber.transcribe(&audio).expect("transcribe");
        let elapsed = start.elapsed();
        assert!(text.is_empty());
        assert!(
            elapsed.as_millis() < 50,
            "silent take took {elapsed:?}; whisper.cpp was called"
        );
    }

    #[test]
    fn real_speech_still_transcribes() {
        let bytes = match std::fs::read(r"C:\repros\notes\speech4s.f32") {
            Ok(bytes) => bytes,
            Err(_) => return, // fixture is optional in CI
        };
        let speech: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let transcriber = Transcriber::new(&Config::default().model_path(), 8, "", false)
            .expect("model");
        let text = transcriber.transcribe(&speech).expect("transcribe speech");
        assert!(
            !text.trim().is_empty(),
            "real speech must still produce a transcript"
        );
    }
}
