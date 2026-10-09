"""Tests for the XDG autostart entry (gui/autostart.py) - Qt-free."""

from pathlib import Path

import pytest

from opensilexlink.gui import autostart  # noqa: E402


@pytest.fixture
def xdg(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    return tmp_path


def test_path_follows_xdg(xdg):
    assert autostart.desktop_path() == str(xdg / "autostart" / "silex-usb-gui.desktop")


def test_enable_writes_valid_entry(xdg):
    path = autostart.enable(hidden=True)
    text = Path(path).read_text(encoding="utf-8")
    assert text.startswith("[Desktop Entry]")
    assert "Type=Application" in text
    assert "\nExec=" in text and "--hidden" in text
    assert "X-GNOME-Autostart-enabled=true" in text
    assert autostart.is_enabled() is True


def test_enable_without_hidden(xdg):
    autostart.enable(hidden=False)
    text = Path(autostart.desktop_path()).read_text(encoding="utf-8")
    assert "--hidden" not in text


def test_disable_is_idempotent(xdg):
    autostart.disable()                 # no file yet -> no error
    autostart.enable()
    assert autostart.is_enabled() is True
    autostart.disable()
    assert autostart.is_enabled() is False
    assert not Path(autostart.desktop_path()).exists()


def test_sync_toggles(xdg):
    autostart.sync(True, hidden=True)
    assert autostart.is_enabled() is True
    autostart.sync(False, hidden=True)
    assert autostart.is_enabled() is False


def test_is_enabled_false_when_flagged_off(xdg):
    Path(autostart.autostart_dir()).mkdir(parents=True)
    Path(autostart.desktop_path()).write_text(
        "[Desktop Entry]\nType=Application\nExec=x\nX-GNOME-Autostart-enabled=false\n",
        encoding="utf-8")
    assert autostart.is_enabled() is False


def test_resolve_exec_fallback_to_module(xdg, monkeypatch):
    monkeypatch.setattr(autostart.shutil, "which", lambda _: None)
    cmd = autostart.resolve_exec(hidden=True)
    assert "-m opensilexlink.gui.app" in cmd and cmd.endswith("--hidden")
