# Changelog

All notable changes to FastTTS are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions use
[Semantic Versioning](https://semver.org/).

## 0.1.0 - 2026-11-16

First release: a lean, English-only dictation app for Windows.

### Added

- Hold Caps Lock to dictate; release to transcribe and type at the caret.
- Self-contained English Whisper `small.en` model under
  `models/faster-whisper-small.en`; no external cache or runtime download is
  needed.
- English-only `small.en` at int8 with beam size 1 and VAD.
- Persistent microphone stream with a 250 ms pre-roll, so the first word is
  not lost and takes start instantly.
- Background model warm-up; the tray icon is available immediately.
- Conservative filler/stutter cleanup (`um`, `uh`, "I I think").
- Tray menu with status, Pause dictation, Open log and Quit.
- On-screen oscilloscope HUD with live microphone bars while listening, a
  scanning pulse while transcribing, and short error messages.
- README screenshots of the listening, transcribing and error HUD states.
- Short stop/error cues, optional via `sounds = false`.
- Single-instance guard and rotating file log.
- Clipboard fallback when SendInput is blocked, plus a `clipboard` mode and an
  `inject = "none"` test mode.
- 74 offline tests for hold-to-talk, recording buffers, cleanup, injection
  events, configuration, transcription arguments, app state and the HUD API.
