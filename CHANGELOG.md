# Changelog

## 1.0.3 - 2026-11-18

### Fixed

- Dictation sometimes pasted nothing in Windows 11 Notepad. Notepad reads
  the clipboard asynchronously, and the clipboard was put back after a
  fixed 400 ms, possibly before Notepad had read the text. The text is now
  offered with delayed rendering: Windows reports when the app actually
  reads it, and only then is the previous clipboard restored. If no app
  reads it within `paste_timeout_ms` (2 s), the text is typed instead.
- Dictated text no longer appears in Windows clipboard history or cloud
  clipboard.

### Changed

- `paste_delay_ms` is replaced by `paste_timeout_ms`.

## 1.0.2 - 2026-11-18

### Fixed

- Takes often sent nothing. Speech had to be louder than a fixed -42 dBFS
  to count, and soft voices on laptop microphone arrays sit around -48 dBFS,
  so whole takes were thrown away as silence. The voice threshold now
  follows the room's noise level (down to -56 dBFS on quiet, processed
  microphones; never stricter than before on noisy ones).
- The oscilloscope ranges itself to your voice, so soft and loud
  microphones both fill the bars.
- The log records each take's loudest level, voice threshold and voiced
  time, so a missed take can be diagnosed.

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
