//! Text insertion: atomic clipboard paste first, tagged Unicode SendInput second.

use anyhow::{anyhow, Result};
use clipboard_win::{formats, get_clipboard, set_clipboard};
use std::{mem::size_of, thread, time::Duration};
use windows::Win32::{
    UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_RETURN, VK_V,
    },
};

use crate::hook::INJECT_TAG;

fn key(vk: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn unicode(unit: u16, up: bool) -> INPUT {
    let flags = if up {
        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
    } else {
        KEYEVENTF_UNICODE
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn send(events: &[INPUT]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        return Err(anyhow!(
            "SendInput was blocked (Windows does not let an unelevated app type into an elevated window)"
        ));
    }
    Ok(())
}

fn send_ctrl_v() -> Result<()> {
    send(&[
        key(VK_CONTROL.0, KEYBD_EVENT_FLAGS(0)),
        key(VK_V.0, KEYBD_EVENT_FLAGS(0)),
        key(VK_V.0, KEYEVENTF_KEYUP),
        key(VK_CONTROL.0, KEYEVENTF_KEYUP),
    ])
}

fn send_unicode_text(text: &str, delay_ms: u64) -> Result<()> {
    let mut events: Vec<INPUT> = Vec::with_capacity(text.len() * 4);
    for ch in text.chars() {
        if ch == '\n' {
            events.push(key(VK_RETURN.0, KEYBD_EVENT_FLAGS(0)));
            events.push(key(VK_RETURN.0, KEYEVENTF_KEYUP));
        } else if ch == '\t' {
            events.push(key(0x09, KEYBD_EVENT_FLAGS(0)));
            events.push(key(0x09, KEYEVENTF_KEYUP));
        } else {
            let mut buffer = [0u16; 2];
            for unit in ch.encode_utf16(&mut buffer) {
                events.push(unicode(*unit, false));
                events.push(unicode(*unit, true));
            }
        }
    }
    if delay_ms == 0 {
        return send(&events);
    }
    for chunk in events.chunks(2) {
        send(chunk)?;
        thread::sleep(Duration::from_millis(delay_ms));
    }
    Ok(())
}

fn paste(text: &str, pause_ms: u64) -> Result<()> {
    let previous: String = get_clipboard(formats::Unicode).unwrap_or_default();
    if set_clipboard(formats::Unicode, text).is_err() {
        return Err(anyhow!("could not put the transcript on the clipboard"));
    }
    let visible: String = get_clipboard(formats::Unicode).unwrap_or_default();
    if visible != text {
        return Err(anyhow!("the clipboard did not accept the transcript"));
    }
    thread::sleep(Duration::from_millis(80));
    send_ctrl_v()?;
    thread::sleep(Duration::from_millis(pause_ms));
    let current: String = get_clipboard(formats::Unicode).unwrap_or_default();
    if current == text {
        let _ = set_clipboard(formats::Unicode, &previous);
    }
    Ok(())
}

/// Insert text at the caret. Clipboard paste is atomic and is the default;
/// SendInput is the fallback for apps that ignore Ctrl+V.
pub fn insert_text(
    text: &str,
    method: &str,
    char_delay_ms: u64,
    paste_delay_ms: u64,
    append_space: bool,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut prepared = text.to_string();
    if append_space && !prepared.ends_with(&[' ', '\n', '\t'][..]) {
        prepared.push(' ');
    }
    match method {
        "clipboard" => {
            if let Err(err) = paste(&prepared, paste_delay_ms) {
                tracing::warn!("clipboard paste failed ({err}); typing instead");
                send_unicode_text(&prepared, char_delay_ms.max(10))?;
            }
            Ok(())
        }
        "sendinput" => {
            if let Err(err) = send_unicode_text(&prepared, char_delay_ms) {
                tracing::warn!("SendInput failed ({err}); pasting instead");
                paste(&prepared, paste_delay_ms)?;
            }
            Ok(())
        }
        "none" => Ok(()),
        other => Err(anyhow!("unknown injection method '{other}'")),
    }
}
