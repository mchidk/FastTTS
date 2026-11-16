"""Offline tests for the Caps Lock hold-to-talk state machine."""

from __future__ import annotations

from fasttts.hotkey import Action, HoldToTalk


def test_press_starts_and_release_stops() -> None:
    machine = HoldToTalk()
    assert machine.press(now=10.0) is Action.START
    assert machine.recording
    assert machine.release(now=10.2) is Action.STOP
    assert not machine.recording


def test_auto_repeat_is_ignored() -> None:
    machine = HoldToTalk()
    assert machine.press(now=1.0) is Action.START
    assert machine.press(now=1.1) is Action.NONE
    assert machine.release(now=1.2) is Action.STOP


def test_release_without_press_is_ignored() -> None:
    assert HoldToTalk().release() is Action.NONE


def test_new_press_starts_a_new_take() -> None:
    machine = HoldToTalk()
    machine.press(now=1.0)
    machine.release(now=1.2)
    assert machine.press(now=2.0) is Action.START
    assert machine.release(now=2.4) is Action.STOP


def test_cancel_resets_both_states() -> None:
    machine = HoldToTalk()
    machine.press(now=1.0)
    machine.cancel()
    assert not machine.recording
    assert machine.release(now=1.1) is Action.NONE
    assert machine.press(now=2.0) is Action.START


def test_pressed_at_is_tracked() -> None:
    machine = HoldToTalk()
    machine.press(now=5.0)
    assert machine.pressed_at == 5.0
    machine.release(now=5.3)
    assert machine.pressed_at is None
