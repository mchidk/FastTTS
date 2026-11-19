//! English speech-to-text with NVIDIA Parakeet TDT 0.6B v2 on ONNX Runtime.
//!
//! Three ONNX models: `nemo128` turns audio into log-mel features, the
//! FastConformer encoder turns those into frames, and the TDT decoder walks
//! the frames, emitting a token and how many frames to skip at each step.

use anyhow::{anyhow, Context, Result};
use ort::{
    environment::Environment,
    session::Session,
    value::{Tensor, TensorRef},
};
use std::{
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use windows::{core::PCWSTR, Win32::System::LibraryLoader::SetDllDirectoryW};

const MODEL_DIR: &str = "models/parakeet-tdt-0.6b-v2";
const RUNTIME_DIR: &str = "runtime";
/// Silence around each chunk: the decoder commits a word only once it has
/// heard a little of what follows.
const PAD_SAMPLES: usize = 8_000;
/// Quiet takes are raised to this peak before transcription.
const TARGET_PEAK: f32 = 0.5;
/// Never amplify more than this (+20 dB), so room noise stays noise.
const MAX_GAIN: f32 = 10.0;
/// TDT emits at most this many tokens before moving to the next frame.
const MAX_TOKENS_PER_FRAME: usize = 10;
/// Decoder LSTM state: 2 layers of 640.
const STATE_SHAPE: [usize; 3] = [2, 1, 640];

/// Folder holding `models/` and `runtime/`: next to the exe when installed,
/// or the repository root when run from `target/`.
pub fn assets_root() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.ancestors()
        .skip(1)
        .find(|dir| dir.join(MODEL_DIR).join("vocab.txt").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("the speech model ({MODEL_DIR}) is missing next to fasttts.exe"))
}

/// ONNX Runtime allows one environment per process.
fn environment(runtime: &Path) -> Result<&'static Environment> {
    static ENV: OnceLock<Environment> = OnceLock::new();
    if let Some(env) = ENV.get() {
        return Ok(env);
    }
    // Let onnxruntime.dll find the Visual C++ runtime that sits next to it.
    let wide: Vec<u16> = runtime.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { SetDllDirectoryW(PCWSTR(wide.as_ptr()))? };
    let env = ort::init_from(runtime.join("onnxruntime.dll"))
        .map_err(|err| anyhow!("could not load ONNX Runtime: {err}"))?
        .with_name("fasttts")
        .build()
        .map_err(|err| anyhow!("could not start ONNX Runtime: {err}"))?;
    Ok(ENV.get_or_init(|| env))
}

pub struct Transcriber {
    features: Session,
    encoder: Session,
    decoder: Session,
    vocab: Vec<String>,
    blank: usize,
}

impl Transcriber {
    pub fn load() -> Result<Self> {
        let root = assets_root()?;
        let env = environment(&root.join(RUNTIME_DIR))?;
        let dir = root.join(MODEL_DIR);
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get() / 2)
            .clamp(1, 8);
        let session = |file: &str| -> Result<Session> {
            let mut builder = Session::builder(env)
                .and_then(|builder| Ok(builder.with_intra_threads(threads)?))
                .map_err(|err| anyhow!("could not configure ONNX Runtime: {err}"))?;
            builder
                .commit_from_file(dir.join(file))
                .map_err(|err| anyhow!("could not load {file}: {err}"))
        };
        let mut vocab = Vec::new();
        let text =
            std::fs::read_to_string(dir.join("vocab.txt")).context("could not read vocab.txt")?;
        for line in text.lines() {
            let (piece, index) = line.rsplit_once(' ').context("vocab.txt is damaged")?;
            let index: usize = index.parse().context("vocab.txt is damaged")?;
            if vocab.len() <= index {
                vocab.resize(index + 1, String::new());
            }
            vocab[index] = piece.replace('\u{2581}', " ");
        }
        let blank = vocab.len().checked_sub(1).context("vocab.txt is empty")?;
        Ok(Self {
            features: session("nemo128.onnx")?,
            encoder: session("encoder-model.int8.onnx")?,
            decoder: session("decoder_joint-model.int8.onnx")?,
            vocab,
            blank,
        })
    }

    /// Transcribe one chunk of 16 kHz mono audio. The chunker has already
    /// checked that it holds speech.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let audio = normalize(samples);
        self.decode(&audio)
            .map_err(|err| anyhow!("transcription failed: {err}"))
    }

    fn decode(&mut self, audio: &[f32]) -> ort::Result<String> {
        let features = self.features.run(ort::inputs![
            "waveforms" => TensorRef::from_array_view(([1usize, audio.len()], audio))?,
            "waveforms_lens" => Tensor::from_array(([1usize], vec![audio.len() as i64]))?,
        ])?;
        let encoded = self.encoder.run(ort::inputs![
            "audio_signal" => features["features"].view(),
            "length" => features["features_lens"].view(),
        ])?;
        let (shape, data) = encoded["outputs"].try_extract_tensor::<f32>()?;
        let (width, frames) = (shape[1] as usize, shape[2] as usize);
        let (_, lengths) = encoded["encoded_lengths"].try_extract_tensor::<i64>()?;
        let length = (lengths[0].max(0) as usize).min(frames);
        // Frame-major copy, so each decoder step reads one contiguous frame.
        let mut columns = vec![0f32; frames * width];
        for (channel, row) in data.chunks_exact(frames).enumerate() {
            for (frame, &value) in row.iter().enumerate() {
                columns[frame * width + channel] = value;
            }
        }

        let state_len: usize = STATE_SHAPE.iter().product();
        let mut states = (vec![0f32; state_len], vec![0f32; state_len]);
        let mut previous = self.blank as i32;
        let mut tokens = Vec::new();
        let (mut frame, mut emitted) = (0, 0);
        while frame < length {
            let column = &columns[frame * width..(frame + 1) * width];
            let step = self.decoder.run(ort::inputs![
                "encoder_outputs" => TensorRef::from_array_view(([1usize, width, 1], column))?,
                "targets" => Tensor::from_array(([1usize, 1], vec![previous]))?,
                "target_length" => Tensor::from_array(([1usize], vec![1i32]))?,
                "input_states_1" => TensorRef::from_array_view((STATE_SHAPE, states.0.as_slice()))?,
                "input_states_2" => TensorRef::from_array_view((STATE_SHAPE, states.1.as_slice()))?,
            ])?;
            let (_, logits) = step["outputs"].try_extract_tensor::<f32>()?;
            let (token_logits, skip_logits) = logits.split_at(self.blank + 1);
            let token = argmax(token_logits);
            let skip = argmax(skip_logits);
            if token != self.blank {
                states.0 = step["output_states_1"]
                    .try_extract_tensor::<f32>()?
                    .1
                    .to_vec();
                states.1 = step["output_states_2"]
                    .try_extract_tensor::<f32>()?
                    .1
                    .to_vec();
                previous = token as i32;
                tokens.push(token);
                emitted += 1;
            }
            if skip > 0 {
                frame += skip;
                emitted = 0;
            } else if token == self.blank || emitted == MAX_TOKENS_PER_FRAME {
                frame += 1;
                emitted = 0;
            }
        }
        let text: String = tokens.iter().map(|&t| self.vocab[t].as_str()).collect();
        Ok(text.trim().to_string())
    }
}

fn argmax(values: &[f32]) -> usize {
    let mut best = 0;
    for (index, value) in values.iter().enumerate() {
        if *value > values[best] {
            best = index;
        }
    }
    best
}

/// Bring quiet speech up to a consistent level, framed by silence.
fn normalize(samples: &[f32]) -> Vec<f32> {
    let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
    let gain = if peak > 0.0 {
        (TARGET_PEAK / peak).clamp(1.0, MAX_GAIN)
    } else {
        1.0
    };
    let mut audio = vec![0.0; PAD_SAMPLES];
    audio.extend(samples.iter().map(|x| x * gain));
    audio.extend(std::iter::repeat_n(0.0, PAD_SAMPLES));
    audio
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

    /// One test owns the model: it is large, so load it once.
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
