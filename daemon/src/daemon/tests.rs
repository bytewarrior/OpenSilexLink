//! Daemon orchestration tests using injected fakes (no hardware, no real UDP).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;

use super::*;
use crate::discovery::DeviceServer;
use crate::protocol::peer as pp;
use crate::protocol::udp_discovery::UsbDeviceEntry;
use crate::settings::SettingsStore;
use crate::usbip_control::UsbipOps;

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const MAC_STR: &str = "00:11:22:33:44:55";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_store() -> Arc<SettingsStore> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("osl-daemon-{}-{}.json", std::process::id(), n));
    let _ = std::fs::remove_file(&path);
    Arc::new(SettingsStore::new(Some(path)))
}

#[derive(Default)]
struct FakeUsbip {
    attached: Mutex<HashMap<String, i32>>,
    next: Mutex<i32>,
    calls: Mutex<Vec<String>>,
}

impl UsbipOps for FakeUsbip {
    fn ensure_module(&self) -> Result<()> {
        self.calls.lock().unwrap().push("ensure_module".into());
        Ok(())
    }
    fn used_ports(&self) -> HashSet<i32> {
        self.attached.lock().unwrap().values().copied().collect()
    }
    fn attach(&self, busid: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("attach {busid}"));
        let mut n = self.next.lock().unwrap();
        *n += 1;
        self.attached.lock().unwrap().insert(busid.to_string(), *n);
        Ok(())
    }
    fn detach(&self, port: i32) -> Result<()> {
        self.calls.lock().unwrap().push(format!("detach {port}"));
        self.attached.lock().unwrap().retain(|_, v| *v != port);
        Ok(())
    }
}

impl FakeUsbip {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct FakePeer {
    sent: Mutex<Vec<(SocketAddr, PeerMessage)>>,
}

impl PeerSender for FakePeer {
    fn send(&self, addr: SocketAddr, packet: Vec<u8>) {
        if let Ok(m) = pp::parse(&packet) {
            self.sent.lock().unwrap().push((addr, m));
        }
    }
}

impl FakePeer {
    fn types(&self) -> Vec<u16> {
        self.sent.lock().unwrap().iter().map(|(_, m)| m.msg_type).collect()
    }
    fn dests(&self) -> Vec<SocketAddr> {
        self.sent.lock().unwrap().iter().map(|(a, _)| *a).collect()
    }
}

fn server() -> DeviceServer {
    DeviceServer {
        ip: "192.0.2.143".into(),
        mac: MAC.to_vec(),
        model: "DS-510".into(),
        version: String::new(),
        hostname: "usbserver".into(),
        attached_device: String::new(),
        devices: vec![
            UsbDeviceEntry::new("dev01", "Generic Mass Storage Device", 0x058f, 0x6387, "H111"),
            UsbDeviceEntry { usb_class: 0x08, ..UsbDeviceEntry::new("dev02", "USB Flash Memory", 0x0930, 0x6544, "H112") },
        ],
    }
}

fn make() -> (Arc<Daemon>, Arc<FakeUsbip>, Arc<FakePeer>) {
    let fake = Arc::new(FakeUsbip::default());
    let fpeer = Arc::new(FakePeer::default());
    let d = Daemon::new(temp_store(), fake.clone() as Arc<dyn UsbipOps>, false);
    *d.peer.lock().unwrap() = Some(fpeer.clone() as Arc<dyn PeerSender>);
    (d, fake, fpeer)
}

fn req(user: &str) -> Vec<u8> {
    pp::build(pp::TYPE_REQUEST, MAC, 0x0930, 0x6544, "USB Flash Memory", "H112", user)
}

fn resp(t: u16, user: &str) -> Vec<u8> {
    pp::build(t, MAC, 0x0930, 0x6544, "USB Flash Memory", "H112", user)
}

fn req_addr() -> SocketAddr {
    "192.0.2.9:54077".parse().unwrap()
}

fn holder_addr() -> SocketAddr {
    format!("192.0.2.50:{}", pp::PORT).parse().unwrap()
}

fn statuses(snap: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for srv in snap["servers"].as_array().unwrap() {
        for d in srv["devices"].as_array().unwrap() {
            out.insert(d["dev_id"].as_str().unwrap().to_string(), d["status"].as_str().unwrap().to_string());
        }
    }
    out
}

#[tokio::test]
async fn snapshot_lists_devices_available() {
    let (d, _f, _p) = make();
    d.apply_server(server(), true);
    let snap = d.servers_snapshot();
    let srv = &snap["servers"][0];
    assert_eq!(srv["mac"], "00:11:22:33:44:55");
    assert_eq!(srv["hostname"], "usbserver");
    let st = statuses(&snap);
    assert_eq!(st["dev01"], "available");
    assert_eq!(st["dev02"], "available");
    // key + type shape
    assert_eq!(srv["devices"][1]["key"], "0930:6544:USB Flash Memory");
    assert_eq!(srv["devices"][1]["type"], "Mass storage device");
}

#[tokio::test]
async fn connect_attaches_and_marks_connected() {
    let (d, fake, _p) = make();
    d.apply_server(server(), true);
    let res = d.connect(MAC_STR, "dev02").await.unwrap();
    assert_eq!(res["connected"], true);
    assert!(fake.calls().iter().any(|c| c == "attach 1-2"));
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert_eq!(statuses(&d.servers_snapshot())["dev02"], "connected");
}

#[tokio::test]
async fn owner_ip_drives_status() {
    let (d, _f, _p) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.119".into(); // us
    s.devices.push(UsbDeviceEntry {
        owner_ip: "192.0.2.77".into(),
        ..UsbDeviceEntry::new("dev03", "Other", 0x1, 0x2, "H1")
    });
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.119".into());
    let st = statuses(&d.servers_snapshot());
    assert_eq!(st["dev01"], "available");
    assert_eq!(st["dev02"], "connected"); // owner == our ip
    assert_eq!(st["dev03"], "in_use"); // foreign owner
}

#[tokio::test]
async fn peer_request_for_unheld_device_only_acks() {
    let (d, _f, fpeer) = make();
    d.apply_server(server(), true); // we hold nothing
    d.on_peer_message(req_addr(), req("Testuser"));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK]);
    assert!(d.pending_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn peer_request_prompts_then_deny_keeps_connection() {
    let (d, _f, fpeer) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(req_addr(), req("Testuser"));
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK]);
    assert!(d.pending_requests.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    let res = d.respond_access_request(MAC_STR, "dev02", false).await.unwrap();
    assert_eq!(res["accepted"], false);
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK, pp::TYPE_DENIED]);
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn peer_request_accept_disconnects() {
    let (d, fake, fpeer) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(req_addr(), req("Testuser"));
    let res = d.respond_access_request(MAC_STR, "dev02", true).await.unwrap();
    assert_eq!(res["accepted"], true);
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK, pp::TYPE_ACCEPTED, pp::TYPE_RELEASED]);
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(fake.calls().iter().any(|c| c == "detach 1"));
}

#[tokio::test]
async fn prune_phantoms_detaches_unplugged_device() {
    let (d, fake, _p) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    // dev02 unplugged: the device server now reports a list WITHOUT dev02 (authoritative).
    let mut gone = server();
    gone.devices.retain(|e| e.dev_id != "dev02");
    d.apply_server(gone, true);
    d.prune_phantoms();
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(fake.calls().iter().any(|c| c == "detach 1"));
}

#[tokio::test]
async fn rule_auto_disconnect_immediate() {
    let (d, _f, fpeer) = make();
    d.apply_server(server(), true);
    d.connect(MAC_STR, "dev02").await.unwrap();
    d.store
        .set_device_rule(
            MAC_STR,
            "0930:6544:USB Flash Memory",
            &serde_json::json!({"auto_disconnect_on_request": true, "auto_disconnect_timeout": 0})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
    d.on_peer_message(req_addr(), req("Testuser"));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(fpeer.types(), vec![pp::TYPE_ACK, pp::TYPE_ACCEPTED, pp::TYPE_RELEASED]);
    // all replies go to the requester's source port (not 49300)
    assert!(fpeer.dests().iter().all(|a| *a == req_addr()));
    assert!(!d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn request_access_sends_to_holder() {
    let (d, _f, fpeer) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.50".into();
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.99".into());
    let res = d.request_access(MAC_STR, "dev02").await.unwrap();
    assert_eq!(res["owner_ip"], "192.0.2.50");
    let sent = fpeer.sent.lock().unwrap();
    assert_eq!(sent[0].0, holder_addr());
    assert_eq!(sent[0].1.msg_type, pp::TYPE_REQUEST);
    assert!(d.outgoing.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn request_access_rejects_non_foreign() {
    let (d, _f, _p) = make();
    d.apply_server(server(), true); // dev01 has no owner
    assert!(d.request_access(MAC_STR, "dev01").await.is_err());
}

#[tokio::test]
async fn request_denied_flow_clears_outgoing() {
    let (d, _f, _p) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.50".into();
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.99".into());
    d.request_access(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(holder_addr(), resp(pp::TYPE_ACK, "Holder"));
    d.on_peer_message(holder_addr(), resp(pp::TYPE_DENIED, "Holder"));
    assert!(!d.outgoing.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
}

#[tokio::test]
async fn request_released_autotakes() {
    let (d, fake, _p) = make();
    let mut s = server();
    s.devices[1].owner_ip = "192.0.2.50".into();
    d.apply_server(s, true);
    d.our_ip.lock().unwrap().insert("192.0.2.143".into(), "192.0.2.99".into());
    d.request_access(MAC_STR, "dev02").await.unwrap();
    d.on_peer_message(holder_addr(), resp(pp::TYPE_ACK, "Holder"));
    d.on_peer_message(holder_addr(), resp(pp::TYPE_ACCEPTED, "Holder"));
    d.on_peer_message(holder_addr(), resp(pp::TYPE_RELEASED, "Holder"));
    // autotake runs in a spawned task
    for _ in 0..50 {
        if d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(d.connections.lock().unwrap().contains_key(&(MAC_STR.into(), "dev02".into())));
    assert!(fake.calls().iter().any(|c| c == "attach 1-2"));
}

#[tokio::test]
async fn respond_without_pending_is_noop() {
    let (d, _f, _p) = make();
    d.apply_server(server(), true);
    let res = d.respond_access_request(MAC_STR, "dev02", true).await.unwrap();
    assert_eq!(res["ok"], false);
}
