"""Tests for the IPC codec, the IPC socket transport, and the settings schema.

The socket transport is tested over AF_INET loopback so the tests also run on Windows
(no AF_UNIX). The daemon itself is the Rust ``silex-daemon`` and is tested separately;
here we only cover the client-side wire codec, the reference server, and settings.
"""

import threading

import pytest

from opensilexlink import ipc  # noqa: E402
from opensilexlink.settings import Settings, SettingsStore, DeviceRule  # noqa: E402


# --------------------------------------------------------------------------- #
#  Codec
# --------------------------------------------------------------------------- #
def test_encode_is_single_line():
    data = ipc.encode(ipc.make_request(1, "ping", {"a": 1}))
    assert data.endswith(b"\n")
    assert data.count(b"\n") == 1


def test_line_decoder_splits_and_buffers():
    dec = ipc.LineDecoder()
    a = ipc.encode({"id": 1, "method": "x"})
    b = ipc.encode({"id": 2, "method": "y"})
    stream = a + b
    # feed in two arbitrary chunks -> both messages must come out
    out = list(dec.feed(stream[:5]))
    out += list(dec.feed(stream[5:]))
    assert [m["id"] for m in out] == [1, 2]


def test_decoder_ignores_blank_lines():
    dec = ipc.LineDecoder()
    assert list(dec.feed(b"\n\n")) == []


# --------------------------------------------------------------------------- #
#  Socket transport (AF_INET, works on Windows)
# --------------------------------------------------------------------------- #
@pytest.fixture
def echo_server():
    srv = ipc.IpcServer(("127.0.0.1", 0))

    def handler(method, params, peer):
        if method == "echo":
            return params
        if method == "boom":
            raise ValueError("broken")
        if method == "sub":
            peer.subscribed = True
            return {"ok": True}
        raise ValueError("unknown")

    srv.set_handler(handler)
    srv.start()
    yield srv
    srv.stop()


def test_request_response_roundtrip(echo_server):
    with ipc.IpcClient(echo_server.bound_address) as cli:
        assert cli.call("echo", {"hello": "world"}) == {"hello": "world"}


def test_error_response_raises(echo_server):
    with ipc.IpcClient(echo_server.bound_address) as cli:
        with pytest.raises(ipc.IpcError, match="broken"):
            cli.call("boom")


def test_broadcast_reaches_subscribers(echo_server):
    events = []
    ev = threading.Event()

    def on_event(name, data):
        events.append((name, data))
        ev.set()

    with ipc.IpcClient(echo_server.bound_address, on_event=on_event) as cli:
        cli.call("sub")
        echo_server.broadcast("tick", {"n": 1})
        assert ev.wait(2.0)
    assert events == [("tick", {"n": 1})]


# --------------------------------------------------------------------------- #
#  Settings
# --------------------------------------------------------------------------- #
def test_settings_roundtrip(tmp_path):
    store = SettingsStore(str(tmp_path / "settings.json"))
    store.settings.refresh_interval = 25
    store.settings.device_rules.append(
        DeviceRule(server_mac="00:11:22:33:44:55", key="1234:5678:USB Flash Memory",
                   label="Good stick", auto_connect=True))
    store.save()

    reloaded = SettingsStore(str(tmp_path / "settings.json"))
    assert reloaded.settings.refresh_interval == 25
    assert reloaded.settings.device_rules[0].auto_connect is True


def test_settings_clamp_refresh():
    s = Settings.from_dict({"refresh_interval": 999})
    assert s.refresh_interval == 60
    s = Settings.from_dict({"refresh_interval": 1})
    assert s.refresh_interval == 3


def test_settings_ignores_unknown_keys():
    s = Settings.from_dict({"refresh_interval": 10, "future_field": 42})
    assert s.refresh_interval == 10


def test_settings_defaults_tray_behavior():
    # Default: closing minimizes to the tray, disconnecting asks for confirmation.
    s = Settings()
    assert s.hide_on_close is True
    assert s.confirm_disconnect is True
    # overridable and persisted in the dict
    assert Settings.from_dict({"hide_on_close": False}).hide_on_close is False


def test_should_auto_connect_prefers_rule():
    s = Settings(auto_connect_new=False)
    s.device_rules.append(DeviceRule("MAC", "KEY", auto_connect=True))
    assert s.should_auto_connect("MAC", "KEY") is True      # the rule wins
    assert s.should_auto_connect("MAC", "OTHER") is False   # global fallback
