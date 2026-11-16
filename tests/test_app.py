"""Offline tests for tray app state and dictation flow (no tray, mic or model)."""

from __future__ import annotations

import time

import numpy as np

from fasttts import app as app_module
from fasttts.app import DictateApp
from fasttts.config import Config


class FakeRecorder:
    sample_rate = 16000

    def __init__(self, audio: np.ndarray | None = None) -> None:
        self.active = False
        self.audio = np.zeros(0, dtype=np.float32) if audio is None else audio
        self.started = 0
        self.cancelled = 0
        self.closed = 0

    def start(self) -> None:
        self.active = True
        self.started += 1

    def stop(self) -> np.ndarray:
        self.active = False
        return self.audio

    def cancel(self) -> None:
        self.active = False
        self.cancelled += 1

    def warmup(self) -> None:
        pass

    def close(self) -> None:
        self.closed += 1


class FakeIcon:
    def __init__(self) -> None:
        self.icon = None
        self.title = ""
        self.visible = False
        self.notifications: list[str] = []
        self.updates = 0
        self.stopped = 0

    def notify(self, message: str, title: str) -> None:
        self.notifications.append(message)

    def update_menu(self) -> None:
        self.updates += 1

    def stop(self) -> None:
        self.stopped += 1


class FakeHud:
    def __init__(self) -> None:
        self.modes: list[tuple[str, str]] = []
        self.levels: list[float] = []

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def set_mode(self, mode: str, detail: str = "") -> None:
        self.modes.append((mode, detail))

    def hide(self) -> None:
        self.modes.append(("hidden", ""))

    def push_level(self, level: float) -> None:
        self.levels.append(level)


class FakeTranscriber:
    def __init__(self, text: str = "hello there", error: Exception | None = None) -> None:
        self.text = text
        self.error = error
        self.calls = 0
        self.closed = 0
        self.warmed = 0

    def transcribe(self, audio) -> str:
        self.calls += 1
        if self.error is not None:
            raise self.error
        return self.text

    def warmup(self) -> None:
        self.warmed += 1
        if self.error is not None:
            raise self.error

    def close(self) -> None:
        self.closed += 1


class FakeTimer:
    def __init__(self, interval, function) -> None:
        self.interval = interval
        self.function = function
        self.daemon = False
        self.started = False

    def start(self) -> None:
        self.started = True


def make_app(monkeypatch, **config_kwargs) -> DictateApp:
    monkeypatch.setattr(app_module, "_beep", lambda kind, enabled: None)
    config = Config(hot_mic=False, **config_kwargs)
    app = DictateApp(config)
    app.recorder = FakeRecorder()
    app.hud = FakeHud()
    app.icon = FakeIcon()
    return app


def test_caps_press_and_release_queue_commands(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app._on_caps_down()
    app._on_caps_up()
    assert app._queue.get_nowait() == "start"
    assert app._queue.get_nowait() == "stop"


def test_short_tap_is_discarded_without_a_transcript(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app.recorder = FakeRecorder(np.zeros(80, dtype=np.float32))
    app._start_recording()
    app._stop_recording()
    assert app._pending_count() == 0
    assert app._jobs.empty()
    assert app.recorder.cancelled == 0


def test_accepted_take_is_queued(monkeypatch) -> None:
    app = make_app(monkeypatch)
    audio = np.full(8000, 0.1, dtype=np.float32)
    app.recorder = FakeRecorder(audio)
    app._start_recording()
    app._recording_started_at = time.monotonic() - 0.5
    app._stop_recording()
    assert app._pending_count() == 1
    assert app._jobs.get_nowait() is audio
    assert app._state == "busy"
    assert ("recording", "") in app.hud.modes
    assert ("transcribing", "") in app.hud.modes


def test_idle_settle_hides_the_hud(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app._model_ready = True
    app._settle()
    assert app.hud.modes[-1] == ("hidden", "")


def test_failure_shows_the_hud_error(monkeypatch) -> None:
    app = make_app(monkeypatch)
    monkeypatch.setattr(app_module.threading, "Timer", FakeTimer)
    app._fail("No speech recognized", "NO SPEECH")
    assert ("error", "NO SPEECH") in app.hud.modes


def test_start_requires_a_minimum_hold(monkeypatch) -> None:
    app = make_app(monkeypatch, min_hold_ms=500)
    app.recorder = FakeRecorder(np.ones(8000, dtype=np.float32))
    app._recording_started_at = time.monotonic() - 0.1
    app.recorder.active = True
    app._stop_recording()
    assert app._pending_count() == 0


def test_transcription_inserts_text(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app.transcriber = FakeTranscriber("hello there")
    calls: list[tuple[str, str]] = []
    monkeypatch.setattr(
        app_module, "inject_text", lambda text, **kwargs: calls.append((text, kwargs["method"]))
    )
    app._enqueue_transcription(np.ones(8000, dtype=np.float32))
    app._jobs.put(None)
    app._transcribe_work()
    assert calls == [("hello there", "sendinput")]
    assert app._pending_count() == 0
    assert app._model_ready is True


def test_empty_transcript_reports_no_speech(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app.transcriber = FakeTranscriber("   ")
    app._enqueue_transcription(np.ones(8000, dtype=np.float32))
    app._jobs.put(None)
    app._transcribe_work()
    assert app._status == "No Speech"
    assert app.icon.notifications


def test_sendinput_blocked_falls_back_to_clipboard(monkeypatch) -> None:
    app = make_app(monkeypatch)
    methods: list[str] = []

    def fake_inject(text, **kwargs):
        methods.append(kwargs["method"])
        if kwargs["method"] == "sendinput":
            raise OSError("blocked")

    monkeypatch.setattr(app_module, "inject_text", fake_inject)
    app._insert("hello")
    assert methods == ["sendinput", "clipboard"]
    assert "clipboard" in app._status


def test_pause_uses_caps_lock_normally(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app._menu_pause(None, None)
    assert app._paused
    assert app._queue.get_nowait() == "cancel"
    app._on_caps_down()
    assert app._queue.empty()
    app._menu_pause(None, None)
    assert not app._paused


def test_settle_reports_model_states(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app._settle()
    assert app._status == "Loading English model..."
    app._model_failed = True
    app._settle()
    assert app._status == "Model unavailable - see the log"
    app._model_failed = False
    app._model_ready = True
    app._settle()
    assert app._status == "Ready - hold Caps Lock to dictate"
    app._menu_pause(None, None)
    assert app._status == "Paused - Caps Lock works normally"


def test_warm_models_sets_ready(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app.transcriber = FakeTranscriber()
    app._warm_models()
    assert app._model_ready
    assert app._status == "Ready - hold Caps Lock to dictate"


def test_warm_models_failure_is_reported(monkeypatch) -> None:
    from fasttts.errors import TranscriptionError

    app = make_app(monkeypatch)
    app.transcriber = FakeTranscriber(error=TranscriptionError("no model"))
    monkeypatch.setattr(app_module.threading, "Timer", FakeTimer)
    app._warm_models()
    assert app._model_failed
    assert app._status == "Model Error"
    assert app.icon.notifications


def test_shutdown_stops_everything(monkeypatch) -> None:
    app = make_app(monkeypatch)
    app._shutdown()
    assert app._stopping.is_set()
    assert app.recorder.cancelled == 1
    assert app.recorder.closed == 1
    assert app.icon.stopped == 1
