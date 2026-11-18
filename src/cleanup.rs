//! Conservative cleanup of dictated English.

use regex::{Captures, Regex};
use std::sync::OnceLock;

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid pattern"))
}

/// "um", "uh" and friends, with the punctuation the model put around them.
fn fillers() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"(?i)(?:,\s*)?\b(?:um+|uh+|erm|er|ah+|hmm+|mhm|mm+)\b[.,;:!?]*\s*",
    )
}

/// A run of short fragments with hyphens in front of a word: "th-th-the".
fn stutter() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"\b((?:[A-Za-z]{1,3}-)+)([A-Za-z]+)")
}

fn space_before_punctuation() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"\s+([,.;:!?])")
}

fn sentence_start() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"([.!?]\s+)([a-z])")
}

/// Real English doubles that must survive ("she had had enough").
const VALID_DOUBLES: &[&str] = &["had", "that"];

/// Drop a fragment run only when every fragment starts the word that
/// follows, so "th-th-the" collapses but "e-mail" and "x-ray" stay.
fn collapse_stutters(text: &str) -> String {
    stutter()
        .replace_all(text, |caps: &Captures| {
            let word = caps[2].to_ascii_lowercase();
            let repeats = caps[1]
                .split_terminator('-')
                .all(|fragment| word.starts_with(&fragment.to_ascii_lowercase()));
            if repeats {
                caps[2].to_string()
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

/// "I I I think" becomes "I think".
fn collapse_repeats(text: &str) -> String {
    let mut words: Vec<&str> = Vec::new();
    for word in text.split_whitespace() {
        let repeat = words.last().is_some_and(|last| {
            last.eq_ignore_ascii_case(word)
                && word.chars().count() <= 4
                && !VALID_DOUBLES.contains(&word.to_ascii_lowercase().as_str())
        });
        if !repeat {
            words.push(word);
        }
    }
    words.join(" ")
}

pub fn clean_transcript(text: &str) -> String {
    let text = fillers().replace_all(text, " ");
    let text = collapse_stutters(&text);
    let text = collapse_repeats(&text);
    let text = space_before_punctuation().replace_all(&text, "$1");
    let text = text.trim_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':'));
    let mut text = sentence_start()
        .replace_all(text, |caps: &Captures| {
            format!("{}{}", &caps[1], caps[2].to_uppercase())
        })
        .into_owned();
    if let Some(first) = text.chars().next().filter(char::is_ascii_lowercase) {
        text.replace_range(..1, &first.to_ascii_uppercase().to_string());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::clean_transcript as clean;

    #[test]
    fn removes_fillers_stutters_and_repeats() {
        assert_eq!(clean("um, hello there"), "Hello there");
        assert_eq!(
            clean("so, uh, we start. um. then we stop"),
            "So we start. Then we stop"
        );
        assert_eq!(clean("I I I think"), "I think");
        assert_eq!(clean("th-th-the start"), "The start");
        assert_eq!(clean("very very good"), "Very good");
    }

    #[test]
    fn keeps_real_words() {
        assert_eq!(
            clean("send an e-mail to my co-worker"),
            "Send an e-mail to my co-worker"
        );
        assert_eq!(clean("the x-ray was clear"), "The x-ray was clear");
        assert_eq!(clean("she had had enough"), "She had had enough");
        assert_eq!(clean("the summer was warm"), "The summer was warm");
        assert_eq!(clean("Hello, world!"), "Hello, world!");
    }

    #[test]
    fn handles_empty_and_spacing() {
        assert_eq!(clean(""), "");
        assert_eq!(clean("  um  "), "");
        assert_eq!(clean("hello , world ."), "Hello, world.");
    }
}
