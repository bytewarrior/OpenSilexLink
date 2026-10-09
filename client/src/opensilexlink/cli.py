"""osl - command-line tool for the OpenSilexLink client.

The heavy lifting (Silex protocol, USB/IP, discovery) lives in the daemon (the Rust
``silex-daemon``). This CLI is a thin client: a debug IPC client and a GUI launcher.

Examples::

    osl ipc list_servers
    osl ipc --watch
    osl gui
"""

from __future__ import annotations

import argparse
import json
import time

from . import __version__, ipc


def _ipc_address(args):
    """--tcp HOST:PORT -> (host, port); otherwise the --socket path (AF_UNIX)."""
    if getattr(args, "tcp", None):
        host, _, port = args.tcp.rpartition(":")
        return (host or "127.0.0.1", int(port))
    return args.socket or ipc.default_socket_path()


def cmd_ipc(args) -> int:
    """Debug client: send one IPC request (or watch events)."""
    params = json.loads(args.params) if args.params else {}
    addr = _ipc_address(args)

    if args.watch:
        def on_event(ev, data):
            print(f"[event] {ev}: {json.dumps(data, ensure_ascii=False)}", flush=True)
        cli = ipc.IpcClient(addr, on_event=on_event).connect()
        cli.call("subscribe")
        print("Subscribed - waiting for events (Ctrl-C to stop).")
        try:
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            pass
        finally:
            cli.close()
        return 0

    with ipc.IpcClient(addr) as cli:
        result = cli.call(args.method, params)
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0


def cmd_gui(args) -> int:
    """Launch the desktop GUI."""
    from .gui.app import main as gui_main
    return gui_main()


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="osl")
    ap.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("ipc", help="debug client for the daemon (send a request / watch events)")
    p.add_argument("method", nargs="?", default="list_servers", help="IPC method")
    p.add_argument("params", nargs="?", help="parameters as a JSON object")
    p.add_argument("--socket", help="AF_UNIX socket path")
    p.add_argument("--tcp", help="TCP HOST:PORT")
    p.add_argument("--watch", action="store_true", help="subscribe and print events continuously")
    p.set_defaults(func=cmd_ipc)

    p = sub.add_parser("gui", help="launch the desktop GUI")
    p.set_defaults(func=cmd_gui)

    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
