# FastTTS

**Hold Caps Lock. Talk. Let go. The words appear.**

FastTTS is a small English dictation app for Windows. It lives in the system
tray, listens while you hold Caps Lock, turns your speech into text on your
own PC, and types it wherever your cursor is.

Everything runs locally. There is no window to manage, no account and no
cloud. The whole app is a single `fasttts.exe` with the speech model inside.

![FastTTS while listening](docs/assets/hud-listening.png)
![FastTTS while transcribing](docs/assets/hud-transcribing.png)
![FastTTS when nothing was heard](docs/assets/hud-error.png)

## Why it feels instant

- **Streaming.** While you are still talking, every finished phrase is
  already being transcribed. Letting go of Caps Lock only waits for the last
  few seconds, typically well under half a second.
- **Small, fast model.** [Cactus Whistle](https://huggingface.co/Cactus-Compute/whistle)
  is a 16.9 MB English speech model that runs on the CPU at roughly 16 times
  real time.
- **No lost first word.** The microphone stays open and keeps the last
  250 ms, so a take starts slightly before you press the key.
- **Native.** One Rust executable, no runtime to start, no DLLs beside it.

## Install

You need Windows 11 (x64) and, to build it:

- [Rust](https://rustup.rs) 1.88 or newer with the `x86_64-pc-windows-gnullvm` target
- [LLVM MinGW](https://github.com/mstorsjo/llvm-mingw) (UCRT) with its `bin`
  folder on `PATH`, for example `winget install MartinStorsjo.LLVM-MinGW.UCRT`

```powershell
rustup target add x86_64-pc-windows-gnullvm
cargo build --release
.\install.ps1
```

`install.ps1` copies `fasttts.exe` to `%LOCALAPPDATA%\Programs\FastTTS`,
adds Desktop and Start menu shortcuts and starts the app. Add `-Startup` to
start FastTTS when you sign in, or run `.\install.ps1 -Uninstall` to remove it.

The first build downloads the Cactus engine library and checks its SHA-256.

## Use

1. Click where the text should go.
2. Hold **Caps Lock** and speak English.
3. Let go.

The text is pasted at the cursor followed by a space. Fillers such as "um"
and "uh", stutters and repeated words are removed, and sentences start with a
capital letter.

The tray icon shows the current state. Its menu can **pause** dictation
(Caps Lock then works as a normal key again), open the **log folder** or
**quit**.

## Settings

Create `%APPDATA%\FastTTS\config.toml` to change anything. Every key is
optional; these are the defaults:

```toml
cleanup = true             # remove fillers and stutters, fix capitals
chunk_seconds = 6.0        # longest piece of continuous speech per model call
silence_ms = 500           # a pause this long ends a phrase
min_hold_ms = 250          # shorter Caps Lock taps are ignored
max_record_seconds = 300   # a take stops by itself after this long
hud = true                 # the oscilloscope above the taskbar
inject = "clipboard"       # "clipboard", "sendinput" (typed) or "none"
char_delay_ms = 10         # pause between typed characters for "sendinput"
paste_delay_ms = 400       # wait before the clipboard is restored
append_space = true        # add a space after each take
mic_device = ""            # microphone name; empty means the Windows default
```

A typo or an out-of-range value stops FastTTS with a message that names it.
Restart FastTTS after changing the file.

## Good to know

- **Privacy.** Audio and text never leave your PC. The microphone stays open
  for the pre-roll, so Windows shows its microphone indicator while FastTTS
  runs. The log records timings and text lengths, never what you said.
- **Your clipboard is kept.** Pasting borrows the clipboard for a moment and
  then puts back what was on it before.
- **Administrator windows.** Windows does not let a normal app type into a
  program that runs as administrator. The HUD then shows INPUT BLOCKED.
- **Microphone unplugged?** FastTTS keeps trying to reconnect every two
  seconds and shows NO MICROPHONE if you press Caps Lock meanwhile.

## How it works

```
Caps Lock hook -> microphone -> 16 kHz mono -> phrase chunker -> Whistle model
                                                                     |
               paste at cursor <- cleanup <- join phrases <----------+
```

The keyboard hook, the HUD, the model and text insertion each run on their
own thread, so the key never waits for the model and the model never waits
for the screen.

## Development

```powershell
cargo test --release
```

The tests cover the resampler, chunker, cleanup, settings and HUD drawing,
and run the real model on a recorded sentence in `tests/fixtures`, both whole
and streamed. To redraw the images in this README from the HUD code:

```powershell
cargo test --release readme_images -- --ignored
```

## License

FastTTS is MIT licensed. Cactus Whistle and its engine are Apache-2.0; see
[NOTICE](NOTICE).
