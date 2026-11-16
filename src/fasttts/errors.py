"""FastTTS errors."""

from __future__ import annotations


class FastTTSError(Exception):
    """Base error."""


class ConfigError(FastTTSError):
    """The configuration file is invalid."""


class MicrophoneError(FastTTSError):
    """The microphone could not be opened or read."""


class TranscriptionError(FastTTSError):
    """Speech recognition failed or is unavailable."""
