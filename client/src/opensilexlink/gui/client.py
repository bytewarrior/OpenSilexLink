"""Qt-free binding to the daemon plus display helpers (testable without PySide6)."""

from __future__ import annotations

from typing import Any, Callable

from .. import ipc
from ..i18n import _

# Daemon status values (mirror of the daemon's STATUS_*).
STATUS_AVAILABLE = "available"
STATUS_CONNECTED = "connected"
STATUS_IN_USE = "in_use"


def status_text(device: dict) -> str:
    """Human-readable status text like in SX Virtual Link."""
    st = device.get("status")
    if st == STATUS_CONNECTED:
        return _("Connected by me")
    if st == STATUS_IN_USE:
        who = device.get("owner_host") or device.get("owner_ip") or "?"
        return _("In use by {}").format(who)
    return _("Available")


def server_label(server: dict) -> str:
    """Tree label, e.g. 'usbserver [00:11:22:33:44:55 Silex device server]'."""
    host = server.get("hostname") or server.get("ip") or "?"
    mac = (server.get("mac") or "").upper()
    model = server.get("model") or ""
    inner = " ".join(x for x in (mac, model) if x)
    return f"{host} [{inner}]" if inner else host


def can_connect(device: dict) -> bool:
    return device.get("status") == STATUS_AVAILABLE


def can_disconnect(device: dict) -> bool:
    return device.get("status") == STATUS_CONNECTED


def can_request(device: dict) -> bool:
    """"Request use" is only offered for devices held by another host."""
    return device.get("status") == STATUS_IN_USE


#: Status texts for the request side (access_request_status) for the status bar.
REQUEST_STATUS_TEXT = {
    "sent": "Request sent ...",
    "acked": "Request delivered - awaiting decision ...",
    "accepted": "The other side agreed - releasing the device ...",
    "denied": "The request was denied.",
    "released": "Device released - taking over ...",
    "connecting": "Connecting ...",
    "connected": "Device taken over.",
    "no_response": "No response from the other side.",
    "error": "Take-over failed.",
}


def request_status_text(data: dict) -> str:
    base = _(REQUEST_STATUS_TEXT.get(data.get("state", ""), ""))
    note = data.get("note")
    return f"{base} ({note})" if note and base else (base or note or "")


#: Connection notices (notice event, kind=reconnect) -> display text.
NOTICE_TEXT = {
    "lost": "Connection lost - reconnecting ...",
    "reconnected": "Reconnected automatically.",
    "failed": "Automatic reconnect failed.",
    "taken": "The device was taken over by another machine.",
    "disconnected": "Connection lost.",
    # Device disappeared at the device server (unplugged) -> vhci device cleaned up
    "removed": "The device was removed - connection closed.",
    # Peer auto-release (auto_disconnect_on_request + grace timer)
    "auto_release_pending": "Request from another machine - releasing the device automatically ...",
    "auto_released": "Device handed over to another machine automatically.",
}


def notice_text(data: dict) -> str:
    """Text for a notice event; prepends the device name if present."""
    base = _(NOTICE_TEXT.get(data.get("state", ""), ""))
    desc = data.get("description")
    return f"{desc}: {base}" if desc and base else base


def notice_is_error(data: dict) -> bool:
    """True for error notices (for the "notify on error" setting)."""
    return data.get("state") in ("failed", "taken", "disconnected")


# Columns of the USB device list (options tab 3 "Display"); key -> header label.
COLUMN_LABELS = {
    "name": "Name", "type": "Type", "status": "Status", "hostname": "Hostname",
    "model": "Model", "ip": "IP address", "ipv6": "IPv6 address", "mac": "Ethernet address",
}
DEFAULT_COLUMNS = ["name", "type", "status", "hostname", "model"]


def resolve_columns(columns: list[str] | None) -> list[str]:
    """Valid, known columns in order; fall back to the default columns."""
    cols = [c for c in (columns or []) if c in COLUMN_LABELS]
    return cols or list(DEFAULT_COLUMNS)


def column_labels(cols: list[str]) -> list[str]:
    return [_(COLUMN_LABELS[c]) for c in cols]


def display_name(device: dict) -> str:
    """Display name of a device: user-defined alias, otherwise the description."""
    d = device or {}
    return d.get("alias") or d.get("description", "")


def device_cell(device: dict, server: dict, col: str) -> str:
    """Cell value of a device for column *col* (device plus its server)."""
    d, s = device or {}, server or {}
    if col == "name":
        return display_name(d)
    if col == "type":
        return _(d.get("type", ""))
    if col == "status":
        return status_text(d)
    if col == "hostname":
        return s.get("hostname", "")
    if col == "model":
        return s.get("model", "")
    if col == "ip":
        return s.get("ip", "")
    if col == "ipv6":
        return s.get("ipv6", "")
    if col == "mac":
        return (s.get("mac", "") or "").upper()
    return ""


def visible_servers(servers: list[dict], filter_enabled: bool,
                    filter_macs: list[str]) -> list[dict]:
    """Restrict the server list by the display filter (options tab 4).

    Without an active filter the list is unchanged; otherwise only servers whose
    MAC is checked are kept."""
    if not filter_enabled:
        return list(servers)
    allowed = {(m or "").lower() for m in (filter_macs or [])}
    return [s for s in servers if (s.get("mac", "") or "").lower() in allowed]


# USB base class -> candidate list of freedesktop icon names (specific -> generic),
# device-class dependent like SX Virtual Link. The first name the desktop theme knows
# is used; every device ends with a generic USB icon so the misleading Qt hard-disk
# fallback never kicks in just because a name is missing from the theme.
_GENERIC_USB_ICONS = ["drive-removable-media-usb", "media-flash", "drive-removable-media"]
USB_CLASS_ICONS = {
    0x01: ["audio-card"],                        # Audio
    0x02: ["modem", "network-wired"],            # Communications
    0x03: ["input-mouse", "input-tablet"],       # HID
    0x06: ["scanner"],                           # Imaging/Scanner
    0x07: ["printer"],                           # Printer
    0x08: ["drive-removable-media-usb", "media-flash", "drive-harddisk"],  # Mass storage
    0x0b: ["auth-smartcard", "smartcard"],       # Smart card
    0x0e: ["camera-web", "camera-photo"],        # Video
    0xe0: ["network-wireless"],                  # Wireless
    0xff: ["application-x-firmware", "application-x-executable"],  # Vendor-specific
}
SERVER_ICON = "network-server"       # device server


def device_icon_candidates(device: dict) -> list[str]:
    """freedesktop icon names for the device, from most specific to generic."""
    names = list(USB_CLASS_ICONS.get(device.get("usb_class", -1), []))
    for g in _GENERIC_USB_ICONS:
        if g not in names:
            names.append(g)
    return names


def server_web_url(server: dict) -> str:
    """Web configuration URL of the device server (http://<ip>), empty if no IP."""
    ip = (server or {}).get("ip", "").strip()
    return f"http://{ip}" if ip else ""


def tray_device_status(device: dict) -> str:
    """Short status for the tray menu row (like SX Virtual Link: '(Not connected)')."""
    st = device.get("status")
    if st == STATUS_CONNECTED:
        return _("Connected")
    if st == STATUS_IN_USE:
        who = device.get("owner_host") or device.get("owner_ip") or "?"
        return _("In use by {}").format(who)
    return _("Not connected")


def connected_keys(servers: list[dict]) -> set:
    """Set of currently connected devices as (mac, dev_id)."""
    return {(srv.get("mac", ""), dev.get("dev_id", ""))
            for srv in servers for dev in srv.get("devices", [])
            if dev.get("status") == STATUS_CONNECTED}


def apps_to_launch(prev_connected: set, servers: list[dict]) -> list[dict]:
    """Newly connected devices with a launch_app set -> applications to launch.

    "New" = connected now but not in the previous state *prev_connected*. This way
    nothing is launched on the first render (baseline), and only once per connect."""
    out = []
    for srv in servers:
        for dev in srv.get("devices", []):
            key = (srv.get("mac", ""), dev.get("dev_id", ""))
            if (dev.get("status") == STATUS_CONNECTED and key not in prev_connected
                    and dev.get("launch_app")):
                out.append({
                    "mac": key[0], "dev_id": key[1], "app": dev["launch_app"],
                    "disconnect_on_app_close": bool(dev.get("disconnect_on_app_close")),
                })
    return out


def connected_count(servers: list[dict]) -> int:
    """Number of currently connected devices across all servers."""
    return sum(1 for srv in servers for dev in srv.get("devices", [])
               if dev.get("status") == STATUS_CONNECTED)


def tray_summary(servers: list[dict]) -> str:
    """Tooltip/subtitle for the tray icon, e.g. '1 device connected'."""
    n = connected_count(servers)
    if n == 0:
        return _("No device connected")
    return (_("{} device connected") if n == 1
            else _("{} devices connected")).format(n)


class DaemonClient:
    """Thin, typed wrapper around :class:`ipc.IpcClient` for the GUI.

    ``on_servers`` is called (on the IPC reader thread!) with the server list, both
    in response to ``subscribe`` and on every change. The GUI must marshal the
    callback into the Qt thread.
    """

    def __init__(self, address: str | tuple[str, int] | None = None,
                 on_servers: Callable[[list[dict]], None] | None = None,
                 on_disconnect: Callable[[], None] | None = None,
                 on_access_request: Callable[[dict], None] | None = None,
                 on_access_status: Callable[[dict], None] | None = None,
                 on_notice: Callable[[dict], None] | None = None):
        self._on_servers = on_servers
        self._on_disconnect = on_disconnect
        self._on_access_request = on_access_request
        self._on_access_status = on_access_status
        self._on_notice = on_notice
        self._cli = ipc.IpcClient(address, on_event=self._on_event)

    def _on_event(self, event: str, data: Any) -> None:
        if event == "servers" and self._on_servers:
            self._on_servers((data or {}).get("servers", []))
        elif event == "access_request" and self._on_access_request:
            self._on_access_request(data or {})
        elif event == "access_request_status" and self._on_access_status:
            self._on_access_status(data or {})
        elif event == "notice" and self._on_notice:
            self._on_notice(data or {})

    def connect(self) -> "DaemonClient":
        self._cli.connect()
        return self

    def subscribe(self) -> list[dict]:
        """Subscribe to changes; the initial state arrives as a servers event."""
        self._cli.call("subscribe")
        return self.list_servers()

    def list_servers(self) -> list[dict]:
        return self._cli.call("list_servers").get("servers", [])

    def refresh(self) -> list[dict]:
        return self._cli.call("refresh").get("servers", [])

    def connect_device(self, mac: str, dev_id: str) -> dict:
        return self._cli.call("connect", {"mac": mac, "dev_id": dev_id}, timeout=120.0)

    def disconnect_device(self, mac: str, dev_id: str) -> dict:
        return self._cli.call("disconnect", {"mac": mac, "dev_id": dev_id}, timeout=60.0)

    def get_settings(self) -> dict:
        return self._cli.call("get_settings")

    def set_settings(self, patch: dict) -> dict:
        return self._cli.call("set_settings", patch)

    def set_device_rule(self, patch: dict) -> dict:
        """patch carries mac, key and the rule fields to set."""
        return self._cli.call("set_device_rule", patch)

    def request_access(self, mac: str, dev_id: str) -> dict:
        """Request a device held by another host from the holding machine."""
        return self._cli.call("request_access", {"mac": mac, "dev_id": dev_id})

    def respond_access_request(self, mac: str, dev_id: str, accept: bool) -> dict:
        """Answer an incoming request (yes/no) for a device we hold."""
        return self._cli.call("respond_access_request",
                              {"mac": mac, "dev_id": dev_id, "accept": accept})

    def close(self) -> None:
        self._cli.close()
