"""Persistent 16 kHz mono microphone capture with a small pre-roll."""

from __future__ import annotations

import contextlib
import logging
import threading
from collections import deque
from collections.abc import Callable

import numpy as np

from .errors import MicrophoneError

log = logging.getLogger(__name__)


def load_sounddevice():
    try:
        import sounddevice as sd
    except (ImportError, OSError) as exc:
        raise MicrophoneError(
            "Microphone capture needs sounddevice and PortAudio. "
            "Install it with: pip install sounddevice"
        ) from exc
    return sd


class Recorder:
    """Keep the input stream open so a take starts instantly.

    The stream runs continuously and keeps ``pre_roll_seconds`` of audio in a
    ring buffer. ``start()`` promotes that buffer into the take, so the first
    word is not lost to PortAudio startup. ``stop()`` returns the take and
    leaves the stream open; ``close()`` releases it.
    """

    def __init__(
        self,
        *,
        sample_rate: int = 16000,
        channels: int = 1,
        device: int | str | None = None,
        max_seconds: float = 120.0,
        pre_roll_seconds: float = 0.25,
        on_level: Callable[[float], None] | None = None,
    ) -> None:
        self.sample_rate = sample_rate
        self.channels = channels
        self.device = device
        self.max_seconds = max_seconds
        self.pre_roll_seconds = max(0.0, pre_roll_seconds)
        self.on_level = on_level
        self._stream = None
        self._chunks: list[np.ndarray] = []
        self._ring: deque[np.ndarray] = deque()
        self._ring_samples = 0
        self._captured_samples = 0
        self._capturing = False
        self._lock = threading.Lock()

    @property
    def active(self) -> bool:
        return self._capturing

    @property
    def streaming(self) -> bool:
        return self._stream is not None

    def _callback(self, indata, frames, time_info, status) -> None:
        if status:
            log.warning("Microphone status: %s", status)
        chunk = indata[:, 0].copy()
        capturing = False
        with self._lock:
            if self.pre_roll_seconds:
                self._ring.append(chunk)
                self._ring_samples += len(chunk)
                limit = int(self.pre_roll_seconds * self.sample_rate)
                while self._ring_samples > limit and self._ring:
                    removed = self._ring.popleft()
                    self._ring_samples -= len(removed)
            if self._capturing:
                self._chunks.append(chunk)
                self._captured_samples += len(chunk)
                capturing = True
        if capturing and self.on_level is not None:
            self.on_level(float(np.sqrt(np.mean(chunk**2))))

    def open(self) -> None:
        """Open the stream without starting a take."""
        if self._stream is not None:
            return
        sd = load_sounddevice()
        try:
            self._stream = sd.InputStream(
                samplerate=self.sample_rate,
                channels=self.channels,
                dtype="float32",
                device=self.device,
                callback=self._callback,
                blocksize=0,
            )
            self._stream.start()
        except Exception as exc:
            if self._stream is not None:
                with contextlib.suppress(Exception):
                    self._stream.close()
            self._stream = None
            raise MicrophoneError(f"Could not open the microphone: {exc}") from exc
        log.info("Microphone stream open")

    def close(self) -> None:
        stream, self._stream = self._stream, None
        if stream is not None:
            with contextlib.suppress(Exception):
                stream.stop()
            with contextlib.suppress(Exception):
                stream.close()
        with self._lock:
            self._ring.clear()
            self._ring_samples = 0

    def start(self) -> None:
        if self._capturing:
            raise RuntimeError("Recorder is already capturing")
        self.open()
        with self._lock:
            self._chunks = list(self._ring) if self.pre_roll_seconds else []
            self._ring.clear()
            self._ring_samples = 0
            self._captured_samples = sum(len(chunk) for chunk in self._chunks)
            self._capturing = True

    def stop(self) -> np.ndarray:
        with self._lock:
            self._capturing = False
            chunks, self._chunks = self._chunks, []
            self._captured_samples = 0
            self._ring.clear()
            self._ring_samples = 0
        if not chunks:
            return np.zeros(0, dtype=np.float32)
        audio = np.concatenate(chunks).astype(np.float32, copy=False)
        limit = int(self.max_seconds * self.sample_rate)
        return audio[:limit]

    def cancel(self) -> None:
        with self._lock:
            self._capturing = False
            self._chunks = []
            self._captured_samples = 0
            self._ring.clear()
            self._ring_samples = 0

    def warmup(self) -> None:
        """Open the microphone ahead of the first take."""
        self.open()
