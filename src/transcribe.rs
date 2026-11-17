//! English-only speech recognition with a local whisper.cpp ggml model.

use anyhow::{Context, Result};
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

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
        if samples.len() < 1600 {
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

        let text = state
            .as_iter()
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
