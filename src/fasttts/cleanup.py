"""Conservative English cleanup for dictated text.

Whisper transcribes what it hears, so conversational audio often contains
fillers ("um", "uh") and stutters ("I I think", "th-th-the"). VAD only removes
silence. This pass is deterministic and easy to disable with ``cleanup = false``.
"""

from __future__ import annotations

import re

_FILLERS = (
    "um",
    "umm",
    "uh",
    "uhh",
    "er",
    "erm",
    "ah",
    "ahh",
    "hmm",
    "hmmm",
    "mhm",
    "mm",
    "mmm",
)

_ALTERNATIVES = "|".join(re.escape(filler) for filler in sorted(_FILLERS, key=len, reverse=True))


def _remove_fillers(text: str) -> str:
    # ", um," in the middle: remove the surrounding commas with the filler.
    text = re.sub(
        rf"(?i),\s*\b(?:{_ALTERNATIVES})\b\s*[.,;:!?]*\s*",
        " ",
        text,
    )
    # Standalone filler, with optional trailing punctuation and whitespace.
    text = re.sub(rf"(?i)\b(?:{_ALTERNATIVES})\b\s*[.,;:!?]*\s*", " ", text)
    return text


def _collapse_stutters(text: str) -> str:
    # "th-th-the" and "I-I-I" keep only the final fragment.
    text = re.sub(r"\b(?:[^\W\d_]{1,3}-)+(?=[^\W\d_])", "", text, flags=re.UNICODE)
    # "I—I" and "I–I".
    text = re.sub(r"\b(\w{1,3})[—–]\1\b", r"\1", text, flags=re.IGNORECASE)

    # Immediate duplicate short words: "I I I think", "the the". Longer repeats
    # ("very very") may be deliberate emphasis and are kept.
    def collapse(match: re.Match[str]) -> str:
        word = match.group(1)
        return word if len(word) <= 4 else match.group(0)

    return re.sub(r"\b(\w+)(?:\s+\1\b)+", collapse, text, flags=re.IGNORECASE)


def clean_transcript(text: str) -> str:
    """Remove common fillers and stutters, then tidy whitespace."""
    if not text:
        return ""
    cleaned = _remove_fillers(text)
    cleaned = _collapse_stutters(cleaned)
    cleaned = re.sub(r"\s+", " ", cleaned)
    cleaned = re.sub(r"\s+([,.;:!?])", r"\1", cleaned)
    cleaned = re.sub(r",\s*,+", ",", cleaned)
    cleaned = cleaned.strip(" ,;:")
    # Capitalize the start and the first word after sentence punctuation.
    cleaned = re.sub(
        r"([.!?]\s+)([a-z])",
        lambda match: match.group(1) + match.group(2).upper(),
        cleaned,
    )
    if cleaned and cleaned[0].isalpha():
        cleaned = cleaned[0].upper() + cleaned[1:]
    return cleaned
