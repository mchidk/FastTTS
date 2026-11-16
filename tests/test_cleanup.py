"""Offline tests for English filler and stutter cleanup."""

from __future__ import annotations

import pytest

from fasttts.cleanup import clean_transcript


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("um, hello there", "Hello there"),
        ("I am, um, ready", "I am ready"),
        ("uh I think so", "I think so"),
        ("hmm let me see", "Let me see"),
        ("That is, er, fine", "That is fine"),
        ("so um uh yes", "So yes"),
        ("I I I think", "I think"),
        ("the the the end", "The end"),
        ("th-th-the start", "The start"),
        ("I—I know", "I know"),
        ("very very good", "Very good"),
        ("  spaced   out  ", "Spaced out"),
        ("", ""),
    ],
)
def test_clean_transcript(raw: str, expected: str) -> None:
    assert clean_transcript(raw) == expected


def test_capitalizes_after_sentence_punctuation() -> None:
    assert clean_transcript("one. two. three!") == "One. Two. Three!"
