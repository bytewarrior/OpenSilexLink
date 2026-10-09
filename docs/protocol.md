# Silex device server protocol

This document describes the network protocol between a Silex USB device server
and its client software, as implemented by OpenSilexLink. It was worked out from
the network traffic between the vendor client (SX Virtual Link) and a **Silex
DS-510**, for the sole purpose of interoperability. Other Silex models probably
speak the same protocol, but only the DS-510 has been tested.

All addresses and MACs in the examples are placeholders (`192.0.2.0/24`,
`00:11:22:33:44:55`). All multi-byte fields are **big-endian** unless stated
otherwise.

## Overview

| Port | Transport | Between | Purpose |
|------|-----------|---------|---------|
| 19540 | UDP | host ↔ device server | discovery, device list, connect/disconnect |
| 19540 | TCP | host ↔ device server | USB tunnel, **one TCP connection per connected USB device (interface)** |
| 49300 | UDP | host ↔ host | "request use" negotiation; the device server is not involved |

UDP commands are ASCII strings with names from the server's printer/scanner
heritage (`EXEC SCANNER CONNECT:`). The TCP tunnel carries raw USB transfers
(control, bulk, interrupt, isochronous); the host acts as the USB host
controller.

Code: [`daemon/src/protocol/`](../daemon/src/protocol/) (pure codecs with unit
tests) and [`daemon/src/usbip/session.rs`](../daemon/src/usbip/session.rs)
(the connect sequence and the tunnel).

## 1. Discovery and control (UDP 19540)

Every UDP message starts with `u16 opcode` and `u16 subcode`.

| opcode / subcode | Direction | Meaning |
|------------------|-----------|---------|
| `0x0002` / `0x9506` | server → host | announcement (periodic, and in reply to a poll) |
| `0x0004` / `0x9506` | host → server | poll; the server announces itself and adds the host to its notify list |
| `0x0001` / `0x0001` | host → server | command, phase **QUERY** |
| `0x0003` / `0x0001` | host → server | command, phase **COMMIT** |
| `0x0001` / `0x0000` | server → host | reply: text (`OK`, `NG`) or device list |
| `0x0003` / `0x0000` | server → host | acknowledgement of a COMMIT |
| `0x2988` / `0x01xx` | server → hosts | descriptor broadcast after a connect; the device is now in use |

### Announcement (`0x0002`)

The server sends it periodically (about every 10 s) to hosts that have polled
it, and answers a broadcast poll with it. Fixed offsets, strings NUL-terminated:

| Offset | Field |
|--------|-------|
| 36 | model name, e.g. `DS-510` |
| 56 | MAC (6 bytes) |
| 66 | IPv4 address (4 bytes) |
| 72 | firmware version string |
| 84 | description of an attached USB device |
| 116 | host name of the server |

The server does not report an IPv6 address; OpenSilexLink derives the
link-local address from the MAC (EUI-64).

### Commands (`0x0001` QUERY / `0x0003` COMMIT)

Each command runs in two phases: first QUERY, then COMMIT. The buffer has a fixed
size (88 bytes; 510 bytes for an `EXEC` QUERY):

| Offset | Field |
|--------|-------|
| 0 | `u16` phase: `0x0001` QUERY, `0x0003` COMMIT |
| 2 | `u16` `0x0001` |
| 4 | ASCII command: `GET USB_DEVICE_LIST`, `EXEC SCANNER CONNECT:` or `EXEC SCANNER DISCONNECT:` |
| … | parameters (`EXEC` only, directly after the command) |
| 84 | `u16` operation code: `0` in QUERY; in COMMIT `0x0083` (device list) or `0x0003` (connect/disconnect) |
| 86 | `u16` `0x0200` |

`EXEC` parameters: `01 06`, server MAC (6 bytes), host IPv4 (4 bytes), device ID
(`dev01`, NUL-terminated); a CONNECT then adds the five bytes `4c 54 02 c0 53`,
whose purpose is unknown (the server accepts them unchanged).

### Device list reply

| Offset | Field |
|--------|-------|
| 0 | `0x0001 0x0000` |
| 4 | `u32` status / operation code (`0x83`, `0x87`, `0x0101` seen) |
| 8 | `u8` number of entries |
| 9 | entries |

Each entry has a variable-length head and a fixed 28-byte block:

```
"dev01\0"                       device ID
"         USB Flash Memory\0"   description (short names are left-padded with spaces)
<4 bytes owner IPv4>            0.0.0.0 = free, otherwise the host that holds the device
-- fixed block, from the class marker --
+0   u8   status (varies: 0x80, 0x84, 0x87, ...)
+1   u8   0x83 (constant; the only reliable anchor)
+2   u16  idVendor
+4   u16  idProduct
+6   16 B tag, NUL-padded, e.g. "H112" or "H111M0"
+22  u8   bDeviceClass
+23  u8   bInterfaceClass
+24  4 B  zero
```

The first letter of the tag is the USB speed: `H` high speed (480 Mbit/s), `F`
full speed (12 Mbit/s), `L` low speed (1.5 Mbit/s, assumed).

**Composite devices** are split into one entry per interface. The entries share
VID:PID and the tag stem; the suffix `M0`, `M1`, … counts the interface, and
`bDeviceClass` is `0xEF`. Each interface needs its own reservation and its own
TCP connection (see below). OpenSilexLink shows them as one device, as the
vendor client does.

### Connect and disconnect

```
host → server   0x0001/0x0001  "EXEC SCANNER CONNECT:"  (QUERY, with MAC, host IP, device ID)
server → host   0x0001/0x0000  "OK"                     (reservation accepted; "NG" = refused)
host → server   0x0003/0x0001  "EXEC SCANNER CONNECT:"  (COMMIT)
server → hosts  0x2988/0x01xx  descriptor broadcast     (device descriptors; device now in use)
server → host   0x0003/0x0000  acknowledgement
host → server   TCP connect to port 19540               (the USB tunnel starts)
```

Disconnect is symmetric: `EXEC SCANNER DISCONNECT:` as QUERY and COMMIT, then the
TCP connection is closed. The next device list shows the device as free.

The server can answer `NG` while it still holds a stale reservation, for example
after it closed a tunnel itself. A DISCONNECT before the retry clears it.

## 2. USB tunnel (TCP 19540)

The tunnel is a stream of frames; a frame is a header plus an optional payload,
and frames are delimited by their length fields only.

### Common prefix

| Offset | Field |
|--------|-------|
| 0 | `u16` opcode. Low byte: `0x01` control, `0x02` interrupt, `0x03` bulk, `0x04` isochronous. Bit `0x8000` = OUT (host sends data). A response repeats the opcode of its request. |
| 2 | `u16` sequence number; a response repeats it |
| 4 | request: `u16` *sel*, high byte = **device index**, low byte = endpoint address. Response: `u16` status1 |
| 6 | request: `u16` `0x0205`. Response: first half of the length field |

The **device index** is `devNN` + 2 (`dev01` → `0x03`, `dev02` → `0x04`). Every
request must carry the index of its target device, otherwise it reaches the first
device. All interface connections of a composite device use the index of its
first interface.

### Requests

**Control** (20 bytes, plus `wLength` bytes of data for OUT):

```
+8   u16  0x0008
+10  u8   bmRequestType
+11  u8   bRequest
+12  u16  wValue
+14  u16  wIndex
+16  u32  wLength
```

**Bulk** (16 bytes, plus the data for OUT):

```
+8   u32  0x82000000
+12  u32  transfer length
```

**Interrupt** (16 bytes, plus the data for OUT):

```
+8   u16  0x8000 | wMaxPacketSize
+10  u16  bInterval
+12  u32  transfer length
```

The server holds an interrupt-IN request open until the endpoint delivers a
report (long poll). Responses on a connection with interrupt endpoints therefore
do not come back in request order; they are matched by sequence number.

**Isochronous** (24-byte header, the same for both directions):

```
+4   u16  0x0001
+6   u16  sel (device index << 8 | endpoint)
+8   u16  0x0205
+10  u16  0x8000 | wMaxPacketSize
+12  u32  start frame (must not be 0 and must rise; 1 ms units)
+16  u16  number of packets n
+18  u16  flags (OUT: 0x8000 = compressed payload)
+20  u32  length (see below)
```

- **IN**: the header is followed by `n` descriptors `{u32 offset, u32 length,
  u32 status}` with packed offsets, and no data. The length field holds the
  reserved size, `n × (wMaxPacketSize + 12)`.
- **OUT**: the length field holds the size of everything after the header.
  Uncompressed (flags `0`): `n` descriptors, then the packed audio. Compressed
  (flags `0x8000`): see [the audio codec](#isochronous-out-audio-codec).

The server accepts the whole packet count of a URB in one request. An isochronous
request must be sent on the connection of its own interface; on another
connection the server resets the TCP connection.

### Responses

```
+0   u16  opcode (as in the request)
+2   u16  sequence number
+4   u16  status1   (0xFFFD = STALL)
+6   u32  data length
+10  u16  status2
+12  data
```

The length is a `u32` at offset 6, which matters for transfers over 64 KiB. An
isochronous IN response carries `u16 n`, `n` descriptors `{offset,
actual_length, status}` and the packed data. An isochronous OUT response is a
bare 12-byte acknowledgement.

### Keepalive

The server closes an idle tunnel after about 30 seconds; UDP traffic does not
count. The vendor client sends a mass-storage TEST UNIT READY every second.
OpenSilexLink instead sends a harmless control transfer (`GET_DESCRIPTOR`) every
10 s, which works for every device class, and enables `SO_KEEPALIVE` on the
socket. Isochronous and interrupt connections keep themselves busy.

### Isochronous OUT audio codec

For playback the vendor client compresses 16-bit audio losslessly, and the
server accepts both compressed and uncompressed frames. OpenSilexLink compresses
only when the result is smaller, and only for AudioStreaming interfaces (class
`0x01`, subclass `0x02`) with 16-bit samples; other isochronous data (24-bit
audio, video) is sent uncompressed. This saves about a third of the bandwidth.

Payload after the 24-byte header:

```
+0   u8   number of channels
+1   u8   0x01
+2   u16  code length in bytes
+4   code bytes (MSB first), then literal bytes
```

The input of the coder is the `n` descriptors followed by the audio, as 16-bit
units:

1. **Per-channel DPCM:** the first sample of each channel stays as it is; every
   following sample is replaced by its difference to the previous sample of the
   same channel (16-bit wrap-around).
2. **Prefix code** over the high byte of each unit; the low byte always goes to
   the literal stream:

| High byte | Code | Literals |
|-----------|------|----------|
| `0x00` | `00` | low byte |
| `0xFF` | `01` | low byte |
| `0x01` | `100` | low byte |
| `0xFE` | `101` | low byte |
| `0x02` | `1100` | low byte |
| `0xFD` | `1101` | low byte |
| other | `111` | low byte, high byte |

Code: [`silex_iso_codec.rs`](../daemon/src/protocol/silex_iso_codec.rs).

## 3. "Request use" between hosts (UDP 49300)

A host that wants a device held by another host sends a request **directly to
the holder** (the owner IP from the device list), on UDP port 49300. Replies go
to the source address of the request.

All messages have the same layout:

```
+0   u16  total length including these two bytes
+2   u16  message type
+4   6 B  device server MAC
+10  u16  idVendor
+12  u16  idProduct
     str  description + NUL
     str  tag + NUL
     str  user name + NUL   (request: the requester; reply: the holder)
     4 B  zero
```

| Type | Direction | Meaning |
|------|-----------|---------|
| `0x0001` | requester → holder | request |
| `0x0007` | holder → requester | received, the holder's user is being asked |
| `0x0004` | holder → requester | denied |
| `0x0003` | holder → requester | accepted, the holder disconnects now |
| `0x0005` | holder → requester | released; the requester can connect |
| `0x0008` | holder → requester | rejected: the request does not match a device the holder uses |

Accept sequence: `0x0007`, then `0x0003`, then the holder sends `EXEC SCANNER
DISCONNECT:` to the device server, then `0x0005`. The requester then connects as
usual.

The description must match the holder's normalized form: every run of spaces is
collapsed to one space and trailing spaces are removed. `"         USB Flash
Memory"` from the device list becomes `" USB Flash Memory"` (one leading space
remains). Any other form is answered with `0x0008`.

## Open questions

- The meaning of the CONNECT token bytes `4c 54 02 c0 53`.
- The meaning of status2 in tunnel responses and of the status byte before the
  `0x83` marker in the device list.
- Whether other Silex models use the same offsets and device index scheme.
