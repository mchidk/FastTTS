"""Type text into the focused Windows control.

``sendinput`` synthesizes Unicode keystrokes, so the clipboard is untouched and
the text works in normal desktop apps. ``clipboard`` pastes instead, which helps
with editors that ignore synthetic Unicode. An unelevated FastTTS cannot type
into an elevated (administrator) window; Windows blocks it.
"""

from __future__ import annotations

import ctypes
import logging
import sys
import time
from ctypes import wintypes
from dataclasses import dataclass

from . import clipboard

log = logging.getLogger(__name__)

KEYEVENTF_EXTENDEDKEY = 0x0001
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004

VK_RETURN = 0x0D
VK_TAB = 0x09
VK_CONTROL = 0x11
VK_V = 0x56

ULONG_PTR = ctypes.c_ulonglong if ctypes.sizeof(ctypes.c_void_p) == 8 else ctypes.c_ulong


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [
        ("uMsg", wintypes.DWORD),
        ("wParamL", wintypes.WORD),
        ("wParamH", wintypes.WORD),
    ]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT), ("hi", HARDWAREINPUT)]  # noqa: RUF012


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


INPUT_KEYBOARD = 1

if sys.platform == "win32":
    _user32 = ctypes.WinDLL("user32", use_last_error=True)
    _user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
    _user32.SendInput.restype = wintypes.UINT
else:  # pragma: no cover - the injector is Windows-only
    _user32 = None


@dataclass(frozen=True)
class KeyEvent:
    """One keyboard transition, testable without touching Windows."""

    vk: int = 0
    scan: int = 0
    flags: int = 0


def unicode_events(char: str) -> list[KeyEvent]:
    """Build UTF-16 key events for one Python character."""
    units = char.encode("utf-16-le")
    events: list[KeyEvent] = []
    for index in range(0, len(units), 2):
        unit = int.from_bytes(units[index : index + 2], "little")
        events.append(KeyEvent(scan=unit, flags=KEYEVENTF_UNICODE))
        events.append(KeyEvent(scan=unit, flags=KEYEVENTF_UNICODE | KEYEVENTF_KEYUP))
    return events


def text_to_events(text: str) -> list[KeyEvent]:
    """Translate text to the event sequence used by the injector."""
    events: list[KeyEvent] = []
    for char in text:
        if char == "\n":
            events.extend(
                [
                    KeyEvent(vk=VK_RETURN),
                    KeyEvent(vk=VK_RETURN, flags=KEYEVENTF_KEYUP),
                ]
            )
        elif char == "\t":
            events.extend(
                [
                    KeyEvent(vk=VK_TAB),
                    KeyEvent(vk=VK_TAB, flags=KEYEVENTF_KEYUP),
                ]
            )
        else:
            events.extend(unicode_events(char))
    return events


def _build_input(event: KeyEvent) -> INPUT:
    item = INPUT(type=INPUT_KEYBOARD)
    item.ki = KEYBDINPUT(
        wVk=event.vk,
        wScan=event.scan,
        dwFlags=event.flags,
        time=0,
        dwExtraInfo=0,
    )
    return item


def send_events(events: list[KeyEvent]) -> None:
    """Send events to the foreground window."""
    if sys.platform != "win32":
        raise RuntimeError("Text injection is only supported on Windows")
    if not events:
        return
    array_type = INPUT * len(events)
    inputs = array_type(*(_build_input(event) for event in events))
    sent = _user32.SendInput(len(events), inputs, ctypes.sizeof(INPUT))
    if sent != len(events):
        error = ctypes.get_last_error()
        raise OSError(
            error,
            "SendInput was blocked. Windows does not let an unelevated app type "
            "into an elevated (administrator) window.",
        )


def send_ctrl_v() -> None:
    """Press Ctrl+V in the foreground window."""
    send_events(
        [
            KeyEvent(vk=VK_CONTROL),
            KeyEvent(vk=VK_V),
            KeyEvent(vk=VK_V, flags=KEYEVENTF_KEYUP),
            KeyEvent(vk=VK_CONTROL, flags=KEYEVENTF_KEYUP),
        ]
    )


def inject_text(
    text: str,
    *,
    method: str = "sendinput",
    delay_ms: int = 1,
    append_space: bool = True,
    paste_delay_ms: int = 180,
) -> None:
    """Insert ``text`` at the caret of the foreground window."""
    if not text:
        return
    if append_space and not text.endswith((" ", "\n", "\t")):
        text += " "

    if method == "sendinput":
        # Per-character delay keeps slower editors from dropping code points.
        if delay_ms:
            for char in text:
                send_events(text_to_events(char))
                time.sleep(delay_ms / 1000.0)
        else:
            send_events(text_to_events(text))
        return

    if method == "clipboard":
        previous = clipboard.get_text()
        for _ in range(3):
            clipboard.set_text(text)
            if clipboard.get_text() == text:
                break
            time.sleep(0.03)
        else:
            raise OSError("Could not put the transcript on the clipboard")
        try:
            time.sleep(0.08)
            send_ctrl_v()
            time.sleep(paste_delay_ms / 1000.0)
        finally:
            try:
                # Only restore if the clipboard still holds our text; another
                # app may have replaced it in the meantime.
                if clipboard.get_text() == text:
                    clipboard.set_text(previous)
            except OSError as exc:  # clipboard may be held by another app
                log.warning("Could not restore the clipboard: %s", exc)
        return

    raise ValueError(f"Unknown injection method '{method}'")
