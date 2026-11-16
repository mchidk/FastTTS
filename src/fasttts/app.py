"""FastTTS: hold Caps Lock to dictate English, locally."""

from __future__ import annotations

import contextlib
import ctypes
import logging
import os
import queue
import sys
import threading
import time
from logging.handlers import RotatingFileHandler
from typing import Any

from .config import Config, load_config, log_dir
from .errors import ConfigError, FastTTSError, MicrophoneError, TranscriptionError
from .hotkey import Action, CapsLockHook, HoldToTalk
from .hud import Hud
from .inject import inject_text
from .recorder import Recorder
from .stt import Transcriber

log = logging.getLogger(__name__)

COLORS = {
    "idle": (100, 116, 139),
    "recording": (255, 92, 108),
    "busy": (255, 180, 84),
    "paused": (71, 85, 105),
    "error": (220, 38, 38),
}

_MUTEX_NAME = "Local\\FastTTS-single-instance"
_mutex_handle = None


def _beep(kind: str, enabled: bool) -> None:
    """Play a short cue without blocking the worker or the microphone."""
    if not enabled or sys.platform != "win32":
        return
    import winsound

    frequency = {"stop": 660, "error": 420}.get(kind, 700)

    def play() -> None:
        with contextlib.suppress(RuntimeError):
            winsound.Beep(frequency, 45)

    threading.Thread(target=play, name="fasttts-cue", daemon=True).start()


def _icon_image(color: tuple[int, int, int]):
    from PIL import Image, ImageDraw

    image = Image.new("RGBA", (64, 64), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.ellipse([2, 2, 62, 62], fill=(15, 18, 25, 255), outline=(*color, 255), width=3)
    draw.rounded_rectangle([26, 15, 38, 38], radius=6, fill=(*color, 255))
    draw.arc([20, 23, 44, 47], start=0, end=180, fill=(*color, 255), width=3)
    draw.line([32, 47, 32, 53], fill=(*color, 255), width=3)
    draw.line([26, 53, 38, 53], fill=(*color, 255), width=3)
    return image


class DictateApp:
    """Owns the Caps Lock hook, the microphone and the English model."""

    def __init__(self, config: Config) -> None:
        self.config = config
        self.transcriber = Transcriber(config)
        self.hud = Hud(enabled=config.hud)
        self.recorder = Recorder(
            sample_rate=16000,
            device=config.mic_device if config.mic_device != "" else None,
            max_seconds=config.max_record_seconds,
            on_level=self.hud.push_level,
        )
        self.machine = HoldToTalk()
        self.hook: CapsLockHook | None = None
        self.icon = None
        self._queue: queue.Queue[str] = queue.Queue()
        self._jobs: queue.Queue[Any] = queue.Queue()
        self._worker: threading.Thread | None = None
        self._transcribe_worker: threading.Thread | None = None
        self._pending = 0
        self._pending_lock = threading.Lock()
        self._paused = False
        self._hook_failed = False
        self._stopping = threading.Event()
        self._state = "idle"
        self._status = "Starting..."
        self._recording_started_at = 0.0
        self._error_until = 0.0
        self._model_ready = False
        self._model_failed = False

    # -- lifecycle ----------------------------------------------------------

    def run(self) -> int:
        try:
            import pystray
        except ImportError:
            print(
                "FastTTS needs pystray and Pillow. Install them with: pip install pystray pillow",
                file=sys.stderr,
            )
            return 1

        self._transcribe_worker = threading.Thread(
            target=self._transcribe_work, name="fasttts-transcribe", daemon=True
        )
        self._transcribe_worker.start()
        self._worker = threading.Thread(target=self._work, name="fasttts-worker", daemon=True)
        self._worker.start()

        menu = pystray.Menu(
            pystray.MenuItem(lambda item: self._status, lambda icon, item: None, enabled=False),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem(
                lambda item: "Resume dictation" if self._paused else "Pause dictation",
                self._menu_pause,
            ),
            pystray.MenuItem("Open log", self._menu_logs),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem("Quit", self._menu_quit),
        )
        self.icon = pystray.Icon(
            "FastTTS",
            _icon_image(COLORS["idle"]),
            "FastTTS - hold Caps Lock to dictate",
            menu,
        )
        self.icon.run(setup=self._setup)
        return 0

    def _setup(self, icon) -> None:
        icon.visible = True
        self._set_status("Loading English model...")
        self._refresh_icon()
        self.hud.start()
        try:
            self.hook = CapsLockHook(
                self._on_caps_down,
                self._on_caps_up,
                should_handle=lambda: not self._paused,
            )
            self.hook.start()
        except Exception as exc:
            self._hook_failed = True
            log.exception("Could not start the Caps Lock hook")
            self._fail(f"Caps Lock dictation unavailable: {exc}", "HOOK ERROR")

        if self.config.hot_mic:
            try:
                self.recorder.warmup()
            except FastTTSError as exc:
                log.warning("Microphone warmup failed: %s", exc)
                self._notify(str(exc))

        threading.Thread(target=self._warm_models, name="fasttts-warmup", daemon=True).start()
        log.info("FastTTS tray app ready")

    def _warm_models(self) -> None:
        try:
            self.transcriber.warmup()
            log.info("English model ready")
            self._model_ready = True
            self._model_failed = False
            if not self._paused and self._pending_count() == 0:
                self._error_until = 0.0
                self._settle()
        except FastTTSError as exc:
            log.warning("Model warmup failed: %s", exc)
            self._model_failed = True
            self._fail(str(exc), "MODEL ERROR")
        except Exception:
            log.exception("Model warmup failed")
            self._model_failed = True
            self._fail("Could not load the English model", "MODEL ERROR")

    def _shutdown(self) -> None:
        if self._stopping.is_set():
            return
        self._stopping.set()
        if self.hook is not None:
            self.hook.stop()
        self._queue.put("quit")
        self._jobs.put(None)
        if self._worker is not None:
            self._worker.join(timeout=1.0)
        self.recorder.cancel()
        self.recorder.close()
        self.hud.stop()
        if self._transcribe_worker is not None:
            self._transcribe_worker.join(timeout=2.0)
        self.transcriber.close()
        if self.icon is not None:
            self.icon.stop()

    # -- menu --------------------------------------------------------------

    def _menu_pause(self, icon, item) -> None:
        self._paused = not self._paused
        if self._paused:
            self._queue.put("cancel")
            self.hud.hide()
            self._set_status("Paused - Caps Lock works normally")
            self._state = "paused"
        else:
            self._set_status("Ready - hold Caps Lock to dictate")
            self._error_until = 0.0
            if self.config.hot_mic:
                try:
                    self.recorder.warmup()
                except FastTTSError as exc:
                    self._notify(str(exc))
            self._settle()
        self._refresh_icon()
        self._refresh_menu()

    def _menu_logs(self, icon, item) -> None:
        _open_path(log_dir())

    def _menu_quit(self, icon, item) -> None:
        self._shutdown()

    # -- hook callbacks ----------------------------------------------------

    def _on_caps_down(self) -> None:
        if self._paused:
            return
        if self.machine.press() is Action.START:
            self._queue.put("start")

    def _on_caps_up(self) -> None:
        if self.machine.release() is Action.STOP:
            self._queue.put("stop")

    # -- worker ------------------------------------------------------------

    def _work(self) -> None:
        while not self._stopping.is_set():
            try:
                command = self._queue.get(timeout=0.2)
            except queue.Empty:
                continue
            try:
                if command == "start":
                    self._start_recording()
                elif command == "stop":
                    self._stop_recording()
                elif command == "cancel":
                    self._cancel_recording()
                elif command == "quit":
                    break
            except Exception:
                log.exception("FastTTS worker failed on '%s'", command)
                self.machine.cancel()
                self.recorder.cancel()
                self._fail("Internal error - see the log", "INTERNAL ERROR")

    def _start_recording(self) -> None:
        if self.recorder.active or self._paused:
            return
        try:
            self.recorder.start()
        except MicrophoneError as exc:
            self.machine.cancel()
            self._fail(str(exc), "MICROPHONE ERROR")
            return
        self._state = "recording"
        self._recording_started_at = time.monotonic()
        self._set_status("Listening...")
        self.hud.set_mode("recording")
        self._refresh_icon()
        log.info("Recording started")

    def _stop_recording(self) -> None:
        if not self.recorder.active:
            self.machine.cancel()
            self._settle()
            return
        audio = self.recorder.stop()
        held = time.monotonic() - self._recording_started_at
        log.info("Recording stopped: %.2fs", len(audio) / self.recorder.sample_rate)
        if held < self.config.min_hold_ms / 1000.0 or audio.size == 0:
            # A stray touch, not speech: discard without a cue or transcript.
            self.hud.hide()
            self._settle()
            return
        _beep("stop", self.config.sounds)
        self._enqueue_transcription(audio)
        self._settle()

    def _transcribe_work(self) -> None:
        """Transcribe takes in order, away from the key-handling worker."""
        while not self._stopping.is_set():
            job = self._jobs.get()
            if job is None:
                break
            try:
                text = self.transcriber.transcribe(job).strip()
                self._model_ready = True
                self._model_failed = False
                if not text:
                    self._fail("No speech recognized", "NO SPEECH")
                elif self.config.inject == "none":
                    log.info("Dictation (inject=none): %s", text)
                    self._set_status(f"Recognized {len(text)} characters (not inserted)")
                else:
                    self._insert(text)
            except TranscriptionError as exc:
                self._fail(str(exc), "TRANSCRIPTION ERROR")
            except Exception:
                log.exception("Transcription worker failed")
                self._fail("Transcription failed", "TRANSCRIPTION ERROR")
            finally:
                with self._pending_lock:
                    self._pending = max(0, self._pending - 1)
                self._settle()

    def _insert(self, text: str) -> None:
        try:
            inject_text(
                text,
                method=self.config.inject,
                delay_ms=self.config.char_delay_ms,
                append_space=self.config.append_space,
                paste_delay_ms=self.config.paste_delay_ms,
            )
        except OSError as exc:
            if self.config.inject != "sendinput":
                self._fail(f"Could not insert the transcript: {exc}", "INPUT BLOCKED")
                return
            log.warning("SendInput was blocked (%s); trying the clipboard", exc)
            try:
                inject_text(
                    text,
                    method="clipboard",
                    append_space=self.config.append_space,
                    paste_delay_ms=self.config.paste_delay_ms,
                )
            except Exception as fallback:
                self._fail(f"Could not insert the transcript: {fallback}", "INPUT BLOCKED")
                return
            self._set_status(f"Inserted {len(text)} characters (clipboard)")
            return
        except Exception as exc:
            self._fail(f"Could not insert the transcript: {exc}", "INPUT ERROR")
            return
        self._set_status(f"Inserted {len(text)} characters")

    def _cancel_recording(self) -> None:
        self.recorder.cancel()
        self.machine.cancel()
        self._settle()

    # -- small helpers -----------------------------------------------------

    def _pending_count(self) -> int:
        with self._pending_lock:
            return self._pending

    def _enqueue_transcription(self, audio) -> None:
        with self._pending_lock:
            self._pending += 1
        self._jobs.put(audio)
        self._state = "busy"
        self._set_status("Transcribing...")
        self.hud.set_mode("transcribing")
        self._refresh_icon()

    def _settle(self) -> None:
        """Recompute the tray state from the real work in flight."""
        if self._hook_failed:
            self._state = "error"
            self._set_status("Caps Lock hook unavailable")
            self._refresh_icon()
            return
        if self._paused and not self.recorder.active and not self._pending_count():
            self._state = "paused"
            self._set_status("Paused - Caps Lock works normally")
        elif self.recorder.active:
            self._state = "recording"
            self._set_status("Listening...")
        elif self._pending_count():
            self._state = "busy"
            self._set_status("Transcribing...")
        elif self._model_failed and not self._model_ready:
            self._state = "error"
            self._set_status("Model unavailable - see the log")
        elif not self._model_ready:
            self._state = "busy"
            self._set_status("Loading English model...")
        elif time.monotonic() < self._error_until:
            self._state = "error"
        else:
            self._state = "idle"
            self._set_status("Ready - hold Caps Lock to dictate")
        if self._state == "recording":
            self.hud.set_mode("recording")
        elif self._state == "busy":
            self.hud.set_mode("transcribing")
        elif self._state != "error":
            self.hud.hide()
        self._refresh_icon()

    def _fail(self, message: str, short: str) -> None:
        log.warning("%s", message)
        self._error_until = time.monotonic() + 2.4
        self._state = "error"
        self._notify(message)
        self._set_status(short.title())
        self.hud.set_mode("error", short)
        self._refresh_icon()
        _beep("error", self.config.sounds)
        timer = threading.Timer(2.5, self._settle)
        timer.daemon = True
        timer.start()

    def _set_status(self, status: str) -> None:
        self._status = status
        self._refresh_menu()

    def _refresh_menu(self) -> None:
        if self.icon is not None:
            with contextlib.suppress(Exception):
                self.icon.update_menu()

    def _refresh_icon(self) -> None:
        if self.icon is None:
            return
        color = COLORS.get(self._state, COLORS["idle"])
        with contextlib.suppress(Exception):
            self.icon.icon = _icon_image(color)
            self.icon.title = f"FastTTS - {self._status}"

    def _notify(self, message: str) -> None:
        if self.icon is None:
            return
        try:
            self.icon.notify(message[:200], "FastTTS")
        except Exception:
            log.debug("Tray notification failed", exc_info=True)


def _open_path(path) -> None:
    if sys.platform == "win32":
        os.startfile(str(path))
    else:
        import subprocess

        subprocess.run(["xdg-open", str(path)], check=False)


def _single_instance() -> bool:
    """Return False when another FastTTS app instance owns the mutex."""
    global _mutex_handle
    if sys.platform != "win32":
        return True
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_wchar_p]
    kernel32.CreateMutexW.restype = ctypes.c_void_p
    _mutex_handle = kernel32.CreateMutexW(None, False, _MUTEX_NAME)
    return ctypes.get_last_error() != 183  # ERROR_ALREADY_EXISTS


def setup_logging() -> None:
    """Rotate logs under %LOCALAPPDATA%\\FastTTS\\logs."""
    directory = log_dir()
    directory.mkdir(parents=True, exist_ok=True)
    handler = RotatingFileHandler(
        directory / "fasttts.log", maxBytes=1_000_000, backupCount=3, encoding="utf-8"
    )
    handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s"))
    root = logging.getLogger()
    root.setLevel(logging.INFO)
    root.addHandler(handler)
    for noisy in ("httpx", "httpcore", "huggingface_hub", "faster_whisper"):
        logging.getLogger(noisy).setLevel(logging.WARNING)
    if sys.stderr is not None:
        stream = logging.StreamHandler()
        stream.setFormatter(logging.Formatter("fasttts: %(message)s"))
        root.addHandler(stream)


def run_app(config: Config | None = None) -> int:
    """Entry point shared by the GUI script and ``python -m fasttts``."""
    if sys.platform != "win32":
        print("FastTTS needs Windows for Caps Lock dictation", file=sys.stderr)
        return 1
    setup_logging()
    if not _single_instance():
        ctypes.windll.user32.MessageBoxW(
            0,
            "FastTTS is already running. Look for its microphone icon in the system tray.",
            "FastTTS",
            0x40,
        )
        return 0
    try:
        app = DictateApp(config or load_config())
        return app.run()
    except Exception as exc:
        log.exception("FastTTS failed to start")
        ctypes.windll.user32.MessageBoxW(0, f"FastTTS could not start:\n\n{exc}", "FastTTS", 0x10)
        return 1


def main() -> int:
    try:
        config = load_config()
    except ConfigError as exc:
        ctypes.windll.user32.MessageBoxW(
            0, f"FastTTS configuration error:\n\n{exc}", "FastTTS", 0x10
        )
        return 1
    return run_app(config)


if __name__ == "__main__":
    raise SystemExit(main())
