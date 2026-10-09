# OpenSilexLink

Linux software for **Silex USB device servers** (USB over Ethernet): a daemon and
a desktop GUI that make the USB devices plugged into a Silex device server
usable on Linux as if they were attached locally.

Silex's client software for these servers, *SX Virtual Link*, has no Linux
version, and several of the servers are end-of-life. OpenSilexLink
speaks the server's network protocol itself and hands the USB devices to the
Linux kernel through **USB/IP**, so the normal kernel drivers handle them and
no kernel module of its own is needed.

## Status

**Version 1.0.0.** Tested against a **Silex DS-510**, an end-of-life model.
Working with USB flash drives (mass storage), USB audio for recording and
playback, and HID devices, including composite devices. Other Silex models
probably speak the same protocol but are untested; reports are welcome.

## Features

- **Discovery** of Silex device servers on the local network, and of the USB
  devices attached to them, including manual search addresses for routed
  networks
- **Connect and disconnect** USB devices; the device appears as a local USB
  device and the desktop mounts drives as usual
- **All transfer types:** control, bulk, interrupt (keyboards, mice, buttons)
  and isochronous (audio, in both directions)
- **Lossless audio compression** for playback, as the vendor client uses it,
  which saves about a third of the network bandwidth
- **Composite devices** shown as one device, as in SX Virtual Link
- **"In use by …"** shows which host holds a device, by host name
- **Request use:** asks the host that holds a device to release it, compatible
  with SX Virtual Link on the other host (accept, deny, or release automatically
  after a grace period)
- **Auto-connect** (global and per device), **auto-reconnect** after a lost
  connection, and clean-up when a device is unplugged at the server
- **Launch an application** when a device is connected, and disconnect when it
  exits
- **System tray** icon with a device menu, **autostart**, start hidden
- **Options dialog** modelled on SX Virtual Link: search addresses, columns,
  server filter, and a device tree or server table view
- **English and German** user interface, chosen by the system locale
- **No root password per connect:** a small helper with strict argument checks
  plus a polkit policy do the privileged attach/detach

## How it works

```
 Silex device server            OpenSilexLink                      Linux kernel
 + USB devices
                    UDP 19540                   USB/IP, 127.0.0.1:3240
 +-------------+  discovery,    +--------------+  usbip attach    +-----------+
 |   DS-510    | <------------> | silex-daemon | <--------------> | vhci-hcd  | --> /dev/sdX,
 |             |  connect       |  (Rust)      |                  |           |     ALSA, input
 |  USB ports  |  TCP 19540     |              |                  +-----------+
 |             | <------------> |              |
 +-------------+  USB tunnel    +--------------+
                                  ^          ^
                       UDP 49300  |          |  JSON lines over a UNIX socket
                   "request use"  |          |
                 with other hosts v          v
                                         osl-gui (Python, PySide6)
```

- **`silex-daemon`** (Rust, tokio) speaks the Silex protocol: discovery,
  reservation of a device, and one TCP tunnel per device (per interface for
  composite devices). It runs a USB/IP server on `127.0.0.1` and translates
  between USB/IP URBs and Silex frames.
- The kernel's **`vhci-hcd`** driver imports the device over USB/IP, so every
  USB device class is handled by its normal Linux driver.
- `usbip attach` and `detach` need root; the daemon runs them through
  **`silex-usbip-helper`** via `pkexec`, and a polkit rule lets administrators
  do this without a password.
- **`osl-gui`** is a thin front end. It talks to the daemon over a local socket
  and starts it when needed; connections stay up when the window is closed.

The protocol is documented in [docs/protocol.md](docs/protocol.md).

## Requirements

- Linux with the `vhci-hcd` kernel module and the `usbip` tools
- polkit with `pkexec`, for attach/detach without running as root
- Python 3.9 or later and PySide6 (Qt 6) for the GUI
- On some distributions the Qt xcb platform plugin needs `libxcb-cursor0`
- To build from source: Rust 1.75 or later
- A Silex USB device server on the local network

## Installation

### From the release packages

The [releases](https://github.com/bytewarrior/OpenSilexLink/releases) contain a
`.deb` and an `.rpm` package for x86-64.

```bash
sudo apt install ./opensilexlink_1.0.0_amd64.deb      # Debian, Ubuntu, Mint
sudo dnf install ./opensilexlink-1.0.0-1.x86_64.rpm   # Fedora
```

This installs `silex-daemon`, `osl`, `osl-gui` with a menu entry, the helper and
the polkit policy and rule. The GUI, `usbip` and polkit come as recommended
packages. Debian 13 and Ubuntu 25.04 or later have PySide6 as packages
(`python3-pyside6.qtwidgets`). On older releases, such as Ubuntu 24.04 and
Linux Mint 22, install PySide6 with pip, for example into a virtual environment
as described below.

### From source

```bash
git clone https://github.com/bytewarrior/OpenSilexLink.git
cd OpenSilexLink

# Daemon: installs silex-daemon to ~/.cargo/bin (must be on PATH)
cargo install --path daemon

# Client: GUI and CLI in a virtual environment
python3 -m venv .venv
.venv/bin/pip install -e './client[gui]'

# Helper, polkit policy and rule (once, as root)
sudo ./packaging/install-privileged.sh
```

Run the tests (no device server needed):

```bash
cargo test --manifest-path daemon/Cargo.toml
.venv/bin/pip install pytest && (cd client && ../.venv/bin/python -m pytest)
```

To build the packages locally (needs [nfpm](https://nfpm.goreleaser.com)):

```bash
PYTHON=.venv/bin/python packaging/build-packages.sh 1.0.0
```

## Usage

### GUI

```bash
osl-gui
```

The window lists the device servers found and their USB devices with their
status: *Available*, *Connected by me* or *In use by <host>*. Select a device
and choose **Connect**, or use the context menu or the tray icon. Closing the
window keeps the program in the system tray, and the connections stay up.
**Options** sets search addresses, columns, filters and autostart; the device
**Properties** set auto-connect and the request-use behaviour per device.

### Command line

```bash
silex-daemon discover               # list device servers and their USB devices
silex-daemon                        # run the daemon in the foreground
osl ipc list_servers                # query a running daemon
osl ipc --watch                     # print the daemon's events
```

The settings are stored in `~/.config/silex/settings.json`. The language follows
the system locale; to force one, start the GUI with e.g. `LANG=en_US.UTF-8
osl-gui`.

## Roadmap

- Tests and reports from other Silex device server models
- More USB device classes in practice (printers, scanners, video)
- Packages for other architectures

## Contributing

Bug reports and pull requests are welcome. Please include the device server
model and firmware version (`silex-daemon discover` shows both), the USB device (`lsusb` on a
machine where it is attached directly), your distribution, and the daemon log
(`RUST_LOG=debug silex-daemon`).

## Acknowledgements

- **USB/IP** by Takahiro Hirofuchi and its maintainers: the kernel's `vhci-hcd`
  driver and the `usbip` tools do the actual USB work on the Linux side.
  OpenSilexLink implements the server side of the USB/IP protocol as
  documented in the kernel's `usbip_protocol.rst`.
- [tokio](https://tokio.rs), [serde](https://serde.rs) and the other Rust
  crates listed in [daemon/Cargo.toml](daemon/Cargo.toml)
- [Qt for Python (PySide6)](https://doc.qt.io/qtforpython-6/) for the GUI
- [nfpm](https://nfpm.goreleaser.com) for the packages

## Disclaimer

This project is not affiliated with, endorsed by or connected to Silex
Technology. All product names and trademarks, including "Silex", "DS-510" and
"SX Virtual Link", belong to their owners and are used only to describe
compatibility. The protocol was worked out by observing the network traffic of
the vendor software and the device, for the sole purpose of interoperability.
Use this software at your own risk.

## License

MIT, see [LICENSE](LICENSE).

## Part of the save old hardware collection

<img src="saveOldHardware.png" align="left" hspace="10">

This project is part of a larger idea: I own a lot of old hardware that would be unusable if I hadn't written drivers and tools to bring it back to life. From the early nineties and, even more so, the early 2000s up to today, a lot of Windows-only or use-once-then-throw-away hardware was developed and sold, only to become electronic waste a few years later. And it kept happening, because the manufacturers kept moving on to newer products and dropping support for older devices entirely. Expensive audio equipment, lab gear, other specialized kit that's hard to find, you name it.

As a tinkerer, electrical design engineer and software developer (I do the latter two for a living), I decided to stop accepting this and do something about it. Hence, I've created this logo, and I attach it to every project where saving a device from becoming waste is a central goal.

Feel free to use this logo as well, and spread the word! Thanks for reading this. People like you make a difference in this world. Become a part of us — try to write software for the stuff you own, and consider publishing it as open source.

<div style="clear: both;"></div>
