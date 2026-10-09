"""Minimal bilingual support (English/German) via Python ``gettext``.

The source strings in the code are **English** (= the ``msgid``), so English needs
no catalog. German is provided as a compiled catalog at
``locale/de/LC_MESSAGES/silex.mo`` (source: the sibling ``.po``, compiled with
``scripts/compile_translations.py``).

Usage: ``from .i18n import _`` and wrap strings in ``_("...")``. For strings with
values use format placeholders instead of f-strings::

    _("Connecting {} ...").format(dev_id)

The language is chosen once at startup from the system locale
(:func:`set_language_from_locale`); without a call it stays at the English source.
"""

from __future__ import annotations

import gettext
import os

DOMAIN = "silex"
_LOCALEDIR = os.path.join(os.path.dirname(__file__), "locale")

# Default: no translation -> gettext returns the msgid (= English).
_translation: gettext.NullTranslations = gettext.NullTranslations()
_language = "en"


def available_languages() -> tuple[str, ...]:
    return ("en", "de")


def set_language(lang: str | None) -> str:
    """Set the active language. ``en`` = source (no catalog), ``de`` = German catalog.

    Unknown/empty locales fall back to English (the source language). Returns the
    language that was actually set."""
    global _translation, _language
    code = (lang or "").strip().lower()[:2]
    if code == "de":
        try:
            _translation = gettext.translation(DOMAIN, _LOCALEDIR, languages=["de"])
        except FileNotFoundError:
            # No catalog present -> fall back to the English source.
            _translation = gettext.NullTranslations()
        _language = "de"
        return "de"
    _translation = gettext.NullTranslations()
    _language = "en"
    return "en"


def set_language_from_locale(locale_name: str | None) -> str:
    """Choose the language from a locale name (e.g. ``de_DE``, ``en_US``, ``fr_FR``)."""
    return set_language(locale_name)


def current_language() -> str:
    return _language


def _(text: str) -> str:
    """Translate *text* into the active language (empty strings unchanged)."""
    return _translation.gettext(text) if text else text
