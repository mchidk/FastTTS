# FastTTS — Rust edition

**Hold Caps Lock. Talk. Let go. The words appear.**

This `dev` branch is a ground-up Rust rewrite of FastTTS. It is the same tiny
English dictation app as the Python `main` branch, but native: no Python
runtime, no PyTorch, no Hugging Face client, no CLI, no TTS.

## Why it is faster

- Native Rust binary: no interpreter startup and no import cascade.
- `whisper-rs` / `whisper.cpp` with a locally bundled `ggml-small.en` model,
  CPU-only int8, greedy decoding, English forced.
- Model loads once in the background; the tray appears immediately.
- Persistent microphone stream with a 250 ms pre-roll, so a take starts
  with the first word and no device-open latency.
- One Win32 message loop drives the tray, Caps Lock hook and HUD; transcription
  runs on its own worker thread and never blocks input.
- **Streaming transcription:** while you hold Caps Lock, complete phrases are
  transcribed as soon as you pause, with a 4-second maximum chunk. Partial
  transcripts are kept in memory, so releasing Caps Lock only has to finish the
  short tail — it feels nearly instant.

## The oscilloscope HUD

A small, click-through oscilloscope sits above the taskbar while you dictate.
It shows live microphone bars while listening, a scanning pulse while
transcribing, and a short error message when needed. It fades away when done.

![FastTTS listening HUD](docs/assets/hud-listening.png)
![FastTTS transcribing HUD](docs/assets/hud-transcribing.png)
![FastTTS error HUD](docs/assets/hud-error.png)

Set `hud = false` in the config to turn it off.

## Requirements

- Windows 11 x64.
- Rust stable with the `x86_64-pc-windows-msvc` toolchain (1.88+).
- Visual Studio 2022 Build Tools with **Desktop development with C++**.
- CMake and LLVM/libclang on `PATH` (needed to build `whisper.cpp` and its
  bindings).
- The bundled English model in `models/whisper.cpp/ggml-small.en.bin`
  (~488 MB). It ships with this checkout and is excluded from Git.
- The MinGW runtime DLLs in `runtime/` are copied next to the release exe by
  `install-shortcut.ps1`; FastTTS runs without MinGW on `PATH`.

## Build and run

```powershell
cargo build --release
.\target\x86_64-pc-windows-gnu\release\fasttts.exe
```

The desktop shortcut points to the release executable and uses
`assets/fasttts.ico`. To rebuild the shortcut after moving the checkout, run
`install-shortcut.ps1` (if present) or recreate it manually.

## Use

1. Put the cursor where the text should go.
2. Hold **Caps Lock** and speak English.
3. Release Caps Lock.

The transcript is pasted at the caret with a trailing space. The default
insertion method is an atomic clipboard paste, so the new Windows 11 Notepad
cannot drop key-up events and repeat characters. Set `inject = "sendinput"`
for apps that ignore `Ctrl+V`; FastTTS then types with tagged Unicode events
and falls back to paste if input is blocked.

Tray menu:

- **Pause/Resume** — Caps Lock behaves normally again while paused.
- **Open log** — opens `%LOCALAPPDATA%\FastTTS\logs`.
- **Quit** — stops the hook and closes the microphone.

## Configuration

Optional TOML at `%APPDATA%\FastTTS\config.toml`. Every key is optional.
Environment variables such as `FASTTTS_HUD=false` or `FASTTTS_MIC_DEVICE=2`
override individual keys.

```toml
model = ""                 # "" = the bundled ggml-small.en.bin
threads = 0                # 0 = choose automatically
initial_prompt = ""
cleanup = true
chunk_seconds = 4.0        # longest audio chunk while holding
silence_ms = 350           # flush a phrase after this much silence
min_chunk_seconds = 0.8    # shortest chunk worth transcribing
min_hold_ms = 250
max_record_seconds = 120
hot_mic = true
sounds = false
hud = true
inject = "clipboard"       # clipboard, sendinput or none
char_delay_ms = 10
paste_delay_ms = 400
append_space = true
mic_device = ""            # "" = Windows default
```

## Not included

- Speech-to-text for any language other than English.
- Text-to-speech, voices, Studio, emotion presets, audio files.
- A CLI, server, Hugging Face downloads, model management or update checks.
- Global hotkeys other than Caps Lock, or any floating window beyond the HUD.

## License

MIT for the Rust code. The whisper.cpp engine, the ggml English model and the
remaining crates keep their own licenses; see [NOTICE](NOTICE).
