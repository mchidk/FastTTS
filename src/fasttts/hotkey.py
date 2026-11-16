"""Caps Lock hold-to-talk: a pure state machine plus a Windows keyboard hook."""

from __future__ import annotations

import ctypes
import logging
import sys
import threading
import time
from collections.abc import Callable
from ctypes import wintypes
from enum import Enum

log = logging.getLogger(__name__)

WH_KEYBOARD_LL = 13
HC_ACTION = 0
WM_QUIT = 0x0012
VK_CAPITAL = 0x14
LLKHF_UP = 0x80
LLKHF_INJECTED = 0x10

ULONG_PTR = ctypes.c_ulonglong if ctypes.sizeof(ctypes.c_void_p) == 8 else ctypes.c_ulong


class Action(Enum):
    """What the app should do after a key transition."""

    NONE = "none"
    START = "start"
    STOP = "stop"


class HoldToTalk:
    """Press Caps Lock to start, release to stop. Auto-repeat is ignored."""

    def __init__(self) -> None:
        self.recording = False
        self._down = False
        self.pressed_at: float | None = None

    def press(self, now: float | None = None) -> Action:
        now = time.monotonic() if now is None else now
        if self._down:  # keyboard auto-repeat
            return Action.NONE
        self._down = True
        self.pressed_at = now
        if self.recording:
            return Action.NONE
        self.recording = True
        return Action.START

    def release(self, now: float | None = None) -> Action:
        if not self._down:
            return Action.NONE
        self._down = False
        self.pressed_at = None
        if not self.recording:
            return Action.NONE
        self.recording = False
        return Action.STOP

    def cancel(self) -> None:
        self.recording = False
        self._down = False
        self.pressed_at = None


if sys.platform == "win32":
    _user32 = ctypes.WinDLL("user32", use_last_error=True)
    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    HOOKPROC = ctypes.WINFUNCTYPE(ctypes.c_ssize_t, ctypes.c_int, wintypes.WPARAM, wintypes.LPARAM)

    class KBDLLHOOKSTRUCT(ctypes.Structure):
        _fields_ = [
            ("vkCode", wintypes.DWORD),
            ("scanCode", wintypes.DWORD),
            ("flags", wintypes.DWORD),
            ("time", wintypes.DWORD),
            ("dwExtraInfo", ULONG_PTR),
        ]

    _user32.SetWindowsHookExW.argtypes = [
        ctypes.c_int,
        HOOKPROC,
        wintypes.HINSTANCE,
        wintypes.DWORD,
    ]
    _user32.SetWindowsHookExW.restype = wintypes.HHOOK
    _user32.CallNextHookEx.argtypes = [
        wintypes.HHOOK,
        ctypes.c_int,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    _user32.CallNextHookEx.restype = ctypes.c_ssize_t
    _user32.UnhookWindowsHookEx.argtypes = [wintypes.HHOOK]
    _user32.UnhookWindowsHookEx.restype = wintypes.BOOL
    _user32.GetMessageW.argtypes = [
        ctypes.POINTER(wintypes.MSG),
        wintypes.HWND,
        wintypes.UINT,
        wintypes.UINT,
    ]
    _user32.GetMessageW.restype = wintypes.BOOL
    _user32.PostThreadMessageW.argtypes = [
        wintypes.DWORD,
        wintypes.UINT,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    _user32.PostThreadMessageW.restype = wintypes.BOOL
    _kernel32.GetCurrentThreadId.restype = wintypes.DWORD
    _kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    _kernel32.GetModuleHandleW.restype = wintypes.HMODULE
else:  # pragma: no cover - the hook is Windows-only
    HOOKPROC = None


class CapsLockHook:
    """Swallow Caps Lock and report press/release without changing caps state.

    ``should_handle`` lets the app pause dictation: when it returns False the
    key is passed through and Caps Lock behaves normally.
    """

    def __init__(
        self,
        on_press: Callable[[], None],
        on_release: Callable[[], None],
        *,
        should_handle: Callable[[], bool] | None = None,
    ) -> None:
        if sys.platform != "win32":
            raise RuntimeError("The Caps Lock hook is only available on Windows")
        self._on_press = on_press
        self._on_release = on_release
        self._should_handle = should_handle
        self._thread: threading.Thread | None = None
        self._thread_id: int | None = None
        self._hook = None
        self._hookproc = None
        self._ready = threading.Event()
        self._error: BaseException | None = None

    def start(self, timeout: float = 5.0) -> None:
        if self._thread is not None:
            raise RuntimeError("The hook is already running")
        self._ready.clear()
        self._error = None
        self._thread = threading.Thread(target=self._run, name="fasttts-hotkey", daemon=True)
        self._thread.start()
        if not self._ready.wait(timeout):
            raise RuntimeError("Timed out while installing the Caps Lock hook")
        if self._error is not None:
            raise RuntimeError(f"Could not install the Caps Lock hook: {self._error}")

    def _run(self) -> None:
        self._thread_id = _kernel32.GetCurrentThreadId()

        def callback(n_code, w_param, l_param):
            try:
                if n_code == HC_ACTION:
                    info = ctypes.cast(l_param, ctypes.POINTER(KBDLLHOOKSTRUCT)).contents
                    if info.vkCode == VK_CAPITAL and not info.flags & LLKHF_INJECTED:
                        if self._should_handle is not None and not self._should_handle():
                            return _user32.CallNextHookEx(self._hook, n_code, w_param, l_param)
                        if info.flags & LLKHF_UP:
                            self._on_release()
                        else:
                            self._on_press()
                        return 1  # swallow: Caps Lock never toggles
            except Exception:
                log.exception("Caps Lock hook callback failed")
            return _user32.CallNextHookEx(self._hook, n_code, w_param, l_param)

        self._hookproc = HOOKPROC(callback)
        self._hook = _user32.SetWindowsHookExW(
            WH_KEYBOARD_LL,
            self._hookproc,
            _kernel32.GetModuleHandleW(None),
            0,
        )
        if not self._hook:
            self._error = ctypes.WinError(ctypes.get_last_error())
            self._ready.set()
            return
        self._ready.set()

        message = wintypes.MSG()
        try:
            while True:
                result = _user32.GetMessageW(ctypes.byref(message), None, 0, 0)
                if result in (0, -1):
                    break
                _user32.TranslateMessage(ctypes.byref(message))
                _user32.DispatchMessageW(ctypes.byref(message))
        finally:
            if self._hook:
                _user32.UnhookWindowsHookEx(self._hook)
                self._hook = None
            self._hookproc = None
            self._thread_id = None

    def stop(self) -> None:
        if self._thread is None:
            return
        thread_id = self._thread_id
        if thread_id:
            _user32.PostThreadMessageW(thread_id, WM_QUIT, 0, 0)
        self._thread.join(timeout=2.0)
        self._thread = None
        self._ready.clear()

    @property
    def running(self) -> bool:
        return bool(self._thread and self._thread.is_alive() and self._hook)

    def __enter__(self) -> CapsLockHook:
        self.start()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.stop()
