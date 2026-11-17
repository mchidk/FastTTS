//! Conservative English cleanup for dictated text.

use regex::Regex;
use std::sync::OnceLock;

fn fillers() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:um|umm|uh|uhh|er|erm|ah|ahh|hmm|hmmm|mhm|mm|mmm)\b\s*[.,;:!?]*\s*")
            .unwrap()
    })
}

fn comma_fillers() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i),\s*\b(?:um|umm|uh|uhh|er|erm|ah|ahh|hmm|hmmm|mhm|mm|mmm)\b\s*[.,;:!?]*\s*",
        )
        .unwrap()
    })
}

fn hyphen_stutter() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(?:[^\W\d_]{1,3}-)+([^\W\d_])").unwrap())
}

fn collapse_duplicates(text: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if let Some(previous) = words.last() {
            let same = previous.eq_ignore_ascii_case(word);
            if same && word.chars().count() <= 4 {
                continue;
            }
        }
        words.push(word.to_string());
    }
    words.join(" ")
}

fn punctuation_space() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+([,.;:!?])").unwrap())
}

fn whitespace() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").unwrap())
}

fn sentence_start() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([.!?]\s+)([a-z])").unwrap())
}

pub fn clean_transcript(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let mut text = comma_fillers().replace_all(text, " ").into_owned();
    text = fillers().replace_all(&text, " ").into_owned();
    text = hyphen_stutter().replace_all(&text, "$1").into_owned();
    text = collapse_duplicates(&text);
    text = whitespace().replace_all(&text, " ").into_owned();
    text = punctuation_space().replace_all(&text, "$1").into_owned();
    text = text
        .trim_matches(|c| matches!(c, ' ' | ',' | ';' | ':'))
        .to_string();
    text = sentence_start()
        .replace_all(&text, |caps: &regex::Captures| {
            format!("{}{}", &caps[1], caps[2].to_uppercase())
        })
        .into_owned();
    if let Some(first) = text.chars().next() {
        if first.is_ascii_alphabetic() {
            text.replace_range(0..first.len_utf8(), &first.to_ascii_uppercase().to_string());
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::clean_transcript;

    #[test]
    fn removes_fillers_and_stutters() {
        assert_eq!(clean_transcript("um, hello there"), "Hello there");
        assert_eq!(clean_transcript("I I I think"), "I think");
        assert_eq!(clean_transcript("th-th-the start"), "The start");
        assert_eq!(clean_transcript("very very good"), "Very good");
    }
}
