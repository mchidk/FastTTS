"""Windows clipboard helpers (text only, stdlib ctypes)."""

from __future__ import annotations

import ctypes
import sys
import time
from ctypes import wintypes

CF_UNICODETEXT = 13
GMEM_MOVEABLE = 0x0002

if sys.platform == "win32":
    _user32 = ctypes.WinDLL("user32", use_last_error=True)
    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    _user32.OpenClipboard.argtypes = [wintypes.HWND]
    _user32.OpenClipboard.restype = wintypes.BOOL
    _user32.CloseClipboard.restype = wintypes.BOOL
    _user32.EmptyClipboard.restype = wintypes.BOOL
    _user32.GetClipboardData.argtypes = [wintypes.UINT]
    _user32.GetClipboardData.restype = wintypes.HANDLE
    _user32.SetClipboardData.argtypes = [wintypes.UINT, wintypes.HANDLE]
    _user32.SetClipboardData.restype = wintypes.HANDLE

    _kernel32.GlobalAlloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
    _kernel32.GlobalAlloc.restype = wintypes.HGLOBAL
    _kernel32.GlobalLock.argtypes = [wintypes.HGLOBAL]
    _kernel32.GlobalLock.restype = wintypes.LPVOID
    _kernel32.GlobalUnlock.argtypes = [wintypes.HGLOBAL]
    _kernel32.GlobalUnlock.restype = wintypes.BOOL
    _kernel32.GlobalFree.argtypes = [wintypes.HGLOBAL]
    _kernel32.GlobalFree.restype = wintypes.HGLOBAL
else:  # pragma: no cover - imported only on Windows paths
    _user32 = None
    _kernel32 = None


def _require_windows() -> None:
    if sys.platform != "win32":
        raise RuntimeError("Clipboard access is only supported on Windows")


def _open_clipboard(retries: int = 6) -> None:
    _require_windows()
    for attempt in range(retries):
        if _user32.OpenClipboard(None):
            return
        time.sleep(0.02 * (attempt + 1))
    raise OSError(ctypes.get_last_error(), "Could not open the Windows clipboard")


def get_text() -> str:
    """Return the clipboard's Unicode text, or an empty string."""
    _open_clipboard()
    try:
        handle = _user32.GetClipboardData(CF_UNICODETEXT)
        if not handle:
            return ""
        pointer = _kernel32.GlobalLock(handle)
        if not pointer:
            return ""
        try:
            return ctypes.wstring_at(pointer)
        finally:
            _kernel32.GlobalUnlock(handle)
    finally:
        _user32.CloseClipboard()


def set_text(text: str) -> None:
    """Replace the clipboard with Unicode text."""
    if "\x00" in text:
        raise ValueError("Clipboard text must not contain NUL characters")
    _open_clipboard()
    try:
        if not _user32.EmptyClipboard():
            raise OSError(ctypes.get_last_error(), "Could not empty the clipboard")
        data = text.encode("utf-16-le") + b"\x00\x00"
        handle = _kernel32.GlobalAlloc(GMEM_MOVEABLE, len(data))
        if not handle:
            raise OSError(ctypes.get_last_error(), "Could not allocate clipboard memory")
        pointer = _kernel32.GlobalLock(handle)
        if not pointer:
            _kernel32.GlobalFree(handle)
            raise OSError(ctypes.get_last_error(), "Could not lock clipboard memory")
        try:
            ctypes.memmove(pointer, data, len(data))
        finally:
            _kernel32.GlobalUnlock(handle)
        if not _user32.SetClipboardData(CF_UNICODETEXT, handle):
            _kernel32.GlobalFree(handle)
            raise OSError(ctypes.get_last_error(), "Could not set clipboard data")
        # Windows owns the handle after SetClipboardData.
    finally:
        _user32.CloseClipboard()
