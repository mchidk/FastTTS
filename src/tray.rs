//! Tray icon with a status tooltip and a small menu.

use anyhow::{bail, Context, Result};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NOTIFYICONDATAW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
                GetSystemMetrics, LoadImageW, PostMessageW, SetForegroundWindow, TrackPopupMenu,
                HICON, IMAGE_ICON, LR_DEFAULTCOLOR, MF_GRAYED, MF_SEPARATOR, MF_STRING,
                SM_CXSMICON, SM_CYSMICON, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_NULL,
            },
        },
    },
};

/// Tray icon events arrive at the app window as this message.
pub const WM_TRAY: u32 = WM_APP + 1;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command {
    TogglePause,
    OpenLog,
    Quit,
}

/// Resource id of the icon embedded by `build.rs`.
const ICON_RESOURCE: usize = 1;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct Tray {
    data: NOTIFYICONDATAW,
    icon: HICON,
    status: String,
    paused: bool,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Result<Self> {
        let icon = unsafe {
            let instance = GetModuleHandleW(None)?;
            LoadImageW(
                Some(instance.into()),
                PCWSTR(ICON_RESOURCE as *const u16),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            )
        }
        .context("could not load the tray icon")?;
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: HICON(icon.0),
            ..Default::default()
        };
        write_tip(&mut data.szTip, "FastTTS");
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            let _ = unsafe { DestroyIcon(HICON(icon.0)) };
            bail!("could not add the tray icon");
        }
        Ok(Self {
            data,
            icon: HICON(icon.0),
            status: String::new(),
            paused: false,
        })
    }

    pub fn set_status(&mut self, status: &str, paused: bool) {
        self.paused = paused;
        if self.status == status {
            return;
        }
        self.status = status.to_string();
        write_tip(&mut self.data.szTip, &format!("FastTTS - {status}"));
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
        }
    }

    /// Show the menu at the cursor and return the chosen command.
    ///
    /// Takes no `self` borrow while the menu is open: Windows keeps
    /// dispatching messages to the app during `TrackPopupMenu`.
    pub fn menu(hwnd: HWND, status: &str, paused: bool) -> Option<Command> {
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let status = wide(status);
            let pause = wide(if paused {
                "Resume dictation"
            } else {
                "Pause dictation"
            });
            let log = wide("Open log folder");
            let quit = wide("Quit");
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(status.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, 1, PCWSTR(pause.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, 2, PCWSTR(log.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, 3, PCWSTR(quit.as_ptr()));

            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            // Required so the menu closes when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let choice = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                cursor.x,
                cursor.y,
                None,
                hwnd,
                None,
            );
            // Documented TrackPopupMenu quirk: nudge the window afterwards.
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            match choice.0 {
                1 => Some(Command::TogglePause),
                2 => Some(Command::OpenLog),
                3 => Some(Command::Quit),
                _ => None,
            }
        }
    }

    /// Add the icon again after Explorer restarted.
    pub fn restore(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &self.data);
        }
    }

    pub fn status(&self) -> (&str, bool) {
        (&self.status, self.paused)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            let _ = DestroyIcon(self.icon);
        }
    }
}

fn write_tip(dst: &mut [u16], text: &str) {
    dst.fill(0);
    let limit = dst.len() - 1;
    for (slot, unit) in dst[..limit].iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
    }
}
