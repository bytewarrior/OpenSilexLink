"""Autostart via an XDG ``.desktop`` entry in ``~/.config/autostart``.

Qt-free and side-effect-free at import time, so it is testable without PySide6. The
desktop (GNOME/KDE/XFCE/Cinnamon ...) launches everything in
``$XDG_CONFIG_HOME/autostart`` at login. We place an entry there on demand that
starts the GUI - by default ``--hidden`` (tray only), like the vendor client.
"""

from __future__ import annotations

import os
import shutil
import sys

APP_ID = "silex-usb-gui"
APP_NAME = "Silex USB Device Server"


def autostart_dir() -> str:
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(
        os.path.expanduser("~"), ".config")
    return os.path.join(base, "autostart")


def desktop_path() -> str:
    return os.path.join(autostart_dir(), f"{APP_ID}.desktop")


def resolve_exec(hidden: bool = True) -> str:
    """Exec line: prefer the installed ``osl-gui`` script, else
    ``python -m opensilexlink.gui.app``. With *hidden* the GUI starts into the tray."""
    exe = shutil.which("osl-gui")
    cmd = exe if exe else f"{sys.executable} -m opensilexlink.gui.app"
    return f"{cmd} --hidden" if hidden else cmd


def build_desktop_entry(exec_cmd: str) -> str:
    return (
        "[Desktop Entry]\n"
        "Type=Application\n"
        f"Name={APP_NAME}\n"
        "Comment=USB devices over Ethernet (Silex USB device server)\n"
        f"Exec={exec_cmd}\n"
        "Icon=drive-removable-media-usb\n"
        "Terminal=false\n"
        "Categories=Utility;System;\n"
        "X-GNOME-Autostart-enabled=true\n"
    )


def is_enabled() -> bool:
    """True if the entry exists and is not explicitly disabled."""
    try:
        with open(desktop_path(), "r", encoding="utf-8") as fh:
            text = fh.read()
    except OSError:
        return False
    low = text.lower()
    if "x-gnome-autostart-enabled=false" in low or "\nhidden=true" in low:
        return False
    return True


def enable(hidden: bool = True) -> str:
    """Write the autostart entry (atomically) and return its path."""
    path = desktop_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(build_desktop_entry(resolve_exec(hidden)))
    os.replace(tmp, path)
    return path


def disable() -> None:
    """Remove the autostart entry (idempotent)."""
    try:
        os.remove(desktop_path())
    except OSError:
        pass


def sync(enabled: bool, hidden: bool = True) -> None:
    """Bring the autostart file in line with the setting."""
    if enabled:
        enable(hidden)
    else:
        disable()
