"""A small always-on-top oscilloscope HUD, centered above the taskbar.

The window is a Win32 layered popup with per-pixel alpha, so it has rounded
corners and a soft glow without stealing focus or appearing in Alt-Tab.  It is
click-through and shows one of three states:

``recording``
    Live microphone bars from :meth:`Hud.push_level`.
``transcribing``
    A scanning pulse while Whisper works.
``error``
    A short message, then the HUD fades out.

Everything is rendered with Pillow at ~30 fps.  If Pillow or the Win32 calls are
unavailable the HUD degrades to a no-op; dictation keeps working.
"""

from __future__ import annotations

import contextlib
import ctypes
import logging
import math
import os
import sys
import threading
import time
from collections import deque
from ctypes import wintypes
from pathlib import Path

log = logging.getLogger(__name__)

WIDTH = 340
HEIGHT = 86
PAD = 10
MARGIN = 4
INTERVAL_MS = 33

ACCENTS = {
    "recording": (255, 92, 108),
    "transcribing": (255, 180, 84),
    "error": (248, 113, 113),
    "idle": (124, 138, 165),
}
LABELS = {
    "recording": "LISTENING",
    "transcribing": "TRANSCRIBING",
    "error": "ERROR",
    "idle": "READY",
}

if sys.platform == "win32":
    _user32 = ctypes.WinDLL("user32", use_last_error=True)
    _gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    WNDPROC = ctypes.WINFUNCTYPE(
        ctypes.c_ssize_t, wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM
    )
    UINT_PTR = ctypes.c_size_t

    class WNDCLASSEXW(ctypes.Structure):
        _fields_ = [
            ("cbSize", wintypes.UINT),
            ("style", wintypes.UINT),
            ("lpfnWndProc", WNDPROC),
            ("cbClsExtra", ctypes.c_int),
            ("cbWndExtra", ctypes.c_int),
            ("hInstance", wintypes.HINSTANCE),
            ("hIcon", wintypes.HICON),
            ("hCursor", wintypes.HANDLE),
            ("hbrBackground", wintypes.HBRUSH),
            ("lpszMenuName", wintypes.LPCWSTR),
            ("lpszClassName", wintypes.LPCWSTR),
            ("hIconSm", wintypes.HICON),
        ]

    class BITMAPINFOHEADER(ctypes.Structure):
        _fields_ = [
            ("biSize", wintypes.DWORD),
            ("biWidth", wintypes.LONG),
            ("biHeight", wintypes.LONG),
            ("biPlanes", wintypes.WORD),
            ("biBitCount", wintypes.WORD),
            ("biCompression", wintypes.DWORD),
            ("biSizeImage", wintypes.DWORD),
            ("biXPelsPerMeter", wintypes.LONG),
            ("biYPelsPerMeter", wintypes.LONG),
            ("biClrUsed", wintypes.DWORD),
            ("biClrImportant", wintypes.DWORD),
        ]

    class BITMAPINFO(ctypes.Structure):
        _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD * 3)]

    class BLENDFUNCTION(ctypes.Structure):
        _fields_ = [
            ("BlendOp", ctypes.c_ubyte),
            ("BlendFlags", ctypes.c_ubyte),
            ("SourceConstantAlpha", ctypes.c_ubyte),
            ("AlphaFormat", ctypes.c_ubyte),
        ]

    _user32.RegisterClassExW.argtypes = [ctypes.POINTER(WNDCLASSEXW)]
    _user32.RegisterClassExW.restype = wintypes.ATOM
    _user32.CreateWindowExW.argtypes = [
        wintypes.DWORD,
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        wintypes.HWND,
        wintypes.HMENU,
        wintypes.HINSTANCE,
        wintypes.LPVOID,
    ]
    _user32.CreateWindowExW.restype = wintypes.HWND
    _user32.DefWindowProcW.argtypes = [
        wintypes.HWND,
        wintypes.UINT,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    _user32.DefWindowProcW.restype = ctypes.c_ssize_t
    _user32.ShowWindow.argtypes = [wintypes.HWND, ctypes.c_int]
    _user32.ShowWindow.restype = wintypes.BOOL
    _user32.UpdateWindow.argtypes = [wintypes.HWND]
    _user32.SetTimer.argtypes = [wintypes.HWND, UINT_PTR, wintypes.UINT, wintypes.LPVOID]
    _user32.SetTimer.restype = UINT_PTR
    _user32.KillTimer.argtypes = [wintypes.HWND, UINT_PTR]
    _user32.KillTimer.restype = wintypes.BOOL
    _user32.DestroyWindow.argtypes = [wintypes.HWND]
    _user32.PostQuitMessage.argtypes = [ctypes.c_int]
    _user32.LoadCursorW.argtypes = [wintypes.HINSTANCE, ctypes.c_void_p]
    _user32.LoadCursorW.restype = wintypes.HANDLE
    _user32.GetSystemMetrics.argtypes = [ctypes.c_int]
    _user32.GetSystemMetrics.restype = ctypes.c_int
    _kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    _kernel32.GetModuleHandleW.restype = wintypes.HMODULE
    try:
        _user32.SetProcessDpiAwarenessContext.argtypes = [wintypes.HANDLE]
        _user32.SetProcessDpiAwarenessContext.restype = wintypes.BOOL
    except AttributeError:
        pass
    _user32.PostMessageW.argtypes = [
        wintypes.HWND,
        wintypes.UINT,
        wintypes.WPARAM,
        wintypes.LPARAM,
    ]
    _user32.GetMessageW.argtypes = [
        ctypes.POINTER(wintypes.MSG),
        wintypes.HWND,
        wintypes.UINT,
        wintypes.UINT,
    ]
    _user32.GetMessageW.restype = wintypes.BOOL
    _user32.TranslateMessage.argtypes = [ctypes.POINTER(wintypes.MSG)]
    _user32.DispatchMessageW.argtypes = [ctypes.POINTER(wintypes.MSG)]
    _user32.SystemParametersInfoW.argtypes = [
        wintypes.UINT,
        wintypes.UINT,
        wintypes.LPVOID,
        wintypes.UINT,
    ]
    _user32.SystemParametersInfoW.restype = wintypes.BOOL
    _user32.GetDC.argtypes = [wintypes.HWND]
    _user32.GetDC.restype = wintypes.HDC
    _user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
    _user32.UpdateLayeredWindow.argtypes = [
        wintypes.HWND,
        wintypes.HDC,
        ctypes.POINTER(wintypes.POINT),
        ctypes.POINTER(wintypes.SIZE),
        wintypes.HDC,
        ctypes.POINTER(wintypes.POINT),
        wintypes.COLORREF,
        ctypes.POINTER(BLENDFUNCTION),
        wintypes.DWORD,
    ]
    _user32.UpdateLayeredWindow.restype = wintypes.BOOL
    _user32.GetDpiForSystem.restype = wintypes.UINT

    _gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
    _gdi32.CreateCompatibleDC.restype = wintypes.HDC
    _gdi32.DeleteDC.argtypes = [wintypes.HDC]
    _gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HGDIOBJ]
    _gdi32.SelectObject.restype = wintypes.HGDIOBJ
    _gdi32.DeleteObject.argtypes = [wintypes.HGDIOBJ]
    _gdi32.CreateDIBSection.argtypes = [
        wintypes.HDC,
        ctypes.POINTER(BITMAPINFO),
        wintypes.UINT,
        ctypes.POINTER(ctypes.c_void_p),
        wintypes.HANDLE,
        wintypes.DWORD,
    ]
    _gdi32.CreateDIBSection.restype = wintypes.HBITMAP

else:  # pragma: no cover - the HUD is Windows-only
    WNDPROC = None


def _load_pillow():
    try:
        from PIL import Image, ImageDraw, ImageFilter, ImageFont
    except ImportError:
        return None
    return Image, ImageDraw, ImageFilter, ImageFont


class Hud:
    """Thread-safe facade over the layered oscilloscope window."""

    def __init__(self, *, enabled: bool = True) -> None:
        self.enabled = enabled and sys.platform == "win32"
        self._thread: threading.Thread | None = None
        self._ready = threading.Event()
        self._error: str | None = None
        self._hwnd = None
        self._lock = threading.Lock()
        self._mode = "hidden"
        self._changed_at = time.monotonic()
        self._detail = ""
        self._levels: deque[float] = deque(maxlen=48)
        self._display: list[float] = []
        self._visible = False
        self._opacity = 0.0
        self._wndproc = None

    # -- public API ---------------------------------------------------------

    def start(self) -> None:
        if not self.enabled or self._thread is not None:
            return
        if _load_pillow() is None:
            log.warning("Pillow is not installed; the HUD is disabled")
            self.enabled = False
            return
        self._thread = threading.Thread(target=self._thread_main, name="fasttts-hud", daemon=True)
        self._thread.start()
        if not self._ready.wait(5.0):
            log.warning("Timed out while creating the HUD window")
            self.enabled = False

    def set_mode(self, mode: str, detail: str = "") -> None:
        """Show ``recording``, ``transcribing``, ``error`` or hide."""
        if mode not in {*ACCENTS, "hidden"}:
            raise ValueError(f"Unknown HUD mode '{mode}'")
        with self._lock:
            self._mode = mode
            self._detail = detail
            self._changed_at = time.monotonic()
            if mode != "recording":
                self._levels.clear()

    def push_level(self, rms: float) -> None:
        """Feed a microphone RMS value (called from the audio thread)."""
        with self._lock:
            if self._mode == "recording":
                self._levels.append(max(0.0, min(1.0, float(rms))))

    def hide(self) -> None:
        self.set_mode("hidden")

    def stop(self) -> None:
        if self._thread is None:
            return
        if self._hwnd:
            _user32.PostMessageW(self._hwnd, 0x0010, 0, 0)  # WM_CLOSE
        self._thread.join(timeout=2.0)
        self._thread = None
        self._hwnd = None

    # -- window thread ------------------------------------------------------

    def _thread_main(self) -> None:
        try:
            with contextlib.suppress(Exception):
                _user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
            scale = (
                max(1.0, _user32.GetDpiForSystem() / 96.0)
                if hasattr(_user32, "GetDpiForSystem")
                else 1.0
            )
            self._scale = scale
            self._width = round(WIDTH * scale)
            self._height = round(HEIGHT * scale)

            instance = _kernel32.GetModuleHandleW(None)
            class_name = "FastTTS_oscilloscope"
            self._wndproc = WNDPROC(self._wndproc_impl)
            window_class = WNDCLASSEXW(
                cbSize=ctypes.sizeof(WNDCLASSEXW),
                style=0x0003,  # CS_HREDRAW | CS_VREDRAW
                lpfnWndProc=self._wndproc,
                cbClsExtra=0,
                cbWndExtra=0,
                hInstance=instance,
                hIcon=None,
                hCursor=_user32.LoadCursorW(None, ctypes.c_void_p(32512)),
                hbrBackground=None,
                lpszMenuName=None,
                lpszClassName=class_name,
                hIconSm=None,
            )
            atom = _user32.RegisterClassExW(ctypes.byref(window_class))
            if not atom and ctypes.get_last_error() != 1410:
                raise ctypes.WinError(ctypes.get_last_error())

            x, y = self._position()
            self._pos_x, self._pos_y = x, y
            ex_style = 0x00080000 | 0x00000020 | 0x00000008 | 0x00000080 | 0x08000000
            # WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
            self._hwnd = _user32.CreateWindowExW(
                ex_style,
                class_name,
                "FastTTS",
                0x80000000,  # WS_POPUP
                x,
                y,
                self._width,
                self._height,
                None,
                None,
                instance,
                None,
            )
            if not self._hwnd:
                raise ctypes.WinError(ctypes.get_last_error())
            _user32.SetTimer(self._hwnd, 1, INTERVAL_MS, None)
            self._ready.set()

            message = wintypes.MSG()
            while True:
                result = _user32.GetMessageW(ctypes.byref(message), None, 0, 0)
                if result in (0, -1):
                    break
                _user32.TranslateMessage(ctypes.byref(message))
                _user32.DispatchMessageW(ctypes.byref(message))
        except Exception as exc:
            self._error = str(exc)
            log.warning("HUD unavailable: %s", exc)
            self._ready.set()
        finally:
            self._hwnd = None

    def _position(self) -> tuple[int, int]:
        rect = wintypes.RECT()
        if not _user32.SystemParametersInfoW(0x0030, 0, ctypes.byref(rect), 0):
            rect.left, rect.top = 0, 0
            rect.right = _user32.GetSystemMetrics(0)
            rect.bottom = _user32.GetSystemMetrics(1)
        width = getattr(self, "_width", WIDTH)
        height = getattr(self, "_height", HEIGHT)
        margin = round(MARGIN * getattr(self, "_scale", 1.0))
        x = rect.left + max(0, (rect.right - rect.left - width) // 2)
        y = rect.bottom - height - margin
        return x, y

    def _wndproc_impl(self, hwnd, message, w_param, l_param):
        try:
            if message == 0x0113:  # WM_TIMER
                self._tick()
                return 0
            if message == 0x0010:  # WM_CLOSE
                _user32.DestroyWindow(hwnd)
                return 0
            if message == 0x0002:  # WM_DESTROY
                _user32.KillTimer(hwnd, 1)
                _user32.PostQuitMessage(0)
                return 0
        except Exception:
            log.exception("HUD render failed")
        return _user32.DefWindowProcW(hwnd, message, w_param, l_param)

    # -- animation ----------------------------------------------------------

    def _tick(self) -> None:
        with self._lock:
            mode = self._mode
            detail = self._detail
            changed_at = self._changed_at
            levels = list(self._levels)
        now = time.monotonic()

        if mode == "error" and now - changed_at > 2.4:
            self.hide()
            mode = "hidden"
        if mode == "hidden":
            if self._visible:
                self._opacity -= 55
                if self._opacity <= 0:
                    self._opacity = 0
                    self._visible = False
                    _user32.ShowWindow(self._hwnd, 0)  # SW_HIDE
                else:
                    self._present(self._render(mode, detail, levels, now))
            return

        if not self._visible:
            self._visible = True
            self._opacity = 0
            _user32.ShowWindow(self._hwnd, 4)  # SW_SHOWNOACTIVATE
        self._opacity = min(255, self._opacity + 55)
        self._present(self._render(mode, detail, levels, now))

    def _render(self, mode: str, detail: str, levels: list[float], now: float):
        pillow = _load_pillow()
        if pillow is None:
            return None
        Image, ImageDraw, ImageFilter, ImageFont = pillow

        scale = getattr(self, "_scale", 1.0)
        width = getattr(self, "_width", WIDTH)
        height = getattr(self, "_height", HEIGHT)
        inset = round(PAD * scale)
        accent = ACCENTS.get(mode, ACCENTS["idle"])
        label = LABELS.get(mode, mode.upper())
        if mode == "error" and detail:
            label = detail.upper()[:30]

        box = [inset, inset, width - inset, height - inset]
        panel_width = box[2] - box[0]
        panel_height = box[3] - box[1]
        radius = round(22 * scale)

        # Soft drop shadow, then a vertical-gradient rounded panel.
        canvas = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        shadow = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        ImageDraw.Draw(shadow).rounded_rectangle(
            [
                box[0] + round(1 * scale),
                box[1] + round(6 * scale),
                box[2] + round(1 * scale),
                box[3] + round(6 * scale),
            ],
            radius=radius,
            fill=(0, 0, 0, 130),
        )
        canvas = Image.alpha_composite(
            canvas, shadow.filter(ImageFilter.GaussianBlur(round(7 * scale)))
        )

        gradient = Image.new("RGBA", (panel_width, panel_height), (0, 0, 0, 0))
        gradient_draw = ImageDraw.Draw(gradient)
        top_color = (19, 23, 31)
        bottom_color = (9, 12, 18)
        for y in range(panel_height):
            blend = y / max(1, panel_height - 1)
            color = tuple(
                round(top_color[i] + (bottom_color[i] - top_color[i]) * blend) for i in range(3)
            )
            gradient_draw.line([(0, y), (panel_width, y)], fill=(*color, 246))

        # Supersample the corner mask for smooth per-pixel alpha.
        supersample = 4
        big_mask = Image.new("L", (panel_width * supersample, panel_height * supersample), 0)
        ImageDraw.Draw(big_mask).rounded_rectangle(
            [0, 0, panel_width * supersample - 1, panel_height * supersample - 1],
            radius=radius * supersample,
            fill=255,
        )
        panel_mask = big_mask.resize((panel_width, panel_height), Image.LANCZOS)
        gradient.putalpha(
            Image.composite(
                gradient.getchannel("A"), Image.new("L", (panel_width, panel_height)), panel_mask
            )
        )
        panel_layer = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        panel_layer.paste(gradient, (box[0], box[1]))
        panel = Image.alpha_composite(canvas, panel_layer)
        draw = ImageDraw.Draw(panel)
        draw.rounded_rectangle(
            box,
            radius=radius,
            outline=(255, 255, 255, 30),
            width=max(1, round(scale)),
        )

        # Accent bloom in the lower-left corner, clipped to the panel.
        bloom = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        ImageDraw.Draw(bloom).ellipse(
            [
                box[0] - round(50 * scale),
                box[3] - round(26 * scale),
                box[0] + round(94 * scale),
                box[3] + round(48 * scale),
            ],
            fill=(*accent, 64),
        )
        full_mask = Image.new("L", (width, height), 0)
        ImageDraw.Draw(full_mask).rounded_rectangle(box, radius=radius, fill=255)
        panel = Image.alpha_composite(
            panel, Image.composite(bloom, Image.new("RGBA", (width, height)), full_mask)
        )

        font = self._font(ImageFont, max(9, round(10 * scale)), bold=True)
        tiny = self._font(ImageFont, max(7, round(8 * scale)), bold=False)
        dot_x = box[0] + round(17 * scale)
        dot_y = box[1] + round(19 * scale)
        glow = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        ImageDraw.Draw(glow).ellipse(
            [
                dot_x - round(5 * scale),
                dot_y - round(5 * scale),
                dot_x + round(5 * scale),
                dot_y + round(5 * scale),
            ],
            fill=(*accent, 160),
        )
        panel = Image.alpha_composite(
            panel, glow.filter(ImageFilter.GaussianBlur(round(2.8 * scale)))
        )
        draw = ImageDraw.Draw(panel)
        draw.ellipse(
            [
                dot_x - round(2.5 * scale),
                dot_y - round(2.5 * scale),
                dot_x + round(2.5 * scale),
                dot_y + round(2.5 * scale),
            ],
            fill=(*accent, 255),
        )
        self._tracked_text(
            draw,
            (box[0] + round(27 * scale), box[1] + round(12 * scale)),
            label,
            font,
            (222, 228, 238, 240),
            scale,
        )
        brand = "FastTTS"
        brand_width = draw.textlength(brand, font=tiny)
        draw.text(
            (box[2] - round(15 * scale) - brand_width, box[1] + round(13 * scale)),
            brand,
            font=tiny,
            fill=(136, 148, 166, 170),
        )

        bars = 38
        left = box[0] + round(18 * scale)
        right = box[2] - round(18 * scale)
        center = box[1] + round(panel_height * 0.71)
        max_height = round(15 * scale)
        gap = (right - left) / bars
        bar_width = max(2.0, 2.0 * scale)
        heights = self._bar_heights(mode, levels, bars, max_height, now)
        wave = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        wave_draw = ImageDraw.Draw(wave)
        for index, bar_height in enumerate(heights):
            x = left + index * gap + (gap - bar_width) / 2
            top = center - bar_height / 2
            bottom = center + bar_height / 2
            wave_draw.rounded_rectangle(
                [x, top, x + bar_width, bottom],
                radius=bar_width / 2,
                fill=(*accent, 255),
            )
        glow_wave = wave.filter(ImageFilter.GaussianBlur(round(3.4 * scale)))
        panel = Image.alpha_composite(panel, glow_wave)
        panel = Image.alpha_composite(panel, wave)

        return panel

    def _bar_heights(
        self, mode: str, levels: list[float], bars: int, max_height: int, now: float
    ) -> list[float]:
        if mode == "recording":
            if not levels:
                levels = [0.0]
            targets = []
            for index in range(bars):
                source = int(index * (len(levels) - 1) / max(1, bars - 1))
                value = levels[source]
                targets.append(0.06 + min(1.0, value * 5.5) ** 0.7)
        elif mode == "transcribing":
            position = (now * 1.15) % 1.0 * (bars - 1)
            targets = []
            for index in range(bars):
                distance = (index - position) / 2.6
                pulse = math.exp(-(distance * distance))
                targets.append(0.05 + 0.55 * pulse + 0.04 * math.sin(now * 5 + index * 0.5))
        elif mode == "error":
            targets = [0.06 + 0.04 * math.sin(now * 8 + index) for index in range(bars)]
        else:
            targets = [0.05 for _ in range(bars)]

        if len(self._display) != bars:
            self._display = targets
        else:
            self._display = [
                current + (target - current) * 0.42
                for current, target in zip(self._display, targets, strict=True)
            ]
        return [max(2.0, value * max_height) for value in self._display]

    def _present(self, image) -> None:
        if image is None or not self._hwnd:
            return
        import numpy as np

        rgba = image.convert("RGBA")
        data = np.asarray(rgba, dtype=np.uint16)
        alpha = data[:, :, 3:4]
        rgb = (data[:, :, :3].astype(np.uint16) * alpha + 127) // 255
        premultiplied = np.dstack((rgb[:, :, 2], rgb[:, :, 1], rgb[:, :, 0], alpha)).astype(
            np.uint8
        )
        payload = premultiplied.tobytes()

        screen_dc = _user32.GetDC(None)
        memory_dc = _gdi32.CreateCompatibleDC(screen_dc)
        bitmap = None
        old_bitmap = None
        try:
            info = BITMAPINFO()
            info.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
            info.bmiHeader.biWidth = self._width
            info.bmiHeader.biHeight = -self._height
            info.bmiHeader.biPlanes = 1
            info.bmiHeader.biBitCount = 32
            info.bmiHeader.biCompression = 0
            bits = ctypes.c_void_p()
            bitmap = _gdi32.CreateDIBSection(
                memory_dc, ctypes.byref(info), 0, ctypes.byref(bits), None, 0
            )
            if not bitmap:
                return
            ctypes.memmove(bits, payload, len(payload))
            old_bitmap = _gdi32.SelectObject(memory_dc, bitmap)
            destination = wintypes.POINT(self._x, self._y)
            size = wintypes.SIZE(self._width, self._height)
            source = wintypes.POINT(0, 0)
            blend = BLENDFUNCTION(0, 0, max(0, min(255, round(self._opacity))), 1)
            _user32.UpdateLayeredWindow(
                self._hwnd,
                screen_dc,
                ctypes.byref(destination),
                ctypes.byref(size),
                memory_dc,
                ctypes.byref(source),
                0,
                ctypes.byref(blend),
                0x00000002,
            )
        finally:
            if old_bitmap:
                _gdi32.SelectObject(memory_dc, old_bitmap)
            if bitmap:
                _gdi32.DeleteObject(bitmap)
            _gdi32.DeleteDC(memory_dc)
            _user32.ReleaseDC(None, screen_dc)

    @property
    def _x(self) -> int:
        return getattr(self, "_pos_x", 0)

    @property
    def _y(self) -> int:
        return getattr(self, "_pos_y", 0)

    def _font(self, ImageFont, size: int, *, bold: bool) -> object:
        windir = Path(os.environ.get("WINDIR", r"C:\Windows")) / "Fonts"
        names = ("segoeuib.ttf", "arialbd.ttf") if bold else ("segoeui.ttf", "arial.ttf")
        for name in names:
            path = windir / name
            if path.is_file():
                try:
                    return ImageFont.truetype(str(path), size)
                except OSError:
                    continue
        try:
            return ImageFont.load_default(size=size)
        except TypeError:
            return ImageFont.load_default()

    def _tracked_text(self, draw, xy, text: str, font, fill, scale: float) -> None:
        x, y = xy
        tracking = 1.1 * scale
        for char in text:
            draw.text((x, y), char, font=font, fill=fill)
            x += draw.textlength(char, font=font) + tracking


def run_preview(seconds: float = 8.0) -> None:
    """Animate every HUD state; useful for development and screenshots."""
    hud = Hud()
    hud.start()
    try:
        hud.set_mode("recording")
        started = time.monotonic()
        while (elapsed := time.monotonic() - started) < seconds / 3:
            hud.push_level(
                0.04 + 0.12 * abs(math.sin(elapsed * 3.1)) + 0.05 * abs(math.sin(elapsed * 9))
            )
            time.sleep(0.03)
        hud.set_mode("transcribing")
        time.sleep(seconds / 2)
        hud.hide()
        time.sleep(0.3)
    finally:
        hud.stop()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    run_preview()
