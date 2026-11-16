"""English-only speech recognition with faster-whisper.

One model, one language, no router: ``small.en`` at int8 is fast on CPU and
produces punctuation. The model is loaded once and kept warm.
"""

from __future__ import annotations

import logging
import threading
from pathlib import Path

import numpy as np

from .cleanup import clean_transcript
from .config import Config, bundled_model_dir
from .errors import TranscriptionError

log = logging.getLogger(__name__)


class Transcriber:
    """Lazy faster-whisper wrapper fixed to English."""

    def __init__(self, config: Config) -> None:
        self.config = config
        self._model = None
        self._lock = threading.Lock()

    def model_ref(self) -> str:
        """Return the configured model path, or the bundled English model."""
        configured = self.config.model.strip()
        if configured:
            return configured
        bundled = bundled_model_dir()
        if not (bundled / "model.bin").is_file():
            raise TranscriptionError(
                f"The bundled English model is missing: {bundled / 'model.bin'}. "
                "Restore models/faster-whisper-small.en from the FastTTS checkout."
            )
        return str(bundled)

    def _load(self):
        from faster_whisper import WhisperModel

        model_ref = self.model_ref()
        local_only = Path(model_ref).is_dir()
        log.info(
            "Loading %s on %s (%s)",
            model_ref,
            self.config.device,
            self.config.compute_type,
        )
        try:
            return WhisperModel(
                model_ref,
                device=self.config.device,
                compute_type=self.config.compute_type,
                download_root=self.config.model_dir or None,
                cpu_threads=self.config.cpu_threads,
                local_files_only=local_only,
            )
        except Exception as exc:
            raise TranscriptionError(
                f"Could not load the English model '{model_ref}': {exc}. "
                "Check that the model folder is complete."
            ) from exc

    def load(self):
        """Load and return the model, once."""
        if self._model is not None:
            return self._model
        with self._lock:
            if self._model is None:
                self._model = self._load()
            return self._model

    @property
    def loaded(self) -> bool:
        return self._model is not None

    def warmup(self) -> None:
        """Load the model and run a tiny silent pass so kernels are ready."""
        model = self.load()
        try:
            segments, _ = model.transcribe(
                np.zeros(1600, dtype=np.float32),
                language="en",
                beam_size=1,
                vad_filter=self.config.vad,
                condition_on_previous_text=False,
                without_timestamps=True,
            )
            list(segments)
        except Exception as exc:
            raise TranscriptionError(f"Could not warm up the English model: {exc}") from exc

    def transcribe(self, audio: np.ndarray) -> str:
        """Transcribe one 16 kHz mono float32 take. Returns clean text."""
        if audio.size == 0:
            return ""
        model = self.load()
        try:
            segments, _ = model.transcribe(
                audio,
                language="en",
                beam_size=self.config.beam_size,
                vad_filter=self.config.vad,
                condition_on_previous_text=False,
                initial_prompt=self.config.initial_prompt or None,
                without_timestamps=True,
            )
            text = " ".join(segment.text.strip() for segment in segments).strip()
        except Exception as exc:
            raise TranscriptionError(f"Transcription failed: {exc}") from exc
        return clean_transcript(text) if self.config.cleanup else text

    def close(self) -> None:
        with self._lock:
            self._model = None
