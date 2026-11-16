"""Windowless launcher used by the FastTTS desktop shortcut."""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "src"))

from fasttts.app import main  # noqa: E402  (path setup must come first)

if __name__ == "__main__":
    raise SystemExit(main())
