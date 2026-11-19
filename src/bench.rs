//! Accuracy benchmark on a folder of real recordings (opt-in).
//!
//! `FASTTTS_BENCH=<dir> cargo test --release -- --ignored bench`
//! reads `<dir>/refs.json` (`[{file, text}]`, 16 kHz mono WAV files) and
//! writes `<dir>/hyp-fasttts.json` with each streamed transcript, its
//! chunks and its timing.

use crate::{stream, transcribe::Transcriber};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Instant};

#[derive(Deserialize)]
struct Item {
    file: String,
}

#[derive(Serialize)]
struct Hypothesis {
    file: String,
    streamed: String,
    /// Streaming chunks as (start, end, continues) in samples, so other
    /// engines can be scored on exactly the same pieces.
    chunks: Vec<(usize, usize, bool)>,
    seconds: f32,
    compute_ms: u128,
}

fn read_wav(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
    bytes[data..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

#[test]
#[ignore]
fn bench() {
    let Ok(dir) = std::env::var("FASTTTS_BENCH") else {
        return;
    };
    let dir = Path::new(&dir);
    let items: Vec<Item> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("refs.json")).unwrap()).unwrap();
    // Chunking can be varied; CUTS_ONLY skips the model to just record cuts.
    let defaults = crate::config::Config::default();
    let env = |key: &str| std::env::var(key).ok();
    let chunk_seconds: f32 =
        env("CHUNK_SECONDS").map_or(defaults.chunk_seconds, |v| v.parse().unwrap());
    let silence_ms: u64 = env("SILENCE_MS").map_or(defaults.silence_ms, |v| v.parse().unwrap());
    let cuts_only = std::env::var("CUTS_ONLY").is_ok();
    let mut model = (!cuts_only).then(|| Transcriber::load().unwrap());
    let threshold = stream::voice_threshold(0.000_03);
    let mut out = Vec::new();
    for item in &items {
        let audio = read_wav(&dir.join(&item.file));
        let start = Instant::now();
        let mut chunker = stream::Chunker::new(chunk_seconds, silence_ms, threshold);
        let mut chunks = Vec::new();
        for slice in audio.chunks(528) {
            chunker.push(slice, &mut chunks);
        }
        chunks.extend(chunker.finish());
        let cuts = chunks
            .iter()
            .map(|c| (c.start, c.start + c.audio.len(), c.continues))
            .collect();
        let parts: Vec<(String, bool)> = match model.as_mut() {
            Some(model) => chunks
                .iter()
                .map(|c| (model.transcribe(&c.audio).unwrap(), c.continues))
                .collect(),
            None => Vec::new(),
        };
        let streamed = crate::cleanup::clean_transcript(&stream::join(&parts));
        let compute_ms = start.elapsed().as_millis();
        out.push(Hypothesis {
            file: item.file.clone(),
            streamed,
            chunks: cuts,
            seconds: audio.len() as f32 / 16_000.0,
            compute_ms,
        });
    }
    let name = std::env::var("BENCH_NAME").unwrap_or_else(|_| "fasttts".into());
    std::fs::write(
        dir.join(format!("hyp-{name}.json")),
        serde_json::to_string_pretty(&out).unwrap(),
    )
    .unwrap();
}
