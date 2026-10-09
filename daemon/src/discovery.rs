//! Live discovery of Silex device servers on the LAN (UDP/19540).
//!
//! Two combined paths: passive (the device periodically sends announcements
//! 0x0002/0x9506) and active (we broadcast a GET USB_DEVICE_LIST query plus a poll,
//! which makes the device announce itself and add us to its notify list).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

use crate::protocol::udp_discovery::{self as ud, Announcement, Message};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceServer {
    pub ip: String,
    pub mac: Vec<u8>,
    pub model: String,
    pub version: String,
    pub hostname: String,
    pub attached_device: String,
    pub devices: Vec<ud::UsbDeviceEntry>,
}

impl DeviceServer {
    pub fn new(ip: &str) -> Self {
        Self { ip: ip.to_string(), ..Default::default() }
    }

    pub fn mac_str(&self) -> String {
        if self.mac.len() == 6 {
            self.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
        } else {
            String::new()
        }
    }

    pub fn update_from_announcement(&mut self, ann: &Announcement) {
        self.mac = ann.mac.clone();
        self.model = ann.model.clone();
        self.version = ann.version.clone();
        self.hostname = ann.hostname.clone();
        self.attached_device = ann.attached_device.clone();
    }
}

/// Best-effort local IP used to reach `peer_ip`.
pub fn local_ip_towards(peer_ip: &str) -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((peer_ip, ud::PORT)).ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

/// Bind a discovery UDP socket: prefer port 19540 (to catch broadcast announcements),
/// falling back to an ephemeral port (send-only / unicast replies) if it is in use.
async fn bind_discovery_socket() -> Result<UdpSocket> {
    let sock = match UdpSocket::bind(("0.0.0.0", ud::PORT)).await {
        Ok(s) => s,
        Err(_) => UdpSocket::bind("0.0.0.0:0").await?,
    };
    sock.set_broadcast(true).ok();
    Ok(sock)
}

async fn send_active(sock: &UdpSocket, targets: &[(String, u16)]) {
    let get = ud::build_get_device_list(ud::Phase::Query, None);
    let poll = ud::build_poll();
    for (host, port) in targets {
        let _ = sock.send_to(&get, (host.as_str(), *port)).await;
        let _ = sock.send_to(&poll, (host.as_str(), *port)).await;
    }
}

fn ingest(servers: &mut HashMap<String, DeviceServer>, src_ip: String, data: &[u8]) {
    let msg = match ud::parse(data) {
        Ok(m) => m,
        Err(_) => return,
    };
    // Only genuine device->host replies create a server entry; our own reflected
    // broadcast commands/polls are ignored (otherwise the local host shows up empty).
    match msg {
        Message::Announcement(ann) => {
            let srv = servers.entry(src_ip.clone()).or_insert_with(|| DeviceServer::new(&src_ip));
            srv.update_from_announcement(&ann);
            if srv.ip.is_empty() {
                srv.ip = ann.ip;
            }
        }
        Message::DeviceList(list) => {
            let srv = servers.entry(src_ip.clone()).or_insert_with(|| DeviceServer::new(&src_ip));
            srv.devices = list.devices;
        }
        _ => {}
    }
}

/// One-shot discovery for `timeout`, returning servers keyed uniquely by IP.
pub async fn discover(timeout_dur: Duration, broadcast: &str) -> Vec<DeviceServer> {
    let sock = match bind_discovery_socket().await {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    send_active(&sock, &[(broadcast.to_string(), ud::PORT)]).await;

    let mut servers: HashMap<String, DeviceServer> = HashMap::new();
    let deadline = Instant::now() + timeout_dur;
    let mut buf = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, addr))) => ingest(&mut servers, addr.ip().to_string(), &buf[..n]),
            _ => break,
        }
    }
    servers.into_values().collect()
}

/// Persistent discovery socket for the daemon: it stays open and receives the periodic
/// announcements at any time (not just in a short query window). Parsed messages are
/// delivered over the returned channel; [`poll`](DiscoveryListener::poll) actively
/// triggers a device-list query (broadcast + optional unicast search addresses).
pub struct DiscoveryListener {
    sock: Arc<UdpSocket>,
    broadcast: String,
}

impl DiscoveryListener {
    pub async fn start(broadcast: &str) -> Result<(Arc<Self>, mpsc::Receiver<(String, Message)>)> {
        let sock = Arc::new(bind_discovery_socket().await?);
        let (tx, rx) = mpsc::channel::<(String, Message)>(256);
        let recv_sock = sock.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match recv_sock.recv_from(&mut buf).await {
                    Ok((n, addr)) => {
                        if let Ok(msg) = ud::parse(&buf[..n]) {
                            if tx.send((addr.ip().to_string(), msg)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Ok((Arc::new(Self { sock, broadcast: broadcast.to_string() }), rx))
    }

    pub async fn poll(&self, addresses: &[(String, u16)]) {
        let mut targets = vec![(self.broadcast.clone(), ud::PORT)];
        targets.extend_from_slice(addresses);
        send_active(&self.sock, &targets).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_str_formats() {
        let mut s = DeviceServer::new("192.0.2.1");
        assert_eq!(s.mac_str(), "");
        s.mac = vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        assert_eq!(s.mac_str(), "00:11:22:33:44:55");
    }

    #[test]
    fn update_from_announcement_fills_fields() {
        let ann = Announcement {
            model: "DS-510".into(),
            mac: vec![0, 1, 2, 3, 4, 5],
            ip: "192.0.2.9".into(),
            version: "Ver 2.2.0".into(),
            attached_device: "Mass Storage".into(),
            hostname: "usbserver".into(),
        };
        let mut s = DeviceServer::new("192.0.2.9");
        s.update_from_announcement(&ann);
        assert_eq!(s.model, "DS-510");
        assert_eq!(s.hostname, "usbserver");
        assert_eq!(s.mac_str(), "00:01:02:03:04:05");
    }

    #[tokio::test]
    async fn discover_returns_empty_without_devices() {
        // No device answers on loopback broadcast within a short window.
        let found = discover(Duration::from_millis(150), "127.0.0.1").await;
        assert!(found.is_empty());
    }
}
