"""Offline tests for the English-only transcriber (fake faster-whisper)."""

from __future__ import annotations

import sys
from types import SimpleNamespace

import numpy as np
import pytest

from fasttts.config import Config
from fasttts.errors import TranscriptionError
from fasttts.stt import Transcriber


class FakeSegment:
    def __init__(self, text: str) -> None:
        self.text = text


class FakeModel:
    def __init__(self, *args, **kwargs) -> None:
        self.args = args
        self.kwargs = kwargs
        self.calls: list[dict] = []
        self.text = " hello there "
        self.fail = False

    def transcribe(self, audio, **kwargs):
        self.calls.append({"audio": audio, **kwargs})
        if self.fail:
            raise RuntimeError("decoder exploded")
        return [FakeSegment(self.text)], SimpleNamespace(language="en")


@pytest.fixture
def fake_whisper(monkeypatch):
    model = FakeModel()

    def factory(*args, **kwargs):
        model.args = args
        model.kwargs = kwargs
        return model

    module = SimpleNamespace(WhisperModel=factory)
    monkeypatch.setitem(sys.modules, "faster_whisper", module)
    return model


def test_load_uses_the_english_model(fake_whisper) -> None:
    transcriber = Transcriber(Config(model="tiny.en", compute_type="int8", cpu_threads=4))
    transcriber.load()
    assert fake_whisper.args == ("tiny.en",)
    assert fake_whisper.kwargs["device"] == "cpu"
    assert fake_whisper.kwargs["compute_type"] == "int8"
    assert fake_whisper.kwargs["cpu_threads"] == 4


def test_default_model_is_the_local_bundle(fake_whisper, monkeypatch, tmp_path) -> None:
    (tmp_path / "model.bin").write_bytes(b"model")
    monkeypatch.setattr("fasttts.stt.bundled_model_dir", lambda: tmp_path)
    transcriber = Transcriber(Config())
    reference = transcriber.model_ref()
    assert reference == str(tmp_path)
    transcriber.load()
    assert fake_whisper.args == (reference,)
    assert fake_whisper.kwargs["local_files_only"] is True


def test_missing_bundle_is_reported(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr("fasttts.stt.bundled_model_dir", lambda: tmp_path)
    with pytest.raises(TranscriptionError, match="bundled English model is missing"):
        Transcriber(Config()).model_ref()


def test_transcribe_is_fixed_to_english(fake_whisper) -> None:
    config = Config(model="tiny.en", beam_size=2, vad=False, initial_prompt="OpenCode")
    text = Transcriber(config).transcribe(np.zeros(1600, dtype=np.float32))
    assert text == "Hello there"
    call = fake_whisper.calls[0]
    assert call["language"] == "en"
    assert call["beam_size"] == 2
    assert call["vad_filter"] is False
    assert call["initial_prompt"] == "OpenCode"
    assert call["without_timestamps"] is True


def test_cleanup_can_be_disabled(fake_whisper) -> None:
    fake_whisper.text = " um hello "
    assert (
        Transcriber(Config(model="tiny.en", cleanup=False)).transcribe(np.zeros(10)) == "um hello"
    )
    assert Transcriber(Config(model="tiny.en", cleanup=True)).transcribe(np.zeros(10)) == "Hello"


def test_empty_audio_does_not_load_a_model(monkeypatch) -> None:
    transcriber = Transcriber(Config())
    monkeypatch.setattr(transcriber, "_load", lambda: pytest.fail("must not load"))
    assert transcriber.transcribe(np.zeros(0, dtype=np.float32)) == ""


def test_warmup_runs_a_silent_pass(fake_whisper) -> None:
    transcriber = Transcriber(Config(model="tiny.en"))
    transcriber.warmup()
    assert transcriber.loaded
    call = fake_whisper.calls[0]
    assert call["language"] == "en"
    assert call["audio"].size == 1600


def test_transcribe_errors_are_wrapped(fake_whisper) -> None:
    transcriber = Transcriber(Config(model="tiny.en"))
    fake_whisper.fail = True
    with pytest.raises(TranscriptionError, match="decoder exploded"):
        transcriber.transcribe(np.zeros(10))


def test_load_errors_are_wrapped(monkeypatch) -> None:
    class Broken:
        def __init__(self, *args, **kwargs):
            raise OSError("model missing")

    monkeypatch.setitem(sys.modules, "faster_whisper", SimpleNamespace(WhisperModel=Broken))
    with pytest.raises(TranscriptionError, match="model missing"):
        Transcriber(Config(model="tiny.en")).load()


def test_close_drops_the_model(fake_whisper) -> None:
    transcriber = Transcriber(Config(model="tiny.en"))
    transcriber.load()
    assert transcriber.loaded
    transcriber.close()
    assert not transcriber.loaded
