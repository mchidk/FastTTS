"""Offline tests for Unicode key event generation and injection flow."""

from __future__ import annotations

import pytest

from fasttts import inject


def test_unicode_events_are_a_down_and_up_pair() -> None:
    events = inject.unicode_events("a")
    assert [event.flags for event in events] == [
        inject.KEYEVENTF_UNICODE,
        inject.KEYEVENTF_UNICODE | inject.KEYEVENTF_KEYUP,
    ]
    assert [event.scan for event in events] == [ord("a"), ord("a")]


def test_bmp_character_uses_one_unit() -> None:
    assert len(inject.unicode_events("é")) == 2


def test_astral_character_uses_surrogate_pair() -> None:
    assert len(inject.unicode_events("😀")) == 4


def test_newline_and_tab_use_virtual_keys() -> None:
    events = inject.text_to_events("\n\t")
    assert len(events) == 4
    assert events[0] == inject.KeyEvent(vk=inject.VK_RETURN)
    assert events[2] == inject.KeyEvent(vk=inject.VK_TAB)


def test_sendinput_appends_space(monkeypatch) -> None:
    sent: list[inject.KeyEvent] = []
    monkeypatch.setattr(inject, "send_events", lambda events: sent.extend(events))
    inject.inject_text("hello", delay_ms=0, append_space=True)
    assert [event.scan for event in sent if event.flags & inject.KEYEVENTF_UNICODE][::2] == [
        ord(char) for char in "hello "
    ]


def test_sendinput_can_skip_space_and_does_not_double(monkeypatch) -> None:
    sent: list[inject.KeyEvent] = []
    monkeypatch.setattr(inject, "send_events", lambda events: sent.extend(events))
    inject.inject_text("hello", delay_ms=0, append_space=False)
    assert len(sent) == 10
    inject.inject_text("hello ", delay_ms=0, append_space=True)
    assert len(sent) == 22  # 10 + 12, the existing space is kept


def test_empty_text_does_nothing(monkeypatch) -> None:
    calls: list[object] = []
    monkeypatch.setattr(inject, "send_events", lambda events: calls.append(events))
    inject.inject_text("")
    assert calls == []


def test_clipboard_mode_restores_text(monkeypatch) -> None:
    clipboard = {"value": "old"}
    monkeypatch.setattr(inject.clipboard, "get_text", lambda: clipboard["value"])
    monkeypatch.setattr(
        inject.clipboard, "set_text", lambda value: clipboard.__setitem__("value", value)
    )
    monkeypatch.setattr(inject, "send_ctrl_v", lambda: None)
    monkeypatch.setattr(inject.time, "sleep", lambda seconds: None)

    inject.inject_text("new", method="clipboard", append_space=False)
    assert clipboard["value"] == "old"


def test_unknown_method_is_an_error() -> None:
    with pytest.raises(ValueError):
        inject.inject_text("hello", method="magic")
