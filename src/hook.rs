//! Low-level Caps Lock hook.
//!
//! The hook lives on its own thread with its own message loop, and its
//! callback only posts a message to the app window, so Windows never sees a
//! slow hook. Caps Lock is swallowed while dictation is active; injected key
//! events (including FastTTS's own) pass through untouched.

use anyhow::{anyhow, Result};
use std::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread::JoinHandle,
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
            KEYEVENTF_KEYUP, VK_CAPITAL,
        },
        WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, PostMessageW, PostThreadMessageW, SetWindowsHookExW,
            UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_UP, MSG,
            WH_KEYBOARD_LL, WM_APP, WM_QUIT,
        },
    },
};

/// `dwExtraInfo` tag on every key event FastTTS injects.
pub const INJECT_TAG: usize = 0x4641_5354;

/// Posted to the app window: `WPARAM(1)` = Caps Lock down, `WPARAM(0)` = up.
pub const WM_CAPSLOCK: u32 = WM_APP + 2;

static TARGET: AtomicUsize = AtomicUsize::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);

/// While paused, Caps Lock behaves like a normal key.
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Release);
}

/// Switch the Caps Lock light off if it is on. The hook swallows the key, so
/// the user could not do it themselves.
pub fn release_caps_lock() {
    if unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 == 0 {
        return;
    }
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_CAPITAL,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    };
    let events = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let ours = info.vkCode == VK_CAPITAL.0 as u32
            && !info.flags.contains(LLKHF_INJECTED)
            && info.dwExtraInfo != INJECT_TAG
            && !PAUSED.load(Ordering::Acquire);
        if ours {
            let target = TARGET.load(Ordering::Acquire);
            if target != 0 {
                let down = !info.flags.contains(LLKHF_UP);
                let _ = PostMessageW(
                    Some(HWND(target as *mut c_void)),
                    WM_CAPSLOCK,
                    WPARAM(down as usize),
                    LPARAM(0),
                );
            }
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Owns the hook thread; dropping it removes the hook.
pub struct HotkeyHook {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl HotkeyHook {
    pub fn start(hwnd: HWND) -> Result<Self> {
        TARGET.store(hwnd.0 as usize, Ordering::Release);
        let (tx, rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let thread = std::thread::Builder::new()
            .name("fasttts-hook".into())
            .spawn(move || {
                let hook =
                    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) } {
                        Ok(hook) => hook,
                        Err(err) => {
                            let _ = tx.send(Err(err.to_string()));
                            return;
                        }
                    };
                let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
                let mut msg = MSG::default();
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {}
                let _ = unsafe { UnhookWindowsHookEx(hook) };
            })?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(anyhow!("could not install the Caps Lock hook: {err}"))
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("the Caps Lock hook thread stopped"))
            }
        }
    }
}

impl Drop for HotkeyHook {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        TARGET.store(0, Ordering::Release);
    }
}
