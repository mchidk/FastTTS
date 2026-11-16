"""Offline tests for configuration loading and validation."""

from __future__ import annotations

import pytest

from fasttts.config import Config, default_config_path, load_config
from fasttts.errors import ConfigError


def test_defaults_are_english_and_sane() -> None:
    config = Config()
    config.validate()
    assert config.model == ""  # "" means the bundled model
    assert config.inject == "clipboard"
    assert config.char_delay_ms == 10
    assert config.paste_delay_ms == 400
    assert config.cleanup is True
    assert config.hot_mic is True
    assert config.hud is True
    assert config.sounds is False
    assert config.min_hold_ms == 250


def test_missing_file_returns_defaults(tmp_path) -> None:
    path = tmp_path / "missing.toml"
    config = load_config(path)
    assert config.path == path
    assert config.model == ""


def test_toml_values_are_loaded(tmp_path) -> None:
    path = tmp_path / "config.toml"
    path.write_text(
        'model = "tiny.en"\nmic_device = 2\ncleanup = false\ninitial_prompt = "OpenCode"\n',
        encoding="utf-8",
    )
    config = load_config(path)
    assert config.model == "tiny.en"
    assert config.mic_device == 2
    assert config.cleanup is False
    assert config.initial_prompt == "OpenCode"


def test_unknown_key_is_rejected(tmp_path) -> None:
    path = tmp_path / "config.toml"
    path.write_text("nonsense = 1\n", encoding="utf-8")
    with pytest.raises(ConfigError, match="nonsense"):
        load_config(path)


def test_invalid_value_is_rejected(tmp_path) -> None:
    path = tmp_path / "config.toml"
    path.write_text('inject = "telepathy"\n', encoding="utf-8")
    with pytest.raises(ConfigError, match="inject"):
        load_config(path)
    path.write_text("min_hold_ms = -1\n", encoding="utf-8")
    with pytest.raises(ConfigError, match="min_hold_ms"):
        load_config(path)


def test_environment_overrides(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("FASTTTS_MIC_DEVICE", "3")
    monkeypatch.setenv("FASTTTS_CLEANUP", "off")
    monkeypatch.setenv("FASTTTS_INITIAL_PROMPT", "FastTTS")
    config = load_config(tmp_path / "missing.toml")
    assert config.mic_device == 3
    assert config.cleanup is False
    assert config.initial_prompt == "FastTTS"


def test_invalid_environment_value(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("FASTTTS_SOUNDS", "sometimes")
    with pytest.raises(ConfigError, match="sounds"):
        load_config(tmp_path / "missing.toml")


def test_config_path_can_be_overridden(monkeypatch, tmp_path) -> None:
    target = tmp_path / "custom.toml"
    monkeypatch.setenv("FASTTTS_CONFIG", str(target))
    assert default_config_path() == target
