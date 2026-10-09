"""Persistent settings shared with the daemon.

Stored as JSON at ``$XDG_CONFIG_HOME/silex/settings.json`` (fallback
``~/.config/silex/settings.json``). The schema mirrors the daemon's; the client
reads it (e.g. for the ``start_hidden`` bootstrap before the daemon is reachable)
and edits it through the daemon over IPC (``get_settings``/``set_settings``/
``set_device_rule``). The daemon owns the file on disk. Unknown fields in an
existing file are ignored on load.
"""

from __future__ import annotations

import json
import os
import threading
from dataclasses import asdict, dataclass, field, fields


def config_dir() -> str:
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(
        os.path.expanduser("~"), ".config")
    return os.path.join(base, "silex")


def config_path() -> str:
    return os.path.join(config_dir(), "settings.json")


@dataclass
class DeviceRule:
    """Per-device rule (properties dialog, like the vendor client).

    A device is identified stably by (server MAC, key). The key is preferably the
    serial number, otherwise ``idVendor:idProduct`` plus the description. The
    fields mirror the SX Virtual Link properties (Connect / Disconnect tabs).
    """

    server_mac: str
    key: str                       # stable device key
    label: str = ""                # last seen description
    alias: str = ""                # user-defined display name
    auto_connect: bool = False     # connect automatically when available
    reconnect_on_error: bool = True    # reconnect automatically after a connection error
    launch_app: str = ""           # application to launch on connect (path)
    disconnect_on_app_close: bool = False
    auto_disconnect_on_request: bool = False   # release when another host requests it
    auto_disconnect_timeout: int = 120         # seconds


@dataclass
class SearchAddress:
    """Manual unicast search address (options tab 2), for NAPT/routing cases."""

    address: str
    port: int | None = None


@dataclass
class Settings:
    # -- Tab 1: General ---------------------------------------------------- #
    autostart: bool = False
    start_hidden: bool = False
    notify_errors: bool = True
    confirm_disconnect: bool = True
    auto_connect_new: bool = False          # global: auto-connect newly discovered devices
    hide_on_close: bool = True              # closing the window -> tray (instead of quitting)
    refresh_interval: int = 10              # 3..60 s
    # -- Tab 2: Search for device servers ---------------------------------- #
    search_enabled: bool = False
    search_addresses: list[SearchAddress] = field(default_factory=list)
    # -- Tab 3: Display ---------------------------------------------------- #
    columns: list[str] = field(default_factory=lambda: [
        "name", "type", "status", "hostname", "model"])
    column_widths: dict[str, int] = field(default_factory=dict)
    # -- Tab 4: Filter ----------------------------------------------------- #
    filter_enabled: bool = False
    filter_servers: list[str] = field(default_factory=list)   # allowed servers (MAC)
    # -- Per-device rules (auto-connect etc.) ------------------------------ #
    device_rules: list[DeviceRule] = field(default_factory=list)

    RANGE_REFRESH = (3, 60)

    # -- (De)serialization ------------------------------------------------- #
    def to_dict(self) -> dict:
        d = asdict(self)
        d.pop("RANGE_REFRESH", None)
        return d

    @classmethod
    def from_dict(cls, data: dict) -> "Settings":
        known = {f.name for f in fields(cls)}
        kwargs = {k: v for k, v in data.items() if k in known}
        if "search_addresses" in kwargs:
            kwargs["search_addresses"] = [
                SearchAddress(**s) if isinstance(s, dict) else s
                for s in kwargs["search_addresses"]]
        if "device_rules" in kwargs:
            kwargs["device_rules"] = [
                DeviceRule(**r) if isinstance(r, dict) else r
                for r in kwargs["device_rules"]]
        s = cls(**kwargs)
        s.clamp()
        return s

    def clamp(self) -> None:
        lo, hi = self.RANGE_REFRESH
        self.refresh_interval = max(lo, min(hi, int(self.refresh_interval)))

    # -- Auto-connect helpers ---------------------------------------------- #
    def rule_for(self, server_mac: str, key: str) -> DeviceRule | None:
        for r in self.device_rules:
            if r.server_mac == server_mac and r.key == key:
                return r
        return None

    def should_auto_connect(self, server_mac: str, key: str) -> bool:
        rule = self.rule_for(server_mac, key)
        if rule is not None:
            return rule.auto_connect
        return self.auto_connect_new

    _RULE_FIELDS = {f.name for f in fields(DeviceRule)} - {"server_mac", "key"}

    def set_device_rule(self, server_mac: str, key: str, **updates) -> DeviceRule:
        """Create or update a per-device rule (upsert).

        Only known, non-``None`` fields are applied."""
        rule = self.rule_for(server_mac, key)
        if rule is None:
            rule = DeviceRule(server_mac=server_mac, key=key)
            self.device_rules.append(rule)
        for name, value in updates.items():
            if value is not None and name in self._RULE_FIELDS:
                setattr(rule, name, value)
        return rule


class SettingsStore:
    """Thread-safe load/save of the settings (atomic via temp+rename)."""

    def __init__(self, path: str | None = None):
        self.path = path or config_path()
        self._lock = threading.Lock()
        self._settings = self.load()

    @property
    def settings(self) -> Settings:
        return self._settings

    def load(self) -> Settings:
        try:
            with open(self.path, "r", encoding="utf-8") as fh:
                return Settings.from_dict(json.load(fh))
        except (FileNotFoundError, ValueError, json.JSONDecodeError):
            return Settings()

    def save(self, settings: Settings | None = None) -> None:
        with self._lock:
            if settings is not None:
                self._settings = settings
            self._settings.clamp()
            os.makedirs(os.path.dirname(self.path), exist_ok=True)
            tmp = self.path + ".tmp"
            with open(tmp, "w", encoding="utf-8") as fh:
                json.dump(self._settings.to_dict(), fh, indent=2, ensure_ascii=False)
            os.replace(tmp, self.path)

    def update(self, patch: dict) -> Settings:
        """Apply a partial dict to the current settings and save."""
        merged = self._settings.to_dict()
        merged.update(patch)
        self.save(Settings.from_dict(merged))
        return self._settings
