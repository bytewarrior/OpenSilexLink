"""Shared test setup.

Makes ``src`` importable without an install and compiles the gettext catalogs so the
German ``.mo`` exists for the i18n tests (English is the source language, so no English
catalog is needed).
"""

import runpy
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))

# Compile .po -> .mo once at collection time (pure Python, no external msgfmt).
_compile = ROOT / "scripts" / "compile_translations.py"
if _compile.exists():
    try:
        runpy.run_path(str(_compile), run_name="__main__")
    except SystemExit:
        pass
