"""Offline API tests for the oscilloscope HUD (no window is created)."""

from __future__ import annotations

import pytest

from fasttts.hud import Hud


def test_disabled_hud_is_a_no_op() -> None:
    hud = Hud(enabled=False)
    hud.start()
    hud.push_level(0.4)
    hud.set_mode("recording")
    hud.hide()
    hud.stop()
    assert not hud.enabled


def test_set_mode_validates_names() -> None:
    hud = Hud(enabled=False)
    with pytest.raises(ValueError, match="Unknown HUD mode"):
        hud.set_mode("speaking")
    hud.set_mode("recording")
    hud.set_mode("transcribing")
    hud.set_mode("error", "NO SPEECH")
    hud.set_mode("hidden")


def test_push_level_only_keeps_recording_levels() -> None:
    hud = Hud(enabled=False)
    hud.set_mode("transcribing")
    hud.push_level(0.5)
    assert not hud._levels
    hud.set_mode("recording")
    hud.push_level(0.5)
    hud.push_level(-1)
    assert list(hud._levels) == [0.5, 0.0]


def test_bar_heights_are_smoothed() -> None:
    hud = Hud(enabled=False)
    heights = hud._bar_heights("recording", [0.2] * 8, 38, 15, 1.0)
    assert len(heights) == 38
    assert all(value >= 2.0 for value in heights)
    assert all(value <= 20.0 for value in heights)
