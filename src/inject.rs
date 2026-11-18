//! Put the transcript at the caret.
//!
//! Pasting borrows the clipboard. The text is offered with delayed
//! rendering, so Windows tells FastTTS the moment the target app actually
//! reads it. Only then is the user's clipboard put back, however slow the
//! app is (Windows 11 Notepad reads asynchronously). If no app reads the
//! offer, the paste did not happen and the text is typed instead.

use crate::hook::INJECT_TAG;
use anyhow::{anyhow, bail, Context, Result};
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::{
            GlobalFree, SetLastError, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WIN32_ERROR, WPARAM,
        },
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
                GetClipboardOwner, GetClipboardSequenceNumber, OpenClipboard,
                RegisterClipboardFormatW, SetClipboardData,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
        },
        UI::{
            Input::KeyboardAndMouse::{
                SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
                KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_RETURN, VK_TAB,
                VK_V,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, MsgWaitForMultipleObjects,
                PeekMessageW, RegisterClassExW, HWND_MESSAGE, MSG, PM_REMOVE, QS_ALLINPUT,
                WINDOW_EX_STYLE, WINDOW_STYLE, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSEXW,
            },
        },
    },
};

const CF_UNICODETEXT: u32 = 13;
/// Clipboard formats held as GDI handles rather than memory; Windows
/// re-synthesises them from the memory formats that are saved.
const HANDLE_FORMATS: &[u32] = &[2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E];
/// Do not snapshot clipboards larger than this.
const SNAPSHOT_LIMIT: usize = 64 << 20;
/// After the app has read the text, give it this long to finish pasting.
const SETTLE: Duration = Duration::from_millis(150);

thread_local! {
    /// UTF-16 text (with terminator) waiting to be rendered.
    static OFFER: RefCell<Option<Vec<u16>>> = const { RefCell::new(None) };
    /// Clipboard sequence number right after the offer was rendered.
    static TAKEN: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Owns the hidden clipboard window. Create and use it on one thread.
pub struct Injector {
    hwnd: HWND,
}

impl Injector {
    pub fn new() -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("FastTTSClipboard");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(clipboard_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                class,
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )
            .context("could not create the clipboard window")?;
            Ok(Self { hwnd })
        }
    }

    /// Insert `text` at the caret with the configured method, falling back
    /// to the other one if it fails.
    pub fn insert(
        &self,
        text: &str,
        method: &str,
        char_delay_ms: u64,
        paste_timeout_ms: u64,
        append_space: bool,
    ) -> Result<()> {
        let mut text = text.to_string();
        if append_space && !text.ends_with(char::is_whitespace) {
            text.push(' ');
        }
        let timeout = Duration::from_millis(paste_timeout_ms);
        match method {
            "clipboard" => self.paste(&text, timeout, send_ctrl_v).or_else(|err| {
                tracing::warn!("paste failed ({err:#}); typing instead");
                type_text(&text, char_delay_ms.max(5))
            }),
            "sendinput" => type_text(&text, char_delay_ms).or_else(|err| {
                tracing::warn!("typing failed ({err:#}); pasting instead");
                self.paste(&text, timeout, send_ctrl_v)
            }),
            _ => Ok(()),
        }
    }

    /// Offer `text` on the clipboard, run `trigger` (Ctrl+V), and wait until
    /// an app has read it. Fails if nobody did within `timeout`.
    fn paste(
        &self,
        text: &str,
        timeout: Duration,
        trigger: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let saved = snapshot(self.hwnd);
        self.offer(text)?;
        let triggered = trigger();
        let started = Instant::now();
        while triggered.is_ok() && TAKEN.get().is_none() && started.elapsed() < timeout {
            pump(timeout.saturating_sub(started.elapsed()));
        }
        let taken = TAKEN.get();
        if taken.is_some() {
            let settle = Instant::now();
            while settle.elapsed() < SETTLE {
                pump(SETTLE - settle.elapsed());
            }
        }
        // Put the user's clipboard back unless someone copied meanwhile.
        let untouched = match taken {
            Some(sequence) => (unsafe { GetClipboardSequenceNumber() }) == sequence,
            None => unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == self.hwnd),
        };
        if untouched {
            match &saved {
                Some(saved) => restore(self.hwnd, saved),
                None => clear(self.hwnd),
            }
        }
        OFFER.set(None);
        triggered?;
        match taken {
            Some(_) => Ok(()),
            None => bail!("no app took the pasted text within {timeout:?}"),
        }
    }

    /// Put a delayed-render promise of `text` on the clipboard, marked so
    /// clipboard history, cloud sync and clipboard managers leave it alone.
    fn offer(&self, text: &str) -> Result<()> {
        OFFER.set(Some(text.encode_utf16().chain(Some(0)).collect()));
        TAKEN.set(None);
        let _open = OpenGuard::open(self.hwnd)?;
        unsafe {
            EmptyClipboard().context("could not empty the clipboard")?;
            // Delayed rendering returns a NULL handle, which the binding
            // reports as an error; only a real error code is one.
            SetLastError(WIN32_ERROR(0));
            if let Err(err) = SetClipboardData(CF_UNICODETEXT, None) {
                if err.code().is_err() {
                    return Err(err).context("could not offer the text");
                }
            }
            for (name, value) in [
                (w!("ExcludeClipboardContentFromMonitorProcessing"), 0u32),
                (w!("CanIncludeInClipboardHistory"), 0),
                (w!("CanUploadToCloudClipboard"), 0),
            ] {
                let format = RegisterClipboardFormatW(name);
                if format != 0 {
                    if let Ok(memory) = global(&value.to_le_bytes()) {
                        let _ = SetClipboardData(format, Some(HANDLE(memory.0)));
                    }
                }
            }
        }
        Ok(())
    }
}

unsafe extern "system" fn clipboard_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // An app is reading the text; the clipboard is already open.
        WM_RENDERFORMAT if wparam.0 as u32 == CF_UNICODETEXT => {
            render();
            LRESULT(0)
        }
        // FastTTS is exiting while it still owns the offer.
        WM_RENDERALLFORMATS => {
            if OpenClipboard(Some(hwnd)).is_ok() {
                if GetClipboardOwner().is_ok_and(|owner| owner == hwnd) {
                    render();
                }
                let _ = CloseClipboard();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn render() {
    let rendered = OFFER.with_borrow(|offer| {
        let units = offer.as_ref()?;
        let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        let memory = global(&bytes).ok()?;
        unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))) }.ok()
    });
    if rendered.is_some() {
        TAKEN.set(Some(unsafe { GetClipboardSequenceNumber() }));
    }
}

/// Process this thread's messages (clipboard requests) for up to `limit`.
fn pump(limit: Duration) {
    unsafe {
        MsgWaitForMultipleObjects(None, false, limit.as_millis() as u32, QS_ALLINPUT);
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

/// Holds the clipboard open; another app may have it briefly, so retry.
struct OpenGuard;

impl OpenGuard {
    fn open(hwnd: HWND) -> Result<Self> {
        for _ in 0..25 {
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                return Ok(Self);
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err(anyhow!("the clipboard is busy"))
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

fn global(bytes: &[u8]) -> Result<HGLOBAL> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let target = GlobalLock(memory) as *mut u8;
        if target.is_null() {
            let _ = GlobalFree(Some(memory));
            bail!("could not lock clipboard memory");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        let _ = GlobalUnlock(memory);
        Ok(memory)
    }
}

/// Every memory-backed format on the clipboard, so it can be put back.
fn snapshot(hwnd: HWND) -> Option<Vec<(u32, Vec<u8>)>> {
    let _open = OpenGuard::open(hwnd).ok()?;
    let mut saved = Vec::new();
    let mut total = 0;
    let mut format = 0;
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if HANDLE_FORMATS.contains(&format) {
            continue;
        }
        let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
            continue;
        };
        unsafe {
            let memory = HGLOBAL(handle.0);
            let size = GlobalSize(memory);
            let source = GlobalLock(memory) as *const u8;
            if source.is_null() {
                continue;
            }
            total += size;
            if total <= SNAPSHOT_LIMIT {
                saved.push((format, std::slice::from_raw_parts(source, size).to_vec()));
            }
            let _ = GlobalUnlock(memory);
        }
        if total > SNAPSHOT_LIMIT {
            return None;
        }
    }
    Some(saved)
}

fn restore(hwnd: HWND, saved: &[(u32, Vec<u8>)]) {
    let Ok(_open) = OpenGuard::open(hwnd) else {
        return;
    };
    unsafe {
        let _ = EmptyClipboard();
        for (format, data) in saved {
            if let Ok(memory) = global(data) {
                if SetClipboardData(*format, Some(HANDLE(memory.0))).is_err() {
                    let _ = GlobalFree(Some(memory));
                }
            }
        }
    }
}

fn clear(hwnd: HWND) {
    if let Ok(_open) = OpenGuard::open(hwnd) {
        let _ = unsafe { EmptyClipboard() };
    }
}

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn send(events: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        bail!("Windows blocked the input (an app running as administrator cannot receive it)");
    }
    Ok(())
}

fn send_ctrl_v() -> Result<()> {
    let none = KEYBD_EVENT_FLAGS(0);
    send(&[
        key(VK_CONTROL, 0, none),
        key(VK_V, 0, none),
        key(VK_V, 0, KEYEVENTF_KEYUP),
        key(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ])
}

fn type_text(text: &str, delay_ms: u64) -> Result<()> {
    let none = KEYBD_EVENT_FLAGS(0);
    let mut events = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        match ch {
            '\n' => events.extend([key(VK_RETURN, 0, none), key(VK_RETURN, 0, KEYEVENTF_KEYUP)]),
            '\t' => events.extend([key(VK_TAB, 0, none), key(VK_TAB, 0, KEYEVENTF_KEYUP)]),
            '\r' => {}
            _ => {
                let mut units = [0u16; 2];
                for &unit in ch.encode_utf16(&mut units).iter() {
                    events.push(key(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
                    events.push(key(
                        VIRTUAL_KEY(0),
                        unit,
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    ));
                }
            }
        }
    }
    if delay_ms == 0 {
        return send(&events);
    }
    for pair in events.chunks(2) {
        send(pair)?;
        thread::sleep(Duration::from_millis(delay_ms));
    }
    Ok(())
}

/// These tests use the real Windows clipboard (and put it back), so they
/// run on request: `cargo test --release -- --ignored clipboard`.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn read_clipboard_text() -> Option<String> {
        unsafe {
            OpenClipboard(None).ok()?;
            let text = GetClipboardData(CF_UNICODETEXT).ok().map(|handle| {
                let memory = HGLOBAL(handle.0);
                let units = GlobalSize(memory) / 2;
                let source = GlobalLock(memory) as *const u16;
                let text = std::slice::from_raw_parts(source, units);
                let end = text.iter().position(|&u| u == 0).unwrap_or(units);
                let text = String::from_utf16_lossy(&text[..end]);
                let _ = GlobalUnlock(memory);
                text
            });
            let _ = CloseClipboard();
            text
        }
    }

    fn set_marker(injector: &Injector, marker: &str) {
        injector.offer(marker).unwrap();
        // Render it at once so the marker is plain clipboard text.
        drop(read_clipboard_text());
        OFFER.set(None);
    }

    /// An app that reads the clipboard a whole second after Ctrl+V, far
    /// later than the old fixed 400 ms, still gets the text, and the
    /// user's clipboard comes back afterwards.
    #[test]
    #[ignore]
    fn clipboard_slow_reader_gets_the_text() {
        let injector = Injector::new().unwrap();
        let original = read_clipboard_text();
        set_marker(&injector, "USER-CLIPBOARD");
        let (tx, rx) = mpsc::channel();
        injector
            .paste("dictated text", Duration::from_secs(3), || {
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(1000));
                    tx.send(read_clipboard_text()).unwrap();
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(rx.recv().unwrap().as_deref(), Some("dictated text"));
        assert_eq!(read_clipboard_text().as_deref(), Some("USER-CLIPBOARD"));
        if let Some(original) = original {
            set_marker(&injector, &original);
        }
    }

    /// When no app reads the offer, the paste reports failure (so the text
    /// gets typed instead) and the user's clipboard is put back.
    #[test]
    #[ignore]
    fn clipboard_unread_paste_fails_and_restores() {
        let injector = Injector::new().unwrap();
        let original = read_clipboard_text();
        set_marker(&injector, "USER-CLIPBOARD");
        let result = injector.paste("dictated text", Duration::from_millis(300), || Ok(()));
        assert!(result.is_err());
        assert_eq!(read_clipboard_text().as_deref(), Some("USER-CLIPBOARD"));
        if let Some(original) = original {
            set_marker(&injector, &original);
        }
    }
}
