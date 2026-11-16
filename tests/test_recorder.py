"""Offline tests for the microphone capture buffer (no audio device)."""

from __future__ import annotations

import numpy as np
import pytest

from fasttts.errors import MicrophoneError
from fasttts.recorder import Recorder


def feed(recorder: Recorder, samples: int, value: float = 0.1) -> None:
    block = np.full((samples, 1), value, dtype=np.float32)
    recorder._callback(block, samples, None, None)


def open_recorder(**kwargs) -> Recorder:
    kwargs.setdefault("pre_roll_seconds", 0.1)
    recorder = Recorder(sample_rate=1000, **kwargs)
    recorder._stream = object()  # pretend an open stream; no sounddevice
    return recorder


def test_pre_roll_is_included_in_the_take() -> None:
    recorder = open_recorder()
    feed(recorder, 50)
    feed(recorder, 50)
    feed(recorder, 50)  # ring keeps the newest 100 samples
    recorder.start()
    feed(recorder, 200)
    audio = recorder.stop()
    assert recorder.streaming
    assert audio.size == 300
    assert np.allclose(audio, 0.1)


def test_start_without_pre_roll() -> None:
    recorder = open_recorder(pre_roll_seconds=0.0)
    feed(recorder, 100)
    recorder.start()
    feed(recorder, 50)
    assert recorder.stop().size == 50


def test_max_seconds_truncates() -> None:
    recorder = open_recorder(max_seconds=0.12)
    recorder.start()
    feed(recorder, 200)
    assert recorder.stop().size == 120


def test_stop_without_audio_is_empty() -> None:
    assert open_recorder().stop().size == 0


def test_cancel_discards_the_take_but_keeps_the_stream() -> None:
    recorder = open_recorder()
    recorder.start()
    feed(recorder, 100)
    recorder.cancel()
    assert not recorder.active
    assert recorder.streaming
    assert recorder.stop().size == 0


def test_start_twice_is_rejected() -> None:
    recorder = open_recorder()
    recorder.start()
    with pytest.raises(RuntimeError):
        recorder.start()
    recorder.cancel()


def test_open_wraps_device_errors(monkeypatch) -> None:
    class BrokenStream:
        def __init__(self, **kwargs):
            raise OSError("no microphone")

    class FakeSoundDevice:
        InputStream = BrokenStream

    monkeypatch.setattr("fasttts.recorder.load_sounddevice", lambda: FakeSoundDevice)
    recorder = Recorder()
    with pytest.raises(MicrophoneError, match="no microphone"):
        recorder.open()
    assert not recorder.streaming


def test_warmup_opens_the_stream(monkeypatch) -> None:
    recorder = Recorder()
    opened: list[int] = []
    monkeypatch.setattr(recorder, "open", lambda: opened.append(1))
    recorder.warmup()
    assert opened == [1]


def test_on_level_reports_only_while_capturing() -> None:
    levels: list[float] = []
    recorder = Recorder(sample_rate=1000, pre_roll_seconds=0.1, on_level=levels.append)
    recorder._stream = object()
    feed(recorder, 50)
    assert levels == []
    recorder.start()
    feed(recorder, 50, value=0.2)
    assert levels == [pytest.approx(0.2)]
    recorder.stop()
    feed(recorder, 50)
    assert len(levels) == 1
