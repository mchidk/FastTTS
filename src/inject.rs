//! Put the transcript at the caret: clipboard paste first, typed Unicode second.

use crate::hook::INJECT_TAG;
use anyhow::{anyhow, Result};
use clipboard_win::{formats, raw, set_clipboard, Clipboard};
use std::{mem::size_of, thread, time::Duration};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_RETURN, VK_TAB, VK_V,
};

/// Clipboard formats held as GDI handles rather than memory; Windows
/// re-synthesises them from the memory formats we do save.
const HANDLE_FORMATS: &[u32] = &[2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E];
/// Do not snapshot clipboards larger than this.
const SNAPSHOT_LIMIT: usize = 64 << 20;

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
        return Err(anyhow!(
            "Windows blocked the input (an app running as administrator cannot receive it)"
        ));
    }
    Ok(())
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

/// Every memory-backed format on the clipboard, so it can be put back.
fn snapshot() -> Option<Vec<(u32, Vec<u8>)>> {
    let _open = Clipboard::new_attempts(10).ok()?;
    let mut saved = Vec::new();
    let mut total = 0;
    for format in raw::EnumFormats::new() {
        if HANDLE_FORMATS.contains(&format) {
            continue;
        }
        let mut data = Vec::new();
        if raw::get_vec(format, &mut data).is_ok() {
            total += data.len();
            if total > SNAPSHOT_LIMIT {
                return None;
            }
            saved.push((format, data));
        }
    }
    Some(saved)
}

fn restore(saved: &[(u32, Vec<u8>)]) {
    let Ok(_open) = Clipboard::new_attempts(10) else {
        return;
    };
    let _ = raw::empty();
    for (format, data) in saved {
        let _ = raw::set_without_clear(*format, data);
    }
}

fn paste(text: &str, settle_ms: u64) -> Result<()> {
    let saved = snapshot();
    set_clipboard(formats::Unicode, text)
        .map_err(|err| anyhow!("could not use the clipboard: {err}"))?;
    let ours = raw::seq_num();
    thread::sleep(Duration::from_millis(40));
    let none = KEYBD_EVENT_FLAGS(0);
    let pasted = send(&[
        key(VK_CONTROL, 0, none),
        key(VK_V, 0, none),
        key(VK_V, 0, KEYEVENTF_KEYUP),
        key(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ]);
    // Give the target app time to read the clipboard before restoring it,
    // and leave it alone if someone else has written to it meanwhile.
    if pasted.is_ok() {
        thread::sleep(Duration::from_millis(settle_ms));
    }
    if let Some(saved) = saved {
        if raw::seq_num() == ours {
            restore(&saved);
        }
    }
    pasted
}

/// Insert `text` at the caret with the configured method, falling back to
/// the other one if it fails.
pub fn insert(
    text: &str,
    method: &str,
    char_delay_ms: u64,
    paste_delay_ms: u64,
    append_space: bool,
) -> Result<()> {
    let mut text = text.to_string();
    if append_space && !text.ends_with(char::is_whitespace) {
        text.push(' ');
    }
    match method {
        "clipboard" => paste(&text, paste_delay_ms).or_else(|err| {
            tracing::warn!("paste failed ({err}); typing instead");
            type_text(&text, char_delay_ms.max(5))
        }),
        "sendinput" => type_text(&text, char_delay_ms).or_else(|err| {
            tracing::warn!("typing failed ({err}); pasting instead");
            paste(&text, paste_delay_ms)
        }),
        _ => Ok(()),
    }
}
