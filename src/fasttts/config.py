"""Minimal configuration: one TOML file, flat keys, readable defaults."""

from __future__ import annotations

import os
import sys
import tomllib
from dataclasses import dataclass, field, fields
from pathlib import Path
from typing import Any

from .errors import ConfigError

APP_NAME = "FastTTS"


def project_root() -> Path:
    """Return the FastTTS folder that contains ``src`` and ``models``."""
    if override := os.environ.get("FASTTTS_HOME"):
        return Path(override).expanduser()
    cwd = Path.cwd()
    if (cwd / "models" / "faster-whisper-small.en").is_dir():
        return cwd
    return Path(__file__).resolve().parents[2]


def bundled_model_dir() -> Path:
    """Return the bundled English model folder."""
    return project_root() / "models" / "faster-whisper-small.en"


def config_dir() -> Path:
    """Return the per-user configuration directory."""
    if override := os.environ.get("FASTTTS_CONFIG_DIR"):
        return Path(override).expanduser()
    if sys.platform == "win32":
        root = os.environ.get("APPDATA")
        if root:
            return Path(root) / APP_NAME
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "fasttts"


def data_dir() -> Path:
    """Return the per-user data directory."""
    if override := os.environ.get("FASTTTS_DATA_DIR"):
        return Path(override).expanduser()
    if sys.platform == "win32":
        root = os.environ.get("LOCALAPPDATA") or os.environ.get("APPDATA")
        if root:
            return Path(root) / APP_NAME
    return Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share")) / "fasttts"


def log_dir() -> Path:
    """Return the log directory."""
    return data_dir() / "logs"


def default_config_path() -> Path:
    """Return the active configuration file path."""
    if override := os.environ.get("FASTTTS_CONFIG"):
        return Path(override).expanduser()
    return config_dir() / "config.toml"


@dataclass
class Config:
    """Everything FastTTS lets you tune. English and Caps Lock are fixed."""

    # Speech recognition. An empty model uses the bundled English model;
    # set a path or a Hugging Face id to override it.
    model: str = ""
    device: str = "cpu"
    compute_type: str = "int8"
    cpu_threads: int = 0
    beam_size: int = 1
    vad: bool = True
    initial_prompt: str = ""
    cleanup: bool = True
    model_dir: str = ""

    # Microphone and hold-to-talk behaviour.
    mic_device: int | str = ""
    min_hold_ms: int = 250
    max_record_seconds: float = 120.0
    hot_mic: bool = True
    sounds: bool = True
    hud: bool = True

    # Text insertion.
    inject: str = "sendinput"
    char_delay_ms: int = 1
    paste_delay_ms: int = 180
    append_space: bool = True

    path: Path | None = field(default=None, compare=False, repr=False)

    def validate(self) -> None:
        if self.device not in {"cpu", "cuda", "auto"}:
            raise ConfigError("device must be cpu, cuda or auto")
        if not self.compute_type.strip():
            raise ConfigError("compute_type must not be empty")
        if not 0 <= self.cpu_threads <= 64:
            raise ConfigError("cpu_threads must be between 0 and 64")
        if self.beam_size < 1:
            raise ConfigError("beam_size must be at least 1")
        if not 0 <= self.min_hold_ms <= 5000:
            raise ConfigError("min_hold_ms must be between 0 and 5000")
        if self.max_record_seconds <= 0:
            raise ConfigError("max_record_seconds must be positive")
        if self.inject not in {"sendinput", "clipboard", "none"}:
            raise ConfigError("inject must be sendinput, clipboard or none")
        if not 0 <= self.char_delay_ms <= 100:
            raise ConfigError("char_delay_ms must be between 0 and 100")
        if not 0 <= self.paste_delay_ms <= 5000:
            raise ConfigError("paste_delay_ms must be between 0 and 5000")
        if isinstance(self.mic_device, int) and self.mic_device < 0:
            raise ConfigError("mic_device must be a device index or a name")


def load_config(path: str | Path | None = None) -> Config:
    """Load configuration. A missing file means defaults."""
    config_path = Path(path).expanduser() if path else default_config_path()
    data: dict[str, Any] = {}
    if config_path.is_file():
        try:
            with config_path.open("rb") as handle:
                data = tomllib.load(handle)
        except tomllib.TOMLDecodeError as exc:
            raise ConfigError(f"{config_path}: {exc}") from exc

    allowed = {item.name for item in fields(Config) if item.name != "path"}
    unknown = sorted(set(data) - allowed)
    if unknown:
        raise ConfigError(f"Unknown option(s) in {config_path}: {', '.join(unknown)}")

    try:
        config = Config(**data, path=config_path)
    except TypeError as exc:
        raise ConfigError(f"Invalid configuration in {config_path}: {exc}") from exc

    for item in fields(Config):
        if item.name == "path":
            continue
        env = os.environ.get(f"FASTTTS_{item.name.upper()}")
        if env is not None:
            setattr(config, item.name, _coerce_env(env, getattr(config, item.name), item.name))

    config.validate()
    return config


def _coerce_env(value: str, current: Any, name: str) -> Any:
    if name == "mic_device":
        text = value.strip()
        return int(text) if text.isdigit() else text
    if isinstance(current, bool):
        lowered = value.strip().lower()
        if lowered in {"1", "true", "yes", "on"}:
            return True
        if lowered in {"0", "false", "no", "off"}:
            return False
        raise ConfigError(f"{name} must be a boolean, got '{value}'")
    if isinstance(current, int):
        try:
            return int(value)
        except ValueError:
            raise ConfigError(f"{name} must be an integer, got '{value}'") from None
    if isinstance(current, float):
        try:
            return float(value)
        except ValueError:
            raise ConfigError(f"{name} must be a number, got '{value}'") from None
    return value
