# FastTTS

**Hold Caps Lock. Talk. Let go. The words appear.**

FastTTS is a tiny English dictation app for Windows. It sits in the system
tray, records while Caps Lock is held, transcribes locally with Cactus
**Whistle**, and types the result into whatever window has focus.

No window to manage. No CLI. No cloud. No text-to-speech.

## Why it is fast and lean

- **Cactus Whistle**: a 16.9 MB English-capable speech model, ~6× faster than
  Whisper base, running on the CPU in a single native engine.
- No whisper.cpp, no ggml files, no CMake/libclang build step, no 150–500 MB
  model. The whole STT dependency is one small crate and one 16.9 MB file.
- Native Rust binary: no interpreter startup.
- Persistent microphone stream with a 250 ms pre-roll, so a take starts with
  the first word.
- One Win32 message loop drives the tray, hook and HUD; transcription runs on
  its own worker thread.
- **Streaming chunks**: phrases are transcribed while you are still holding
  Caps Lock; release only finishes the short tail.

## The oscilloscope HUD

A small, click-through oscilloscope sits above the taskbar while you dictate.
It shows live microphone bars while listening, a scanning pulse while
transcribing, and a short error message when needed.

![FastTTS listening HUD](docs/assets/hud-listening.png)
![FastTTS transcribing HUD](docs/assets/hud-transcribing.png)
![FastTTS error HUD](docs/assets/hud-error.png)

Set `hud = false` in the config to turn it off.

## Requirements

- Windows 11 x64.
- Rust stable with the `x86_64-pc-windows-gnullvm` target (1.88+).
- LLVM MinGW (UCRT) toolchain for the Cactus native engine.
- The bundled `models/cactus/whistle.cact` (16.9 MB). Excluded from Git.

## Build and run

```powershell
rustup default stable-x86_64-pc-windows-gnullvm
rustup target add x86_64-pc-windows-gnullvm
cargo build --release
.\target\x86_64-pc-windows-gnullvm\release\fasttts.exe
```

`install-shortcut.ps1` copies the required runtime DLLs next to the exe and
creates/updates the desktop shortcut.

## Use

1. Put the cursor where the text should go.
2. Hold **Caps Lock** and speak English.
3. Release Caps Lock.

The transcript is pasted at the caret with a trailing space. The default
method is atomic clipboard paste; set `inject = "sendinput"` for apps that
ignore `Ctrl+V`.

Tray menu: **Pause/Resume**, **Open log**, **Quit**.

## Configuration

Optional TOML at `%APPDATA%\FastTTS\config.toml`; every key is optional.

```toml
model = ""                 # "" = the bundled Whistle model
cleanup = true
chunk_seconds = 4.0
silence_ms = 350
min_chunk_seconds = 0.8
min_hold_ms = 250
max_record_seconds = 120
hot_mic = true
sounds = false
hud = true
inject = "clipboard"
char_delay_ms = 10
paste_delay_ms = 400
append_space = true
mic_device = ""            # "" = Windows default
```

## Not included

- Speech-to-text for any language other than English selection.
- Text-to-speech, voices, Studio, emotion presets, audio files.
- A CLI, server, model management or update checks.
- Global hotkeys other than Caps Lock, or any window beyond the HUD.

## License

MIT for the Rust code. Cactus Whistle and its engine are Apache-2.0; see
[NOTICE](NOTICE).
