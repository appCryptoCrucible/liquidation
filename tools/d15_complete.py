#!/usr/bin/env python3
"""Legacy entrypoint — delegates to tools/d15/regenerate.py."""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))

from d15.regenerate import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
