"""Tests for bilingual support (i18n, gettext). Also checks that the compiled German
catalog (silex.mo) is present and correct. English is the source language (msgid)."""

import pytest

from opensilexlink import i18n            # noqa: E402
from opensilexlink.gui import client as gc  # noqa: E402


@pytest.fixture(autouse=True)
def _reset_language():
    # After EVERY test in this file, restore English (the default source language) so
    # no global state leaks into other tests (which expect English strings).
    yield
    i18n.set_language("en")


def test_default_is_english():
    i18n.set_language("en")
    assert i18n._("Connect") == "Connect"
    assert gc.status_text({"status": "available"}) == "Available"


def test_german_catalog_loaded():
    assert i18n.set_language("de") == "de"
    assert i18n._("Connect") == "Verbinden"
    assert i18n._("Available") == "Verfügbar"
    assert i18n._("In use by {}").format("host-a") == "Wird von host-a verwendet"
    # also affects the client helpers
    assert gc.status_text({"status": "available"}) == "Verfügbar"
    assert gc.status_text({"status": "connected"}) == "Von mir verbunden"
    assert gc.column_labels(["mac", "type"]) == ["Ethernet-Adresse", "Typ"]
    assert gc.tray_summary([{"devices": [{"status": "connected"}]}]) == "1 Gerät verbunden"


def test_unknown_locale_falls_back_to_english():
    assert i18n.set_language("fr_FR") == "en"
    assert i18n._("Connect") == "Connect"


def test_empty_string_not_translated():
    i18n.set_language("de")
    assert i18n._("") == ""
