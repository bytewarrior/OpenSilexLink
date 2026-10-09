#!/usr/bin/env python3
"""Compile all ``.po`` under ``src/opensilexlink/locale`` to ``.mo`` (pure Python).

Usable without an external ``msgfmt`` (also on Windows). Run::

    python scripts/compile_translations.py
"""

from __future__ import annotations

import glob
import os
import struct
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOCALE_DIR = os.path.join(ROOT, "src", "opensilexlink", "locale")


def _unescape(s: str) -> str:
    return (s.replace(r"\n", "\n").replace(r"\t", "\t")
             .replace(r'\"', '"').replace(r"\\", "\\"))


def parse_po(path: str) -> dict[str, str]:
    """Very small .po parser: msgid/msgstr (incl. multi-line "..." continuations)."""
    entries: dict[str, str] = {}
    key = val = None
    target = None                       # "id" or "str"
    with open(path, "r", encoding="utf-8") as fh:
        for raw in fh:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            if line.startswith("msgid "):
                if key is not None:
                    entries[key] = val or ""
                key = _unescape(line[6:].strip().strip('"'))
                val = ""
                target = "id"
            elif line.startswith("msgstr "):
                val = _unescape(line[7:].strip().strip('"'))
                target = "str"
            elif line.startswith('"'):
                chunk = _unescape(line.strip().strip('"'))
                if target == "id":
                    key += chunk
                elif target == "str":
                    val += chunk
        if key is not None:
            entries[key] = val or ""
    return entries


def write_mo(entries: dict[str, str], path: str) -> None:
    # Keep the header entry msgid "" (metadata); drop empty translations.
    items = sorted((k, v) for k, v in entries.items() if v or k == "")
    keys = [k.encode("utf-8") for k, _ in items]
    vals = [v.encode("utf-8") for _, v in items]
    n = len(items)
    offsets_o, offsets_t = [], []
    # The string data area starts after the header (28) + two tables (8*n each).
    base = 28 + 16 * n
    data = bytearray()
    for k in keys:
        offsets_o.append((len(k), base + len(data)))
        data += k + b"\x00"
    for v in vals:
        offsets_t.append((len(v), base + len(data)))
        data += v + b"\x00"
    out = bytearray()
    out += struct.pack("<Iiiiiii", 0x950412de, 0, n, 28, 28 + 8 * n, 0, 0)
    for length, off in offsets_o:
        out += struct.pack("<ii", length, off)
    for length, off in offsets_t:
        out += struct.pack("<ii", length, off)
    out += data
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as fh:
        fh.write(out)


def main() -> int:
    pos = glob.glob(os.path.join(LOCALE_DIR, "*", "LC_MESSAGES", "*.po"))
    if not pos:
        print(f"No .po found under {LOCALE_DIR}.")
        return 1
    for po in pos:
        mo = po[:-3] + ".mo"
        entries = parse_po(po)
        write_mo(entries, mo)
        print(f"{po} -> {mo} ({len(entries)} entries)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
