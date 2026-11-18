# Changelog

## 1.0.1 - 2026-11-18

### Fixed

- FastTTS closed on the first dictation without pasting anything. The
  installed 1.0.0 exe was built before a text-cleanup dependency fix.
  `install.ps1` now builds the release itself, so the installed app always
  matches the source.
- Text cleanup is checked when FastTTS starts, and an unexpected error now
  shows a message instead of the app silently disappearing.

## 1.0.0 - 2026-11-18

First release.

- Hold Caps Lock to dictate English into any app; release to paste.
- Streaming transcription: phrases are transcribed while you speak, cut at
  pauses or at the quietest moment, so words are never split.
- Cactus Whistle speech model (16.9 MB) embedded in a single, self-contained
  `fasttts.exe` with no DLLs beside it.
- Any microphone format, resampled to 16 kHz with an anti-aliasing filter;
  automatic reconnect when the microphone disappears.
- Oscilloscope HUD on a decibel scale that follows normal speech levels,
  with clear messages for no speech, no microphone and blocked input.
- Clipboard paste that restores everything that was on the clipboard,
  with typed input as a fallback.
- Conservative cleanup of fillers, stutters and repeated words.
- Tray menu to pause, open the log folder or quit.
- `install.ps1` for a per-user install, autostart and uninstall.
