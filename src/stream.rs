//! Streaming: split a live 16 kHz take into phrases while Caps Lock is held.
//!
//! The chunker works on sample time, not wall-clock time, so it behaves the
//! same however often the UI thread drains the microphone. A phrase is cut at
//! a pause; a phrase that runs past `chunk_seconds` is cut at the quietest
//! moment of its last 1.5 s so a word is never split in half.

use crate::audio::RATE;

/// 20 ms analysis frame.
const FRAME: usize = RATE / 50;
/// Voice must be this much louder than the room (about +14 dB).
const OVER_NOISE: f32 = 5.0;
/// The voice threshold never drops below -56 dBFS or rises above -42 dBFS.
const MIN_VOICE_RMS: f32 = 0.0015;
const MAX_VOICE_RMS: f32 = 0.008;

/// Frame RMS that counts as voice, given the room's noise floor. A
/// processed laptop array sits near -90 dBFS and quiet speech near -45, so
/// a fixed threshold either misses soft voices or triggers on noisy rooms.
pub fn voice_threshold(noise_floor: f32) -> f32 {
    (noise_floor * OVER_NOISE).clamp(MIN_VOICE_RMS, MAX_VOICE_RMS)
}
/// A chunk needs at least this many voiced frames (100 ms) to be worth
/// sending to the model; shorter blips are clicks and key taps.
const MIN_VOICED_FRAMES: usize = 5;
/// Lead-in kept in front of a phrase while nobody is speaking.
const LEAD_IN: usize = RATE * 3 / 10;
/// Forced cuts look this far back for the quietest frame.
const CUT_SEARCH: usize = RATE * 3 / 2;
/// A phrase must be at least this long before a pause may end it.
const MIN_PHRASE: usize = RATE;

pub struct Chunk {
    pub audio: Vec<f32>,
    /// Position of the first sample within the take (used by the bench).
    #[cfg_attr(not(test), allow(dead_code))]
    pub start: usize,
    /// True when the cut was forced mid-speech, so the next chunk continues
    /// the same sentence.
    pub continues: bool,
}

pub struct Chunker {
    threshold: f32,
    /// Loudest frame and voiced frame count of the whole take, for the log.
    pub loudest: f32,
    pub voiced_total: usize,
    max_len: usize,
    /// Position within the take of `buf[0]`.
    origin: usize,
    pause_frames: usize,
    buf: Vec<f32>,
    frames: Vec<f32>,
    voiced: usize,
    quiet: usize,
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

/// True when `samples` holds enough voiced frames to be worth transcribing.
pub fn has_speech(samples: &[f32], threshold: f32) -> bool {
    samples
        .chunks(FRAME)
        .filter(|frame| rms(frame) > threshold)
        .count()
        >= MIN_VOICED_FRAMES
}

impl Chunker {
    pub fn new(chunk_seconds: f32, pause_ms: u64, threshold: f32) -> Self {
        Self {
            threshold,
            loudest: 0.0,
            voiced_total: 0,
            max_len: ((chunk_seconds * RATE as f32) as usize).max(MIN_PHRASE + FRAME),
            pause_frames: (pause_ms as usize).div_ceil(20).max(1),
            buf: Vec::new(),
            origin: 0,
            frames: Vec::new(),
            voiced: 0,
            quiet: 0,
        }
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Feed new samples; completed chunks are appended to `out`.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<Chunk>) {
        for &sample in samples {
            self.buf.push(sample);
            if self.buf.len().is_multiple_of(FRAME) {
                self.end_frame(out);
            }
        }
    }

    /// Flush whatever is left when Caps Lock is released.
    pub fn finish(&mut self) -> Option<Chunk> {
        let start = self.origin;
        let audio = std::mem::take(&mut self.buf);
        let speech = self.voiced >= MIN_VOICED_FRAMES || has_speech(&audio, self.threshold);
        self.origin += audio.len();
        self.reset();
        speech.then_some(Chunk {
            audio,
            start,
            continues: false,
        })
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.frames.clear();
        self.voiced = 0;
        self.quiet = 0;
    }

    fn end_frame(&mut self, out: &mut Vec<Chunk>) {
        let level = rms(&self.buf[self.buf.len() - FRAME..]);
        self.frames.push(level);
        self.loudest = self.loudest.max(level);
        if level > self.threshold {
            self.voiced += 1;
            self.voiced_total += 1;
            self.quiet = 0;
        } else {
            self.quiet += 1;
        }

        if self.voiced == 0 {
            // Nobody is talking yet: keep only a short lead-in.
            if self.buf.len() >= LEAD_IN + RATE {
                let drop = (self.buf.len() - LEAD_IN) / FRAME * FRAME;
                self.buf.drain(..drop);
                self.frames.drain(..drop / FRAME);
                self.origin += drop;
            }
            return;
        }

        if self.quiet >= self.pause_frames && self.buf.len() >= MIN_PHRASE {
            // A pause ended the phrase.
            let start = self.origin;
            let audio = std::mem::take(&mut self.buf);
            let speech = self.voiced >= MIN_VOICED_FRAMES;
            self.origin += audio.len();
            self.reset();
            if speech {
                out.push(Chunk {
                    audio,
                    start,
                    continues: false,
                });
            }
        } else if self.buf.len() >= self.max_len {
            let cut = self.quietest_cut();
            let rest = self.buf.split_off(cut);
            let audio = std::mem::replace(&mut self.buf, rest);
            let start = self.origin;
            self.origin += cut;
            self.frames.drain(..cut / FRAME);
            let threshold = self.threshold;
            self.voiced = self.frames.iter().filter(|&&f| f > threshold).count();
            self.quiet = self
                .frames
                .iter()
                .rev()
                .take_while(|&&f| f <= threshold)
                .count();
            // Only a cut with speech right after it splits a sentence.
            out.push(Chunk {
                audio,
                start,
                continues: self.voiced > 0,
            });
        }
    }

    /// Frame-aligned cut position at the quietest frame near the end.
    fn quietest_cut(&self) -> usize {
        let count = self.frames.len();
        let first = (count.saturating_sub(CUT_SEARCH / FRAME)).max(MIN_PHRASE / FRAME);
        let index = (first..count)
            .min_by(|&a, &b| self.frames[a].total_cmp(&self.frames[b]))
            .unwrap_or(count);
        // Cut in front of the quiet frame, so it leads the next chunk.
        index.max(1) * FRAME
    }
}

/// Words that never start a sentence after a forced mid-sentence cut unless
/// they are capitalised for that reason alone.
const CONTINUATION_WORDS: &[&str] = &[
    "a", "about", "after", "also", "an", "and", "are", "as", "at", "be", "because", "been",
    "before", "but", "by", "can", "could", "for", "from", "had", "has", "have", "he", "her", "him",
    "his", "if", "in", "into", "is", "it", "its", "just", "me", "my", "not", "of", "on", "or",
    "our", "she", "should", "so", "than", "that", "the", "their", "them", "then", "there", "these",
    "they", "this", "those", "to", "up", "us", "was", "we", "were", "what", "when", "where",
    "which", "while", "who", "will", "with", "would", "you", "your",
];

/// Join chunk transcripts. Across a forced cut the model sees two half
/// sentences and punctuates each as complete; undo that at the seam.
pub fn join(parts: &[(String, bool)]) -> String {
    let parts: Vec<(&str, bool)> = parts
        .iter()
        .map(|(text, continues)| (text.trim(), *continues))
        .filter(|(text, _)| !text.is_empty())
        .collect();
    let mut text = String::new();
    let mut glue = false;
    for (index, &(part, continues)) in parts.iter().enumerate() {
        let mut part = part.to_string();
        // The seam only exists if another part follows.
        let continues = continues && index + 1 < parts.len();
        if glue {
            lowercase_continuation(&mut part);
        }
        if continues && part.ends_with('.') && !part.ends_with("..") {
            part.pop();
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&part);
        glue = continues;
    }
    text
}

fn lowercase_continuation(part: &mut String) {
    let word: String = part
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let lower = word.to_ascii_lowercase();
    if word.len() > 1 && word != lower && CONTINUATION_WORDS.contains(&lower.as_str()) {
        part.replace_range(..1, &lower[..1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f32, amp: f32) -> Vec<f32> {
        (0..(seconds * RATE as f32) as usize)
            .map(|i| (i as f32 * 0.07).sin() * amp)
            .collect()
    }

    fn silence(seconds: f32) -> Vec<f32> {
        vec![0.0; (seconds * RATE as f32) as usize]
    }

    #[test]
    fn silence_never_produces_chunks() {
        let mut chunker = Chunker::new(4.0, 350, MAX_VOICE_RMS);
        let mut out = Vec::new();
        chunker.push(&silence(20.0), &mut out);
        assert!(out.is_empty());
        assert!(chunker.finish().is_none());
        assert!(chunker.buf.len() <= LEAD_IN + RATE);
    }

    #[test]
    fn a_pause_ends_a_phrase() {
        let mut chunker = Chunker::new(8.0, 350, MAX_VOICE_RMS);
        let mut out = Vec::new();
        chunker.push(&silence(0.5), &mut out);
        chunker.push(&tone(1.5, 0.2), &mut out);
        chunker.push(&silence(0.5), &mut out);
        assert_eq!(out.len(), 1);
        assert!(!out[0].continues);
        chunker.push(&tone(1.2, 0.2), &mut out);
        let tail = chunker.finish().expect("tail");
        assert!(!tail.continues);
        let total: usize = out.iter().map(|c| c.audio.len()).sum::<usize>() + tail.audio.len();
        assert!(total >= RATE * 27 / 10, "speech must not be dropped");
    }

    #[test]
    fn long_speech_is_cut_at_the_quietest_point() {
        let mut chunker = Chunker::new(4.0, 350, MAX_VOICE_RMS);
        let mut out = Vec::new();
        let mut speech = tone(3.0, 0.2);
        speech.extend(tone(0.1, 0.001)); // a short gap between words
        speech.extend(tone(3.0, 0.2));
        chunker.push(&speech, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].continues);
        let cut = out[0].audio.len();
        assert!(
            (3 * RATE..=3 * RATE + RATE / 10).contains(&cut),
            "cut at {cut}"
        );
        let tail = chunker.finish().expect("tail");
        assert_eq!(cut + tail.audio.len(), speech.len(), "no samples lost");
    }

    #[test]
    fn a_cut_in_trailing_silence_does_not_continue() {
        let mut chunker = Chunker::new(4.0, 500, MAX_VOICE_RMS);
        let mut out = Vec::new();
        chunker.push(&tone(3.7, 0.2), &mut out);
        chunker.push(&silence(0.45), &mut out);
        assert_eq!(out.len(), 1);
        assert!(!out[0].continues, "nothing follows the cut");
    }

    #[test]
    fn clicks_are_not_speech() {
        let mut chunker = Chunker::new(4.0, 350, MAX_VOICE_RMS);
        let mut out = Vec::new();
        chunker.push(&silence(0.5), &mut out);
        chunker.push(&tone(0.04, 0.5), &mut out);
        chunker.push(&silence(2.0), &mut out);
        assert!(out.is_empty());
        assert!(chunker.finish().is_none());
    }

    #[test]
    fn the_voice_threshold_follows_the_room() {
        // Processed laptop array: -90 dBFS room, so soft speech counts.
        assert_eq!(voice_threshold(0.000_03), MIN_VOICE_RMS);
        // Raw mic at -60 dBFS: about +14 dB above it.
        assert!((voice_threshold(0.001) - 0.005).abs() < 1e-6);
        // Noisy room: never stricter than the old fixed gate.
        assert_eq!(voice_threshold(0.01), MAX_VOICE_RMS);
    }

    #[test]
    fn speech_gate() {
        let gate = MAX_VOICE_RMS;
        assert!(!has_speech(&[], gate));
        assert!(!has_speech(&silence(1.0), gate));
        for amp in [0.002f32, 0.006, 0.012] {
            let noise: Vec<f32> = (0..RATE)
                .map(|i| {
                    let x = ((i as u32).wrapping_mul(2_654_435_761) % 65_536) as f32 / 32_768.0;
                    (x - 1.0) * amp
                })
                .collect();
            assert!(!has_speech(&noise, gate), "room noise {amp} must be gated");
        }
        assert!(has_speech(&tone(0.5, 0.05), gate));
    }

    #[test]
    fn joins_forced_cuts_into_one_sentence() {
        let parts = vec![
            ("Send the report to the.".to_string(), true),
            ("Finance team before Friday.".to_string(), false),
            ("We should meet.".to_string(), true),
            ("On Monday.".to_string(), false),
        ];
        assert_eq!(
            join(&parts),
            "Send the report to the Finance team before Friday. We should meet on Monday."
        );
        let names = vec![
            ("I met.".to_string(), true),
            ("John today.".to_string(), false),
        ];
        assert_eq!(join(&names), "I met John today.");
        let pronoun = vec![("Yes.".to_string(), true), ("I agree.".to_string(), false)];
        assert_eq!(join(&pronoun), "Yes I agree.");
        let last = vec![("Hello world.".to_string(), true), (String::new(), false)];
        assert_eq!(join(&last), "Hello world.", "no seam without a next part");
    }
}
