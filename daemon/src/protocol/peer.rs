//! Peer-to-peer "request use" protocol (UDP/49300, host-to-host).
//!
//! A host that wants to use a device held by another host sends the request NOT to
//! the device server but directly to the holder's IP (from the owner IP of the device
//! list) on UDP port 49300. The holder acknowledges immediately (ACK) and, after the
//! user's decision, sends the result.
//!
//! Packet layout (big-endian, identical for every message)::
//!
//!     [0:2]   u16   payload length (incl. these 2 bytes)
//!     [2:4]   u16   message type
//!     [4:10]  6B    device server MAC
//!     [10:12] u16   idVendor
//!     [12:14] u16   idProduct
//!     [..]    str   description + NUL
//!     [..]    str   tag ("H112") + NUL
//!     [..]    str   user name + NUL   (request: requester; reply: holder)
//!     [..]    4B    padding 00 00 00 00
//!
//! Message types: 0x0001 REQUEST, 0x0007 ACK, 0x0004 DENIED, 0x0003 ACCEPTED,
//! 0x0005 RELEASED, 0x0008 REJECTED (description mismatch).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::wire::{need, rd_u16, read_cstr};

pub const PORT: u16 = 49300;

pub const TYPE_REQUEST: u16 = 0x0001;
pub const TYPE_ACK: u16 = 0x0007;
pub const TYPE_DENIED: u16 = 0x0004;
pub const TYPE_ACCEPTED: u16 = 0x0003;
pub const TYPE_RELEASED: u16 = 0x0005;
pub const TYPE_REJECTED: u16 = 0x0008;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMessage {
    pub msg_type: u16,
    pub mac: [u8; 6],
    pub id_vendor: u16,
    pub id_product: u16,
    pub description: String,
    pub tag: String,
    pub user: String,
}

impl PeerMessage {
    pub fn mac_str(&self) -> String {
        self.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
    }
}

pub fn build(
    msg_type: u16,
    mac: [u8; 6],
    id_vendor: u16,
    id_product: u16,
    description: &str,
    tag: &str,
    user: &str,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&mac);
    body.extend_from_slice(&id_vendor.to_be_bytes());
    body.extend_from_slice(&id_product.to_be_bytes());
    body.extend_from_slice(description.as_bytes());
    body.push(0);
    body.extend_from_slice(tag.as_bytes());
    body.push(0);
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.extend_from_slice(&[0, 0, 0, 0]); // observed padding
    let total = (4 + body.len()) as u16;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// Reply carrying the same device fields as `msg` but a new type/user.
pub fn build_from(msg_type: u16, msg: &PeerMessage, user: &str) -> Vec<u8> {
    build(msg_type, msg.mac, msg.id_vendor, msg.id_product, &msg.description, &msg.tag, user)
}

/// Normalize a description exactly like SX Virtual Link.
///
/// The device server left-pads short device descriptions with spaces (e.g.
/// `"         USB Flash Memory"`). SX Virtual Link normalizes them before display and
/// matching, and rejects a request whose description does not match (type 0x0008).
///
/// The rule, confirmed against the vendor client's replies: every run of
/// spaces (leading or internal) collapses to exactly one, and trailing spaces are
/// removed entirely (rstrip); no further trimming. So `"         USB Flash Memory"`
/// becomes `" USB Flash Memory"` (the single leading space is kept).
pub fn request_description(desc: &str) -> String {
    let mut out = String::with_capacity(desc.len());
    let mut prev_space = false;
    for c in desc.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

pub fn parse(buf: &[u8]) -> Result<PeerMessage> {
    if buf.len() < 14 {
        bail!("peer packet too short");
    }
    need(buf, 14, "peer packet")?;
    let msg_type = rd_u16(buf, 2);
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&buf[4..10]);
    let id_vendor = rd_u16(buf, 10);
    let id_product = rd_u16(buf, 12);
    let (description, off) = read_cstr(buf, 14);
    let (tag, off) = read_cstr(buf, off);
    let (user, _off) = read_cstr(buf, off);
    Ok(PeerMessage { msg_type, mac, id_vendor, id_product, description, tag, user })
}

/// Fire-and-forget sender for peer replies (real endpoint or a test fake).
pub trait PeerSender: Send + Sync {
    fn send(&self, addr: SocketAddr, packet: Vec<u8>);
}

/// UDP endpoint on port 49300: receives peer messages and sends replies.
///
/// Replies MUST go to the datagram's source address: a requester sends from an
/// ephemeral port and expects the reply there (not on the fixed port 49300).
pub struct PeerEndpoint {
    sock: Arc<UdpSocket>,
}

impl PeerEndpoint {
    /// Bind the endpoint and start the receive loop. Returns the endpoint and a channel
    /// of `(source_addr, datagram)`.
    pub async fn start() -> Result<(Arc<PeerEndpoint>, mpsc::Receiver<(SocketAddr, Vec<u8>)>)> {
        let sock = Arc::new(UdpSocket::bind(("0.0.0.0", PORT)).await?);
        let (tx, rx) = mpsc::channel::<(SocketAddr, Vec<u8>)>(256);
        let recv_sock = sock.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match recv_sock.recv_from(&mut buf).await {
                    Ok((n, addr)) => {
                        if tx.send((addr, buf[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Ok((Arc::new(PeerEndpoint { sock }), rx))
    }
}

impl PeerSender for PeerEndpoint {
    fn send(&self, addr: SocketAddr, packet: Vec<u8>) {
        // Non-blocking best-effort send (fire-and-forget, like the vendor client).
        let _ = self.sock.try_send_to(&packet, addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real wire bytes captured from the vendor client (2026 captures), MAC sanitized
    // to a placeholder while keeping the exact layout/lengths.
    const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

    #[test]
    fn build_parse_roundtrip() {
        let pkt = build(TYPE_REQUEST, MAC, 0x0930, 0x6544, " USB Flash Memory", "H112", "Testuser");
        let msg = parse(&pkt).unwrap();
        assert_eq!(msg.msg_type, TYPE_REQUEST);
        assert_eq!(msg.mac_str(), "00:11:22:33:44:55");
        assert_eq!((msg.id_vendor, msg.id_product), (0x0930, 0x6544));
        assert_eq!(msg.description, " USB Flash Memory");
        assert_eq!(msg.tag, "H112");
        assert_eq!(msg.user, "Testuser");
    }

    #[test]
    fn build_length_prefix_and_padding() {
        let pkt = build(TYPE_REQUEST, MAC, 0x0930, 0x6544, " USB Flash Memory", "H112", "Testuser");
        // total length prefix equals the whole packet length
        assert_eq!(rd_u16(&pkt, 0) as usize, pkt.len());
        // ends with 4 padding zero bytes
        assert_eq!(&pkt[pkt.len() - 4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn build_from_keeps_device_fields() {
        let req = parse(&build(TYPE_REQUEST, MAC, 0x0930, 0x6544, " USB Flash Memory", "H112", "Requester")).unwrap();
        let ack = parse(&build_from(TYPE_ACK, &req, "holder")).unwrap();
        assert_eq!(ack.msg_type, TYPE_ACK);
        assert_eq!((ack.id_vendor, ack.id_product), (0x0930, 0x6544));
        assert_eq!(ack.description, " USB Flash Memory");
        assert_eq!(ack.user, "holder"); // the reply carries the holder's name
    }

    #[test]
    fn parse_too_short() {
        assert!(parse(&[0x00, 0x01]).is_err());
    }

    #[test]
    fn request_description_matches_vendor_routine() {
        // Runs of spaces (leading/internal) -> one space; trailing spaces removed.
        assert_eq!(request_description("         USB Flash Memory"), " USB Flash Memory");
        assert_eq!(request_description(" USB Flash Memory"), " USB Flash Memory");
        assert_eq!(request_description("Generic Mass Storage Device"), "Generic Mass Storage Device");
        assert_eq!(request_description("A   B  C"), "A B C");
        assert_eq!(request_description("Memory   "), "Memory");
        assert_eq!(request_description("  A  B   "), " A B");
        assert_eq!(request_description(""), "");
    }
}
