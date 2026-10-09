"""Local IPC between the OpenSilexLink daemon and the client (GUI / CLI).

Transport: a stream socket, either
  * **AF_UNIX** (default at runtime): a path, e.g. ``$XDG_RUNTIME_DIR/silex-daemon.sock``;
  * **AF_INET** (tests / platforms without Unix sockets, e.g. Windows dev): ``(host, port)``.

Protocol: **one JSON message per line** (newline-delimited JSON, UTF-8). This keeps the
codec purely byte-based and testable without a socket.

Message kinds
=============
Request  (client -> server)::  {"id": <int>, "method": <str>, "params": {...}}
Response (server -> client)::  {"id": <int>, "ok": true,  "result": {...}}
                               {"id": <int>, "ok": false, "error": {"message": ...}}
Event    (server -> client)::  {"event": <str>, "data": {...}}     (no "id")

A request with ``id=null`` is a notification with no expected response.

The client (:class:`IpcClient`) is what the GUI/CLI use. The server side
(:class:`IpcServer`) is kept here so the wire codec has a self-contained reference
implementation for tests; the production daemon is the Rust ``silex-daemon``.
"""

from __future__ import annotations

import json
import os
import socket
import threading
from dataclasses import dataclass
from typing import Any, Callable, Iterator


# --------------------------------------------------------------------------- #
#  Addressing
# --------------------------------------------------------------------------- #
def default_socket_path() -> str:
    """Default path of the daemon socket (XDG_RUNTIME_DIR, else /tmp)."""
    base = os.environ.get("XDG_RUNTIME_DIR") or "/tmp"
    return os.path.join(base, "silex-daemon.sock")


def _make_socket(address: str | tuple[str, int]) -> socket.socket:
    if isinstance(address, tuple):
        return socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if not hasattr(socket, "AF_UNIX"):
        raise RuntimeError("AF_UNIX is not available on this platform - use (host, port)")
    return socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)


# --------------------------------------------------------------------------- #
#  Codec (purely byte-based -> unit-testable without a socket)
# --------------------------------------------------------------------------- #
def encode(msg: dict) -> bytes:
    """Serialize a message to a single line (including the trailing ``\\n``)."""
    return (json.dumps(msg, separators=(",", ":"), ensure_ascii=False) + "\n").encode("utf-8")


def make_request(msg_id: int | None, method: str, params: dict | None = None) -> dict:
    return {"id": msg_id, "method": method, "params": params or {}}


def make_response(msg_id: int | None, result: Any) -> dict:
    return {"id": msg_id, "ok": True, "result": result}


def make_error(msg_id: int | None, message: str, **extra: Any) -> dict:
    return {"id": msg_id, "ok": False, "error": {"message": message, **extra}}


def make_event(event: str, data: Any) -> dict:
    return {"event": event, "data": data}


class LineDecoder:
    """Splits a byte stream into individual JSON messages (line by line)."""

    def __init__(self) -> None:
        self._buf = bytearray()

    def feed(self, chunk: bytes) -> Iterator[dict]:
        self._buf += chunk
        while True:
            nl = self._buf.find(b"\n")
            if nl < 0:
                break
            line = bytes(self._buf[:nl])
            del self._buf[:nl + 1]
            line = line.strip()
            if not line:
                continue
            yield json.loads(line.decode("utf-8"))


# --------------------------------------------------------------------------- #
#  Connection (blocking send/receive of whole messages on a single peer socket)
# --------------------------------------------------------------------------- #
class IpcConnection:
    """Thin wrapper around a connected socket: sends/reads whole messages."""

    def __init__(self, sock: socket.socket):
        self._sock = sock
        self._decoder = LineDecoder()
        self._send_lock = threading.Lock()

    def send(self, msg: dict) -> None:
        data = encode(msg)
        with self._send_lock:
            self._sock.sendall(data)

    def messages(self) -> Iterator[dict]:
        """Blocking generator over incoming messages until EOF."""
        while True:
            try:
                chunk = self._sock.recv(65536)
            except OSError:
                return
            if not chunk:
                return
            yield from self._decoder.feed(chunk)

    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass


# --------------------------------------------------------------------------- #
#  Client
# --------------------------------------------------------------------------- #
class IpcClient:
    """Synchronous IPC client with an optional event callback (own reader thread)."""

    def __init__(self, address: str | tuple[str, int] | None = None,
                 on_event: Callable[[str, Any], None] | None = None,
                 timeout: float = 5.0):
        self.address = address or default_socket_path()
        self._on_event = on_event
        self._timeout = timeout
        self._conn: IpcConnection | None = None
        self._sock: socket.socket | None = None
        self._next_id = 1
        self._pending: dict[int, threading.Event] = {}
        self._results: dict[int, dict] = {}
        self._lock = threading.Lock()
        self._reader: threading.Thread | None = None

    def connect(self) -> "IpcClient":
        sock = _make_socket(self.address)
        sock.settimeout(self._timeout)
        sock.connect(self.address)
        sock.settimeout(None)
        self._sock = sock
        self._conn = IpcConnection(sock)
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()
        return self

    def _read_loop(self) -> None:
        assert self._conn is not None
        for msg in self._conn.messages():
            if "event" in msg:
                if self._on_event:
                    self._on_event(msg["event"], msg.get("data"))
                continue
            mid = msg.get("id")
            with self._lock:
                self._results[mid] = msg
                ev = self._pending.get(mid)
            if ev is not None:
                ev.set()
        # Connection closed -> wake everyone still waiting.
        with self._lock:
            for ev in self._pending.values():
                ev.set()

    def call(self, method: str, params: dict | None = None, timeout: float = 10.0) -> Any:
        """Send a request and wait for the result (raises on error)."""
        assert self._conn is not None, "not connected"
        with self._lock:
            mid = self._next_id
            self._next_id += 1
            ev = threading.Event()
            self._pending[mid] = ev
        self._conn.send(make_request(mid, method, params))
        if not ev.wait(timeout):
            with self._lock:
                self._pending.pop(mid, None)
            raise TimeoutError(f"IPC timeout on {method!r}")
        with self._lock:
            self._pending.pop(mid, None)
            msg = self._results.pop(mid, None)
        if msg is None:
            raise ConnectionError("lost connection to the daemon")
        if not msg.get("ok"):
            raise IpcError(msg.get("error", {}).get("message", "unknown error"))
        return msg.get("result")

    def notify(self, method: str, params: dict | None = None) -> None:
        assert self._conn is not None
        self._conn.send(make_request(None, method, params))

    def close(self) -> None:
        if self._conn:
            self._conn.close()

    def __enter__(self) -> "IpcClient":
        return self.connect()

    def __exit__(self, *exc) -> None:
        self.close()


class IpcError(RuntimeError):
    """An application error reported by the daemon."""


# --------------------------------------------------------------------------- #
#  Server (reference implementation for tests; production daemon is in Rust)
# --------------------------------------------------------------------------- #
@dataclass
class _Peer:
    conn: IpcConnection


class IpcServer:
    """Accepts connections and calls a handler per request.

    The handler receives ``(method, params, peer)`` and returns the result (any
    JSON-serializable object) or raises an exception, which is sent back as an
    error response. Via ``peer.send_event`` it can later push events to that
    specific connection (e.g. for subscriptions).
    """

    def __init__(self, address: str | tuple[str, int] | None = None):
        self.address = address or default_socket_path()
        self._handler: Callable[[str, dict, "ServerPeer"], Any] | None = None
        self._srv: socket.socket | None = None
        self._peers: set["ServerPeer"] = set()
        self._peers_lock = threading.Lock()
        self._stop = threading.Event()
        self.bound_address: str | tuple[str, int] | None = None

    def set_handler(self, handler: Callable[[str, dict, "ServerPeer"], Any]) -> None:
        self._handler = handler

    def start(self) -> "IpcServer":
        srv = _make_socket(self.address)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        if isinstance(self.address, str):
            try:
                os.unlink(self.address)
            except FileNotFoundError:
                pass
        srv.bind(self.address)
        srv.listen(8)
        self.bound_address = srv.getsockname() if isinstance(self.address, tuple) else self.address
        self._srv = srv
        threading.Thread(target=self._accept_loop, daemon=True).start()
        return self

    def _accept_loop(self) -> None:
        assert self._srv is not None
        while not self._stop.is_set():
            try:
                conn, _ = self._srv.accept()
            except OSError:
                return
            peer = ServerPeer(IpcConnection(conn))
            with self._peers_lock:
                self._peers.add(peer)
            threading.Thread(target=self._serve_peer, args=(peer,), daemon=True).start()

    def _serve_peer(self, peer: "ServerPeer") -> None:
        try:
            for msg in peer.conn.messages():
                self._dispatch(peer, msg)
        finally:
            with self._peers_lock:
                self._peers.discard(peer)
            peer.conn.close()

    def _dispatch(self, peer: "ServerPeer", msg: dict) -> None:
        mid = msg.get("id")
        method = msg.get("method", "")
        params = msg.get("params") or {}
        if self._handler is None:
            peer.conn.send(make_error(mid, "no handler registered"))
            return
        try:
            result = self._handler(method, params, peer)
        except Exception as exc:  # noqa: BLE001 - return it as an error response
            if mid is not None:
                peer.conn.send(make_error(mid, str(exc)))
            return
        if mid is not None:
            peer.conn.send(make_response(mid, result))

    def broadcast(self, event: str, data: Any, only_subscribed: bool = True) -> None:
        """Send an event to all (optionally only subscribed) connections."""
        with self._peers_lock:
            peers = list(self._peers)
        for peer in peers:
            if only_subscribed and not peer.subscribed:
                continue
            try:
                peer.send_event(event, data)
            except OSError:
                pass

    def stop(self) -> None:
        self._stop.set()
        if self._srv:
            self._srv.close()
        with self._peers_lock:
            peers = list(self._peers)
        for peer in peers:
            peer.conn.close()
        if isinstance(self.address, str):
            try:
                os.unlink(self.address)
            except OSError:
                pass


class ServerPeer:
    """Represents a connected client on the server side."""

    def __init__(self, conn: IpcConnection):
        self.conn = conn
        self.subscribed = False

    def send_event(self, event: str, data: Any) -> None:
        self.conn.send(make_event(event, data))
