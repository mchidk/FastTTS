//! System-tray icon and popup menu, built directly on `Shell_NotifyIconW`.
//!
//! The application owns the hidden message window and its `GetMessageW` loop;
//! this module only registers an icon against that window and displays a popup
//! menu when asked.  Menu selections are posted back to the window as
//! `WM_COMMAND` messages with the `TRAY_MENU_*` ids.

use std::cell::Cell;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, LoadImageW, PostMessageW,
    SetForegroundWindow, TrackPopupMenu, HICON, HMENU, IMAGE_ICON, LR_LOADFROMFILE, MF_DISABLED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND,
};

/// Command ids posted to the owner window as `WM_COMMAND`.
pub const TRAY_MENU_PAUSE: u16 = 2;
pub const TRAY_MENU_LOG: u16 = 3;
pub const TRAY_MENU_QUIT: u16 = 4;

const ICON_ID: u32 = 1;
const STATUS_ITEM_ID: usize = 1;

/// Message sent to the owner window for tray icon events (`WM_APP + 1`).
/// `LPARAM` holds the event (`WM_RBUTTONUP`, `WM_LBUTTONUP`, ...).
pub const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;

/// Owns the tray icon, its tooltip and its popup menu.
///
/// Intended to live on the application's GUI thread (Win32 window handles and
/// menus are thread-affine).
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    data: NOTIFYICONDATAW,
    menu: HMENU,
    status: String,
    paused: bool,
    removed: Cell<bool>,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy `text` into a fixed-size, zero-filled Win32 string buffer.
fn write_fixed(dst: &mut [u16], text: &str) {
    dst.fill(0);
    for (slot, unit) in dst.iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
    }
}

impl Tray {
    /// Load `icon_path` (an `.ico` file) and register it in the tray.
    pub fn new(hwnd: HWND, icon_path: &Path) -> Result<Self> {
        if hwnd.0.is_null() {
            bail!("Tray::new needs a non-null window handle");
        }
        if !icon_path.is_file() {
            bail!("tray icon not found: {}", icon_path.display());
        }

        let path_wide: Vec<u16> = icon_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            LoadImageW(
                None,
                PCWSTR::from_raw(path_wide.as_ptr()),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE,
            )
        }
        .with_context(|| format!("could not load tray icon {}", icon_path.display()))?;
        let icon = HICON(handle.0);

        let status = "FastTTS - ready".to_string();
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = ICON_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = TRAY_CALLBACK_MESSAGE;
        data.hIcon = icon;
        write_fixed(&mut data.szTip, "FastTTS - ready");

        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            let _ = unsafe { DestroyIcon(icon) };
            bail!("Shell_NotifyIconW(NIM_ADD) failed");
        }

        let mut tray = Self {
            hwnd,
            icon,
            data,
            menu: HMENU::default(),
            status,
            paused: false,
            removed: Cell::new(false),
        };
        if let Err(err) = tray.rebuild_menu() {
            tray.remove();
            return Err(err);
        }
        Ok(tray)
    }

    /// Update the tooltip text and rebuild the popup menu.
    ///
    /// `status` is supplied by the application (for example "Listening..." or
    /// "Paused - Caps Lock works normally").  When `paused` is true the menu
    /// offers "Resume dictation" instead of "Pause dictation".
    pub fn set_status(&mut self, status: &str, paused: bool) {
        self.status = status.to_string();
        self.paused = paused;

        write_fixed(&mut self.data.szTip, status);
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.data) }.as_bool() {
            tracing::warn!("Shell_NotifyIconW(NIM_MODIFY) failed");
        }

        if let Err(err) = self.rebuild_menu() {
            tracing::warn!("could not rebuild tray menu: {err}");
        }
    }

    /// Show the popup menu at the current cursor position.
    ///
    /// The selected command is posted to the owner window as `WM_COMMAND` with
    /// one of the `TRAY_MENU_*` ids (0 if the menu was dismissed).
    pub fn show_menu(&self) {
        if self.removed.get() || self.menu.0.is_null() {
            return;
        }

        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }

        // Required so the menu is dismissed correctly when focus moves away.
        let _ = unsafe { SetForegroundWindow(self.hwnd) };

        let selection = unsafe {
            TrackPopupMenu(
                self.menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD,
                cursor.x,
                cursor.y,
                Some(0),
                self.hwnd,
                None,
            )
        };

        if selection.0 != 0 {
            let _ = unsafe {
                PostMessageW(
                    Some(self.hwnd),
                    WM_COMMAND,
                    WPARAM(selection.0 as usize),
                    LPARAM(0),
                )
            };
        }
    }

    /// Remove the icon and free the icon, menu and tooltip resources.
    ///
    /// Safe to call more than once.
    pub fn remove(&self) {
        if self.removed.replace(true) {
            return;
        }

        if !self.data.hWnd.0.is_null() {
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data) };
        }
        if !self.menu.0.is_null() {
            let _ = unsafe { DestroyMenu(self.menu) };
        }
        if !self.icon.0.is_null() {
            let _ = unsafe { DestroyIcon(self.icon) };
        }
    }

    fn rebuild_menu(&mut self) -> Result<()> {
        unsafe {
            if !self.menu.0.is_null() {
                let _ = DestroyMenu(self.menu);
            }
            let menu = CreatePopupMenu().context("CreatePopupMenu failed")?;

            // Disabled, non-selectable status line.
            let status = wide(&self.status);
            let _ = AppendMenuW(
                menu,
                MF_STRING | MF_DISABLED | MF_GRAYED,
                STATUS_ITEM_ID,
                PCWSTR::from_raw(status.as_ptr()),
            );

            let pause_label = if self.paused {
                "Resume dictation"
            } else {
                "Pause dictation"
            };
            let pause = wide(pause_label);
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                TRAY_MENU_PAUSE as usize,
                PCWSTR::from_raw(pause.as_ptr()),
            );

            let log = wide("Open log");
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                TRAY_MENU_LOG as usize,
                PCWSTR::from_raw(log.as_ptr()),
            );

            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

            let quit = wide("Quit");
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                TRAY_MENU_QUIT as usize,
                PCWSTR::from_raw(quit.as_ptr()),
            );

            self.menu = menu;
            Ok(())
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}
