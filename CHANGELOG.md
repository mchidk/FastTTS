# Changelog

## 0.2.1 - 2026-11-17

### Added

- Streaming transcription: while Caps Lock is held, complete phrases are
  transcribed as soon as a short silence is detected, with a 4-second maximum
  chunk. Partial transcripts are kept in memory, so releasing Caps Lock only
  waits for the final tail instead of the whole utterance.
- `chunk_seconds`, `silence_ms` and `min_chunk_seconds` config keys.

## 0.2.0 - 2026-11-17

### Changed

- Complete Rust rewrite on the `dev` branch.
- Native `whisper-rs` / `whisper.cpp` engine with the bundled `ggml-small.en`
  model. No Python runtime, no PyTorch, no Hugging Face client.
- One Win32 message loop for the Caps Lock hook, tray icon and HUD; a separate
  worker thread handles transcription.
- Atomic clipboard paste by default with tagged Unicode SendInput fallback,
  preventing repeated-character corruption in Windows 11 Notepad.
- Rebuilt layered HUD with live microphone bars and clean flat PNG assets with
  no shadows or outlines.
- Desktop shortcut now launches the native release executable.

## 0.1.2 - 2026-11-16

### Changed

- FastTTS is silent by default. The beep heard after releasing Caps Lock was
  the transcript-captured cue, not a Caps Lock toggle sound. Set
  `sounds = true` to bring back the stop/error cues.

## 0.1.1 - 2026-11-16

### Fixed

- Corrupted or repeated characters in Windows 11 Notepad. Fast per-character
  SendInput at 1 ms could lose key-up events. Clipboard paste is now the
  default, verifies the clipboard, waits 400 ms and restores the previous
  clipboard only if it is still ours.
- SendInput remains available at 10 ms with automatic fallback.
- 76 offline tests.

## 0.1.0 - 2026-11-16

First release of the Python implementation: hold Caps Lock to dictate English,
with a bundled faster-whisper small.en model, tray app and oscilloscope HUD.
