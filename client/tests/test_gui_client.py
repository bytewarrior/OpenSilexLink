"""Tests for the Qt-free GUI logic (gui/client.py) - without PySide6.

The default language is English (the source language), so these tests expect the
English strings.
"""

import threading

import pytest

from opensilexlink import ipc  # noqa: E402
from opensilexlink.gui import client as gc  # noqa: E402


# --------------------------------------------------------------------------- #
#  Display helpers
# --------------------------------------------------------------------------- #
def test_status_text_states():
    assert gc.status_text({"status": "available"}) == "Available"
    assert gc.status_text({"status": "connected"}) == "Connected by me"
    assert gc.status_text(
        {"status": "in_use", "owner_host": "host-a"}) == "In use by host-a"
    # without a hostname -> IP
    assert gc.status_text(
        {"status": "in_use", "owner_ip": "192.0.2.99"}) == "In use by 192.0.2.99"


def test_server_label():
    assert gc.server_label({"hostname": "usbserver", "mac": "00:11:22:33:44:55",
                            "model": "DS-510"}) == "usbserver [00:11:22:33:44:55 DS-510]"
    assert gc.server_label({"ip": "192.0.2.143"}) == "192.0.2.143"


def test_can_connect_disconnect():
    assert gc.can_connect({"status": "available"}) is True
    assert gc.can_connect({"status": "connected"}) is False
    assert gc.can_disconnect({"status": "connected"}) is True
    assert gc.can_disconnect({"status": "in_use"}) is False


def test_can_request():
    assert gc.can_request({"status": "in_use"}) is True
    assert gc.can_request({"status": "available"}) is False
    assert gc.can_request({"status": "connected"}) is False


def test_request_status_text():
    assert gc.request_status_text({"state": "denied"}) == "The request was denied."
    assert "taken over" in gc.request_status_text({"state": "connected"}).lower()
    assert gc.request_status_text({"state": "error", "note": "boom"}).endswith("(boom)")
    assert gc.request_status_text({"state": "nonsense"}) == ""


def test_visible_servers_filter():
    servers = [{"mac": "00:11:22:33:44:55"}, {"mac": "AA:BB:CC:DD:EE:FF"}]
    assert gc.visible_servers(servers, False, []) == servers          # off -> all
    only = gc.visible_servers(servers, True, ["00:11:22:33:44:55"])   # case-insensitive
    assert [s["mac"] for s in only] == ["00:11:22:33:44:55"]
    assert gc.visible_servers(servers, True, []) == []                # nothing checked


def test_resolve_columns_and_labels():
    assert gc.resolve_columns(None) == gc.DEFAULT_COLUMNS
    assert gc.resolve_columns([]) == gc.DEFAULT_COLUMNS
    assert gc.resolve_columns(["nonsense"]) == gc.DEFAULT_COLUMNS      # only invalid -> default
    assert gc.resolve_columns(["mac", "name", "x"]) == ["mac", "name"]
    assert gc.column_labels(["mac", "status"]) == ["Ethernet address", "Status"]


def test_display_name_prefers_alias():
    assert gc.display_name({"description": "USB Flash Memory"}) == "USB Flash Memory"
    assert gc.display_name({"description": "USB Flash Memory", "alias": "My stick"}) == "My stick"
    assert gc.display_name({"description": "X", "alias": ""}) == "X"      # empty alias -> description
    assert gc.display_name({}) == ""


def test_device_cell_uses_alias():
    dev = {"description": "USB Flash Memory", "alias": "My stick"}
    assert gc.device_cell(dev, {}, "name") == "My stick"


def test_device_cell_values():
    dev = {"description": "USB Flash Memory", "type": "Mass storage device", "status": "connected"}
    srv = {"hostname": "usbserver", "model": "DS-510", "ip": "192.0.2.143",
           "mac": "00:11:22:33:44:55"}
    assert gc.device_cell(dev, srv, "name") == "USB Flash Memory"
    assert gc.device_cell(dev, srv, "type") == "Mass storage device"
    assert gc.device_cell(dev, srv, "status") == "Connected by me"
    assert gc.device_cell(dev, srv, "hostname") == "usbserver"
    assert gc.device_cell(dev, srv, "model") == "DS-510"
    assert gc.device_cell(dev, srv, "ip") == "192.0.2.143"
    assert gc.device_cell(dev, srv, "ipv6") == ""
    assert gc.device_cell(dev, srv, "mac") == "00:11:22:33:44:55"


def test_device_icon_candidates():
    assert gc.device_icon_candidates({"usb_class": 0x08})[0] == "drive-removable-media-usb"
    assert gc.device_icon_candidates({"usb_class": 0x03})[0] == "input-mouse"
    assert gc.device_icon_candidates({"usb_class": 0x07})[0] == "printer"
    assert gc.device_icon_candidates({"usb_class": 0xff})[0] == "application-x-firmware"
    # every device ends with generic USB icons -> never the Qt hard-disk fallback
    for cls in (0x08, 0xff, 0x42):
        assert "drive-removable-media-usb" in gc.device_icon_candidates({"usb_class": cls})
    assert gc.device_icon_candidates({})[0] == "drive-removable-media-usb"   # unknown


def test_connected_keys():
    servers = [{"mac": "M", "devices": [
        {"dev_id": "dev01", "status": "connected"},
        {"dev_id": "dev02", "status": "available"}]}]
    assert gc.connected_keys(servers) == {("M", "dev01")}


def test_apps_to_launch_only_new_connected_with_app():
    servers = [{"mac": "M", "devices": [
        {"dev_id": "dev01", "status": "connected", "launch_app": "/bin/foo",
         "disconnect_on_app_close": True},
        {"dev_id": "dev02", "status": "connected", "launch_app": ""},        # no app
        {"dev_id": "dev03", "status": "available", "launch_app": "/bin/bar"}]}]
    # dev01 is newly connected + has an app -> launch; dev02 no app; dev03 not connected
    out = gc.apps_to_launch(set(), servers)
    assert out == [{"mac": "M", "dev_id": "dev01", "app": "/bin/foo",
                    "disconnect_on_app_close": True}]
    # if dev01 was already connected before (baseline) -> do not launch again
    assert gc.apps_to_launch({("M", "dev01")}, servers) == []


def test_server_web_url():
    assert gc.server_web_url({"ip": "192.0.2.143"}) == "http://192.0.2.143"
    assert gc.server_web_url({"ip": ""}) == ""
    assert gc.server_web_url({}) == ""


def test_tray_device_status():
    assert gc.tray_device_status({"status": "available"}) == "Not connected"
    assert gc.tray_device_status({"status": "connected"}) == "Connected"
    assert gc.tray_device_status(
        {"status": "in_use", "owner_host": "host-a"}) == "In use by host-a"
    assert gc.tray_device_status(
        {"status": "in_use", "owner_ip": "192.0.2.9"}) == "In use by 192.0.2.9"


def test_connected_count_and_tray_summary():
    empty = [{"devices": [{"status": "available"}, {"status": "in_use"}]}]
    assert gc.connected_count(empty) == 0
    assert gc.tray_summary(empty) == "No device connected"
    one = [{"devices": [{"status": "connected"}, {"status": "available"}]}]
    assert gc.tray_summary(one) == "1 device connected"
    two = [{"devices": [{"status": "connected"}]},
           {"devices": [{"status": "connected"}, {"status": "in_use"}]}]
    assert gc.connected_count(two) == 2
    assert gc.tray_summary(two) == "2 devices connected"
    assert gc.tray_summary([]) == "No device connected"


def test_event_routing_dispatches_by_name():
    reqs, stats, notes = [], [], []
    cli = gc.DaemonClient(None, on_access_request=reqs.append,
                          on_access_status=stats.append, on_notice=notes.append)
    cli._on_event("access_request", {"requester": "X"})
    cli._on_event("access_request_status", {"state": "sent"})
    cli._on_event("notice", {"state": "lost"})
    cli._on_event("servers", {"servers": []})       # without on_servers -> no crash
    assert reqs == [{"requester": "X"}]
    assert stats == [{"state": "sent"}]
    assert notes == [{"state": "lost"}]


def test_notice_text_and_error_flag():
    assert gc.notice_text({"state": "reconnected"}) == "Reconnected automatically."
    assert gc.notice_text(
        {"state": "lost", "description": "USB Flash Memory"}
    ) == "USB Flash Memory: Connection lost - reconnecting ..."
    assert gc.notice_text(
        {"state": "removed", "description": "USB Flash Memory"}
    ) == "USB Flash Memory: The device was removed - connection closed."
    assert gc.notice_text({"state": "nonsense"}) == ""
    assert gc.notice_is_error({"state": "failed"}) is True
    assert gc.notice_is_error({"state": "taken"}) is True
    assert gc.notice_is_error({"state": "reconnected"}) is False
    assert gc.notice_is_error({"state": "lost"}) is False


# --------------------------------------------------------------------------- #
#  DaemonClient against a fake daemon (IpcServer)
# --------------------------------------------------------------------------- #
@pytest.fixture
def fake_daemon():
    srv = ipc.IpcServer(("127.0.0.1", 0))
    state = {"servers": [{"mac": "M", "devices": [{"dev_id": "dev01", "status": "available"}]}]}
    calls = []

    def handler(method, params, peer):
        calls.append((method, params))
        if method == "subscribe":
            peer.subscribed = True
            peer.send_event("servers", {"servers": state["servers"]})
            return {"subscribed": True}
        if method == "list_servers":
            return {"servers": state["servers"]}
        if method == "connect":
            return {"connected": True, "port": 0}
        if method == "disconnect":
            return {"disconnected": True}
        if method == "request_access":
            return {"requested": True, "owner_ip": "192.0.2.1"}
        if method == "respond_access_request":
            return {"ok": True, "accepted": params.get("accept")}
        raise ValueError("unknown")

    srv.set_handler(handler)
    srv.start()
    srv._calls = calls
    yield srv
    srv.stop()


def test_daemonclient_subscribe_fires_callback(fake_daemon):
    got = []
    ev = threading.Event()

    def on_servers(servers):
        got.append(servers)
        ev.set()

    cli = gc.DaemonClient(fake_daemon.bound_address, on_servers=on_servers).connect()
    try:
        cli.subscribe()
        assert ev.wait(2.0)
    finally:
        cli.close()
    assert got[0][0]["mac"] == "M"


def test_daemonclient_connect_disconnect(fake_daemon):
    cli = gc.DaemonClient(fake_daemon.bound_address).connect()
    try:
        assert cli.connect_device("M", "dev01")["connected"] is True
        assert cli.disconnect_device("M", "dev01")["disconnected"] is True
    finally:
        cli.close()
    methods = [m for m, _ in fake_daemon._calls]
    assert "connect" in methods and "disconnect" in methods


def test_daemonclient_request_and_respond(fake_daemon):
    cli = gc.DaemonClient(fake_daemon.bound_address).connect()
    try:
        assert cli.request_access("M", "dev01")["requested"] is True
        res = cli.respond_access_request("M", "dev01", False)
        assert res["accepted"] is False
    finally:
        cli.close()
    methods = [m for m, _ in fake_daemon._calls]
    assert "request_access" in methods and "respond_access_request" in methods
