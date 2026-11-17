//! Low-level Caps Lock hook for hold-to-talk dictation.
//!
//! A `WH_KEYBOARD_LL` hook is installed on a dedicated thread that pumps a
//! `GetMessageW` loop (Windows requires the installing thread to keep pumping
//! messages for a low-level hook to stay alive).  The hook procedure itself is
//! deliberately tiny: it only posts a notification to the application window
//! and returns immediately.  All real work (audio capture, transcription,
//! injection) happens on other threads.
//!
//! Injected keystrokes (including our own Unicode injection) are ignored so
//! that injecting text can never re-trigger the hotkey.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{bail, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_CAPITAL;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_UP, MSG, WH_KEYBOARD_LL,
    WM_APP, WM_QUIT,
};

/// `dwExtraInfo` tag for synthetic keystrokes produced by the text injector.
///
/// The injector tags its `SendInput` events with this value; the hook skips any
/// event carrying it (or the `LLKHF_INJECTED` flag) so our own typed text never
/// toggles dictation.
pub const INJECT_TAG: usize = 0x46415354;

/// Window message posted on a Caps Lock transition.
///
/// `WPARAM(1)` = key down, `WPARAM(0)` = key up, `LPARAM(0)`.
pub const WM_CAPSLOCK: u32 = WM_APP + 2;

// Globals: a `HOOKPROC` is a plain `extern "system" fn` and cannot capture a
// closure, so the target window and the pause flag are published atomically.
// Both are written before the hook thread starts and cleared only after the
// thread has been joined, which keeps the raw pointer in PAUSED_PTR valid for
// the whole time the callback can run.
static TARGET_HWND: AtomicUsize = AtomicUsize::new(0);
static PAUSED_PTR: AtomicUsize = AtomicUsize::new(0);

/// The hook procedure. Must return quickly; Windows silently unhooks a
/// low-level hook whose callback takes too long (roughly one second).
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code as u32 == HC_ACTION {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

        if info.vkCode == VK_CAPITAL.0 as u32
            && !info.flags.contains(LLKHF_INJECTED)
            && info.dwExtraInfo != INJECT_TAG
        {
            let paused_ptr = PAUSED_PTR.load(Ordering::Relaxed);
            let paused = if paused_ptr == 0 {
                false
            } else {
                (*(paused_ptr as *const AtomicBool)).load(Ordering::Relaxed)
            };

            if paused {
                // Let Caps Lock behave normally while dictation is paused.
                return CallNextHookEx(None, code, wparam, lparam);
            }

            let hwnd_bits = TARGET_HWND.load(Ordering::Relaxed);
            if hwnd_bits != 0 {
                let down = !info.flags.contains(LLKHF_UP);
                let hwnd = HWND(hwnd_bits as *mut c_void);
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_CAPSLOCK,
                    WPARAM(if down { 1 } else { 0 }),
                    LPARAM(0),
                );
            }
            // Swallow the key so the keyboard Caps Lock state never toggles.
            return LRESULT(1);
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

type Ready = std::result::Result<u32, String>;

fn hook_thread(tx: std::sync::mpsc::Sender<Ready>, _keep_alive: Arc<AtomicBool>) {
    let thread_id = unsafe { GetCurrentThreadId() };

    let hhook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) };
    let hhook = match hhook {
        Ok(handle) => handle,
        Err(err) => {
            let _ = tx.send(Err(err.to_string()));
            return;
        }
    };

    // Installed; hand the thread id back so Drop can post WM_QUIT.
    let _ = tx.send(Ok(thread_id));

    let mut msg = MSG::default();
    loop {
        // GetMessageW returns > 0 for a message, 0 for WM_QUIT, < 0 on error.
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
    }

    let _ = unsafe { UnhookWindowsHookEx(hhook) };
}

/// Owns a global low-level Caps Lock hook and its dedicated thread.
pub struct HotkeyHook {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    // Keeps the AtomicBool that PAUSED_PTR points at alive.
    _paused: Arc<AtomicBool>,
}

impl HotkeyHook {
    /// Install the hook and post Caps Lock transitions to `hwnd`.
    ///
    /// `paused` is read on every key event; when it is `true` Caps Lock is
    /// passed through to the system instead of being swallowed.
    pub fn start(hwnd: HWND, paused: Arc<AtomicBool>) -> Result<Self> {
        if hwnd.0.is_null() {
            bail!("HotkeyHook::start needs a non-null window handle");
        }

        let paused_ptr = Arc::as_ptr(&paused) as usize;
        TARGET_HWND.store(hwnd.0 as usize, Ordering::SeqCst);
        PAUSED_PTR.store(paused_ptr, Ordering::SeqCst);

        let (tx, rx) = std::sync::mpsc::channel::<Ready>();
        let keep_alive = Arc::clone(&paused);
        let thread = std::thread::Builder::new()
            .name("fasttts-hotkey".into())
            .spawn(move || hook_thread(tx, keep_alive))
            .map_err(|e| anyhow::anyhow!("could not spawn hotkey thread: {e}"))?;

        let thread_id = match rx.recv() {
            Ok(Ok(id)) => id,
            Ok(Err(msg)) => {
                let _ = thread.join();
                TARGET_HWND.store(0, Ordering::SeqCst);
                PAUSED_PTR.store(0, Ordering::SeqCst);
                bail!("could not install Caps Lock hook: {msg}");
            }
            Err(err) => {
                let _ = thread.join();
                TARGET_HWND.store(0, Ordering::SeqCst);
                PAUSED_PTR.store(0, Ordering::SeqCst);
                bail!("hotkey thread stopped before installing the hook: {err}");
            }
        };

        Ok(Self {
            thread_id,
            thread: Some(thread),
            _paused: paused,
        })
    }
}

impl Drop for HotkeyHook {
    fn drop(&mut self) {
        // WM_QUIT ends the thread's GetMessageW loop; the thread then unhooks.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Safe to clear now: the callback only runs on the (now stopped) thread.
        TARGET_HWND.store(0, Ordering::SeqCst);
        PAUSED_PTR.store(0, Ordering::SeqCst);
    }
}
