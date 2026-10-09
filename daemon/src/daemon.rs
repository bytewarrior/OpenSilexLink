//! `silex-daemon` orchestration: discovery, state, IPC surface, connect/disconnect,
//! auto-connect/-reconnect, and the host-to-host "request use" peer protocol.
//!
//! The daemon holds the truth (which device servers + USB devices exist and their
//! status) and pushes changes as a `servers` event to subscribed clients (the GUI).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use tokio::task::JoinHandle;

use crate::discovery::{local_ip_towards, DeviceServer, DiscoveryListener};
use crate::ipc::{Handler, IpcAddr, IpcServer, Peer};
use crate::protocol::peer::{self as peerproto, PeerEndpoint, PeerMessage, PeerSender};
use crate::protocol::udp_discovery as ud;
use crate::settings::SettingsStore;
use crate::usbip::proto::TCP_PORT;
use crate::usbip::server::UsbipServer;
use crate::usbip_control::{UsbipControl, UsbipOps};

pub const STATUS_AVAILABLE: &str = "available";
pub const STATUS_CONNECTED: &str = "connected";
pub const STATUS_IN_USE: &str = "in_use";

const REQUEST_ACK_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_RESULT_TIMEOUT: Duration = Duration::from_secs(180);
const RECONNECT_CHECK_INTERVAL: Duration = Duration::from_secs(3);
const RECONNECT_BACKOFF_MS: &[u64] = &[500, 1000, 2000, 4000, 8000];
const DISCONNECT_REFRESH_TIMEOUT: Duration = Duration::from_secs(12);
const RELEASE_OPTIMISTIC_WINDOW: Duration = Duration::from_secs(15);

type Key = (String, String); // (server_mac, dev_id)

pub fn device_key(d: &ud::UsbDeviceEntry) -> String {
    format!("{:04x}:{:04x}:{}", d.id_vendor, d.id_product, d.description)
}

#[derive(Clone)]
struct Connection {
    server_mac: String,
    dev_id: String,
    busid: String,
    port: Option<i32>,
    description: String, // for notices when the device disappears
}

struct Outgoing {
    acked: bool,
    timer: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    owner: String,
}

struct GraceTimer {
    handle: JoinHandle<()>,
    addr: SocketAddr,
    msg: PeerMessage,
}

pub struct Daemon {
    store: Arc<SettingsStore>,
    usbip: Arc<dyn UsbipOps>,
    start_usbip_server: bool,
    ipc: OnceLock<Arc<IpcServer>>,
    listener: Mutex<Option<Arc<DiscoveryListener>>>,
    peer: Mutex<Option<Arc<dyn PeerSender>>>,
    peer_user: String,

    servers: Mutex<HashMap<String, DeviceServer>>, // key: server IP
    last_seen: Mutex<HashMap<String, Instant>>,
    our_ip: Mutex<HashMap<String, String>>,
    host_cache: Mutex<HashMap<String, String>>,
    connections: Mutex<HashMap<Key, Connection>>,
    busids: Mutex<HashMap<String, String>>,
    usbip_server: Mutex<Option<Arc<UsbipServer>>>,
    recently_released: Mutex<HashMap<Key, Instant>>,
    autoconnect_pending: Mutex<HashSet<Key>>,
    reconnecting: Mutex<HashSet<Key>>,
    pending_requests: Mutex<HashMap<Key, (SocketAddr, PeerMessage)>>,
    grace_timers: Mutex<HashMap<Key, GraceTimer>>,
    outgoing: Mutex<HashMap<Key, Outgoing>>,
    connect_lock: tokio::sync::Mutex<()>,
}

fn local_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| hostname())
        .unwrap_or_else(|| "silex".to_string())
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|s| s.trim().to_string())
}

fn mac_array(mac: &[u8]) -> Result<[u8; 6]> {
    if mac.len() != 6 {
        bail!("device MAC unknown");
    }
    let mut m = [0u8; 6];
    m.copy_from_slice(mac);
    Ok(m)
}

impl Daemon {
    pub fn new(store: Arc<SettingsStore>, usbip: Arc<dyn UsbipOps>, start_usbip_server: bool) -> Arc<Self> {
        Arc::new(Self {
            store,
            usbip,
            start_usbip_server,
            ipc: OnceLock::new(),
            listener: Mutex::new(None),
            peer: Mutex::new(None),
            peer_user: local_user(),
            servers: Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
            our_ip: Mutex::new(HashMap::new()),
            host_cache: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            busids: Mutex::new(HashMap::new()),
            usbip_server: Mutex::new(None),
            recently_released: Mutex::new(HashMap::new()),
            autoconnect_pending: Mutex::new(HashSet::new()),
            reconnecting: Mutex::new(HashSet::new()),
            pending_requests: Mutex::new(HashMap::new()),
            grace_timers: Mutex::new(HashMap::new()),
            outgoing: Mutex::new(HashMap::new()),
            connect_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Convenience constructor for production: real usbip control.
    pub fn with_escalation(store: Arc<SettingsStore>, escalation: &str) -> Arc<Self> {
        Self::new(store, Arc::new(UsbipControl::new(escalation)), true)
    }

    fn attach_ipc(self: &Arc<Self>, addr: IpcAddr) -> Arc<IpcServer> {
        let weak = Arc::downgrade(self);
        let handler: Handler = Arc::new(move |method, params, peer| {
            let weak = weak.clone();
            Box::pin(async move {
                match weak.upgrade() {
                    Some(d) => d.handle(method, params, peer).await,
                    None => bail!("daemon shut down"),
                }
            })
        });
        let server = Arc::new(IpcServer::new(addr, handler));
        let _ = self.ipc.set(server.clone());
        server
    }

    /// Start all background loops and serve IPC (runs until the listener stops).
    pub async fn run(self: &Arc<Self>, addr: IpcAddr) -> Result<()> {
        let server = self.attach_ipc(addr);

        // Discovery: persistent listener + consumer + poll loop.
        if let Ok((listener, mut rx)) = DiscoveryListener::start("255.255.255.255").await {
            *self.listener.lock().unwrap() = Some(listener.clone());
            let me = self.clone();
            tokio::spawn(async move {
                while let Some((ip, msg)) = rx.recv().await {
                    me.on_disc_message(ip, msg);
                }
            });
            let me = self.clone();
            tokio::spawn(async move { me.poll_loop().await });
        }

        // Peer endpoint (request use).
        if let Ok((pe, mut prx)) = PeerEndpoint::start().await {
            *self.peer.lock().unwrap() = Some(pe as Arc<dyn PeerSender>);
            let me = self.clone();
            tokio::spawn(async move {
                while let Some((addr, data)) = prx.recv().await {
                    me.on_peer_message(addr, data);
                }
            });
        }

        // Connection watchdog (auto-reconnect).
        let me = self.clone();
        tokio::spawn(async move { me.reconnect_loop().await });

        server.serve().await
    }

    // -- broadcast helpers ------------------------------------------------- //
    fn broadcast(&self, event: &str, data: Value) {
        if let Some(s) = self.ipc.get() {
            s.broadcast(event, data, true);
        }
    }

    fn broadcast_servers(self: &Arc<Self>) {
        let snap = self.servers_snapshot();
        self.broadcast("servers", snap);
    }

    fn notice(&self, mac: &str, dev_id: &str, description: &str, state: &str, kind: &str) {
        self.broadcast(
            "notice",
            json!({"mac": mac, "dev_id": dev_id, "description": description, "kind": kind, "state": state}),
        );
    }

    fn broadcast_req_status(&self, mac: &str, dev_id: &str, state: &str, extra: Value) {
        let mut obj = json!({"mac": mac, "dev_id": dev_id, "state": state});
        if let (Value::Object(o), Value::Object(e)) = (&mut obj, extra) {
            for (k, v) in e {
                o.insert(k, v);
            }
        }
        self.broadcast("access_request_status", obj);
    }

    // -- discovery / state ------------------------------------------------- //
    fn poll_targets(&self) -> Vec<(String, u16)> {
        let s = self.store.get();
        let mut targets = Vec::new();
        if s.search_enabled {
            for a in &s.search_addresses {
                if !a.address.is_empty() {
                    targets.push((a.address.clone(), a.port.unwrap_or(ud::PORT)));
                }
            }
        }
        for ip in self.servers.lock().unwrap().keys() {
            targets.push((ip.clone(), ud::PORT));
        }
        targets
    }

    async fn poll_loop(self: Arc<Self>) {
        loop {
            let targets = self.poll_targets();
            let listener = self.listener.lock().unwrap().clone();
            if let Some(l) = listener {
                l.poll(&targets).await;
            }
            self.prune();
            let interval = self.store.get().refresh_interval.max(1) as u64;
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    }

    async fn refresh_now(self: &Arc<Self>) {
        let targets = self.poll_targets();
        let listener = self.listener.lock().unwrap().clone();
        if let Some(l) = listener {
            l.poll(&targets).await;
        }
    }

    fn on_disc_message(self: &Arc<Self>, ip: String, msg: ud::Message) {
        // `has_device_list` distinguishes a real device list (even when empty -> it
        // replaces) from a beacon without a list (never replace). This lets us detect the
        // *last* device being unplugged (empty list).
        let (srv, has_device_list) = match msg {
            ud::Message::Announcement(a) => {
                let mut s = DeviceServer::new(&ip);
                s.update_from_announcement(&a);
                (Some(s), false)
            }
            ud::Message::DeviceList(l) => {
                let mut s = DeviceServer::new(&ip);
                s.devices = l.devices;
                (Some(s), true)
            }
            _ => (None, false),
        };
        if let Some(s) = srv {
            if self.apply_server(s, has_device_list) {
                self.broadcast_servers();
                self.maybe_autoconnect();
            }
        }
    }

    fn apply_server(&self, srv: DeviceServer, has_device_list: bool) -> bool {
        let ip = srv.ip.clone();
        self.last_seen.lock().unwrap().insert(ip.clone(), Instant::now());
        let mut map = self.servers.lock().unwrap();
        match map.get_mut(&ip) {
            None => {
                map.insert(ip, srv);
                true
            }
            Some(existing) => {
                let before = existing.clone();
                if !srv.mac.is_empty() {
                    existing.mac = srv.mac;
                    existing.model = srv.model;
                    existing.version = srv.version;
                    existing.hostname = srv.hostname;
                    existing.attached_device = srv.attached_device;
                }
                // A real (possibly empty) device list replaces; a beacon never does.
                let update_devices = if has_device_list { true } else { !srv.devices.is_empty() };
                if update_devices {
                    existing.devices = srv.devices;
                }
                *existing != before
            }
        }
    }

    fn prune(self: &Arc<Self>) {
        let ttl = Duration::from_secs((self.store.get().refresh_interval * 3).max(30) as u64);
        let now = Instant::now();
        let mut changed = false;
        {
            let mut servers = self.servers.lock().unwrap();
            let mut last = self.last_seen.lock().unwrap();
            let stale: Vec<String> = servers
                .keys()
                .filter(|ip| now.duration_since(*last.get(*ip).unwrap_or(&now)) > ttl)
                .cloned()
                .collect();
            for ip in stale {
                servers.remove(&ip);
                last.remove(&ip);
                changed = true;
            }
        }
        if changed {
            self.broadcast_servers();
        }
    }

    fn find_server_by_mac(&self, mac: &str) -> Option<DeviceServer> {
        self.servers.lock().unwrap().values().find(|s| s.mac_str() == mac).cloned()
    }

    fn entry_for(&self, mac: &str, dev_id: &str) -> Option<ud::UsbDeviceEntry> {
        self.find_server_by_mac(mac)?.devices.into_iter().find(|d| d.dev_id == dev_id)
    }

    // -- snapshots --------------------------------------------------------- //
    fn our_ip_for(&self, server_ip: &str) -> String {
        let mut c = self.our_ip.lock().unwrap();
        if let Some(v) = c.get(server_ip) {
            return v.clone();
        }
        let v = local_ip_towards(server_ip).unwrap_or_default();
        c.insert(server_ip.to_string(), v.clone());
        v
    }

    fn device_status(&self, srv: &DeviceServer, dev_id: &str, owner_ip: &str) -> &'static str {
        let mac = if srv.mac_str().is_empty() { srv.ip.clone() } else { srv.mac_str() };
        let key = (mac, dev_id.to_string());
        if self.connections.lock().unwrap().contains_key(&key) {
            return STATUS_CONNECTED;
        }
        if !owner_ip.is_empty() {
            if owner_ip == self.our_ip_for(&srv.ip) {
                if let Some(ts) = self.recently_released.lock().unwrap().get(&key) {
                    if ts.elapsed() < RELEASE_OPTIMISTIC_WINDOW {
                        return STATUS_AVAILABLE;
                    }
                }
                return STATUS_CONNECTED;
            }
            return STATUS_IN_USE;
        }
        STATUS_AVAILABLE
    }

    fn owner_host(self: &Arc<Self>, ip: &str) -> String {
        if ip.is_empty() {
            return String::new();
        }
        {
            let mut cache = self.host_cache.lock().unwrap();
            if let Some(name) = cache.get(ip) {
                return name.clone();
            }
            cache.insert(ip.to_string(), String::new()); // mark in progress
        }
        let me = self.clone();
        let ip_owned = ip.to_string();
        tokio::spawn(async move {
            let ip2 = ip_owned.clone();
            let name = tokio::task::spawn_blocking(move || {
                ip2.parse::<std::net::IpAddr>().ok().and_then(|a| dns_lookup::lookup_addr(&a).ok())
            })
            .await
            .ok()
            .flatten();
            if let Some(full) = name {
                let short = full.split('.').next().unwrap_or(&full).to_string();
                if !short.is_empty() {
                    me.host_cache.lock().unwrap().insert(ip_owned, short);
                    me.broadcast_servers();
                }
            }
        });
        String::new()
    }

    fn snapshot_device(self: &Arc<Self>, srv: &DeviceServer, d: &ud::LogicalDevice) -> Value {
        let key = device_key(d.primary());
        let settings = self.store.get();
        let rule = settings.rule_for(&srv.mac_str(), &key);
        let owner_ip = d.owner_ip();
        json!({
            "dev_id": d.dev_id(),
            "description": d.description().trim(),
            "alias": rule.map(|r| r.alias.clone()).unwrap_or_default(),
            "launch_app": rule.map(|r| r.launch_app.clone()).unwrap_or_default(),
            "disconnect_on_app_close": rule.map(|r| r.disconnect_on_app_close).unwrap_or(false),
            "id_vendor": d.id_vendor(),
            "id_product": d.id_product(),
            "tag": d.tag(),
            "speed": d.speed(),
            "type": d.device_type(),
            "usb_class": d.representative_class(),
            "composite": d.is_composite(),
            "key": key,
            "status": self.device_status(srv, d.dev_id(), &owner_ip),
            "owner_ip": owner_ip,
            "owner_host": self.owner_host(&owner_ip),
        })
    }

    fn snapshot_server(self: &Arc<Self>, srv: &DeviceServer) -> Value {
        let ipv6 = if srv.mac.len() == 6 { ud::mac_to_link_local(&srv.mac) } else { String::new() };
        // Composite devices (multiple interfaces) are collapsed into ONE row like SX
        // Virtual Link; the representative class drives type + icon.
        let devices: Vec<Value> =
            ud::logical_devices(&srv.devices).iter().map(|d| self.snapshot_device(srv, d)).collect();
        json!({
            "ip": srv.ip,
            "mac": srv.mac_str(),
            "model": srv.model,
            "version": srv.version,
            "hostname": srv.hostname,
            "ipv6": ipv6,
            "devices": devices,
        })
    }

    fn servers_snapshot(self: &Arc<Self>) -> Value {
        let servers: Vec<DeviceServer> = self.servers.lock().unwrap().values().cloned().collect();
        let list: Vec<Value> = servers.iter().map(|s| self.snapshot_server(s)).collect();
        json!({ "servers": list })
    }

    // -- IPC dispatch ------------------------------------------------------ //
    async fn handle(self: Arc<Self>, method: String, params: Value, peer: Arc<Peer>) -> Result<Value> {
        let sp = |k: &str| -> Result<String> {
            params.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| anyhow::anyhow!("missing parameter {k:?}"))
        };
        match method.as_str() {
            "hello" => Ok(json!({"name": "silex-daemon", "version": 1})),
            "list_servers" => Ok(self.servers_snapshot()),
            "subscribe" => {
                peer.subscribe();
                peer.send_event("servers", self.servers_snapshot());
                Ok(json!({"subscribed": true}))
            }
            "refresh" => {
                self.refresh_now().await;
                Ok(self.servers_snapshot())
            }
            "get_settings" => Ok(self.store.get().to_value()),
            "set_settings" => {
                let s = self.store.update(params.clone())?;
                self.broadcast("settings", s.to_value());
                self.maybe_autoconnect();
                Ok(s.to_value())
            }
            "set_device_rule" => {
                let mac = sp("mac")?;
                let key = sp("key")?;
                let mut updates = Map::new();
                if let Value::Object(o) = &params {
                    for (k, v) in o {
                        if k != "mac" && k != "key" {
                            updates.insert(k.clone(), v.clone());
                        }
                    }
                }
                let s = self.store.set_device_rule(&mac, &key, &updates)?;
                self.broadcast("settings", s.to_value());
                self.maybe_autoconnect();
                Ok(s.to_value())
            }
            "connect" => self.connect(&sp("mac")?, &sp("dev_id")?).await,
            "disconnect" => self.disconnect(&sp("mac")?, &sp("dev_id")?).await,
            "request_access" => self.request_access(&sp("mac")?, &sp("dev_id")?).await,
            "respond_access_request" => {
                let accept = params.get("accept").and_then(Value::as_bool).unwrap_or(false);
                self.respond_access_request(&sp("mac")?, &sp("dev_id")?, accept).await
            }
            other => bail!("unknown method: {other:?}"),
        }
    }

    // -- auto-connect ------------------------------------------------------ //
    fn maybe_autoconnect(self: &Arc<Self>) {
        let settings = self.store.get();
        let servers: Vec<DeviceServer> = self.servers.lock().unwrap().values().cloned().collect();
        for srv in servers {
            let mac = srv.mac_str();
            if mac.is_empty() {
                continue;
            }
            for d in &srv.devices {
                // Connect a composite device only ONCE (via the leading interface M0).
                if !matches!(d.interface_index(), None | Some(0)) {
                    continue;
                }
                if !d.owner_ip.is_empty() {
                    continue;
                }
                let key = (mac.clone(), d.dev_id.clone());
                if self.connections.lock().unwrap().contains_key(&key) {
                    continue;
                }
                {
                    let mut pending = self.autoconnect_pending.lock().unwrap();
                    if pending.contains(&key) {
                        continue;
                    }
                    if !settings.should_auto_connect(&mac, &device_key(d)) {
                        continue;
                    }
                    pending.insert(key.clone());
                }
                let me = self.clone();
                let (m, dev) = key.clone();
                tokio::spawn(async move {
                    let _ = me.connect(&m, &dev).await;
                    me.autoconnect_pending.lock().unwrap().remove(&(m, dev));
                });
            }
        }
    }

    // -- connect / disconnect ---------------------------------------------- //
    async fn ensure_export(self: &Arc<Self>, srv: &DeviceServer) -> Result<()> {
        {
            let mut b = self.busids.lock().unwrap();
            if b.is_empty() {
                // One busid per LOGICAL device (a composite = one entry, the primary
                // dev_id). SilexDevice connects the sub-interfaces itself on import -> they
                // must NOT be separately attachable (double reservation otherwise).
                for (i, d) in ud::logical_devices(&srv.devices).iter().enumerate() {
                    b.insert(d.primary().dev_id.clone(), format!("1-{}", i + 1));
                }
            }
        }
        if self.start_usbip_server && self.usbip_server.lock().unwrap().is_none() {
            let busids = self.busids.lock().unwrap().clone();
            let devices: Vec<(String, String)> = srv
                .devices
                .iter()
                .filter_map(|d| busids.get(&d.dev_id).map(|b| (b.clone(), d.dev_id.clone())))
                .collect();
            let server = Arc::new(UsbipServer::new(
                &srv.ip,
                mac_array(&srv.mac)?,
                None,
                ("127.0.0.1".to_string(), TCP_PORT),
                ud::PORT,
                devices,
            ));
            server.bind().await?;
            let s = server.clone();
            tokio::spawn(async move {
                let _ = s.serve().await;
            });
            *self.usbip_server.lock().unwrap() = Some(server);
        }
        Ok(())
    }

    async fn connect(self: &Arc<Self>, mac: &str, dev_id: &str) -> Result<Value> {
        self.cancel_grace_timer(mac, dev_id);
        let _guard = self.connect_lock.lock().await;
        let srv = self.find_server_by_mac(mac).ok_or_else(|| anyhow::anyhow!("device server {mac} not found"))?;
        if !srv.devices.iter().any(|d| d.dev_id == dev_id) {
            bail!("{dev_id} not present at {mac}");
        }
        let key = (mac.to_string(), dev_id.to_string());
        if let Some(c) = self.connections.lock().unwrap().get(&key) {
            return Ok(json!({"connected": true, "busid": c.busid, "port": c.port}));
        }
        self.ensure_export(&srv).await?;
        let busid = self
            .busids
            .lock()
            .unwrap()
            .get(dev_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no busid for {dev_id}"))?;
        self.usbip.ensure_module()?;
        let before = self.usbip.used_ports();
        self.usbip.attach(&busid)?;
        // The new vhci port may appear in `usbip port` shortly after attach.
        let mut port = None;
        for _ in 0..20 {
            let new: Vec<i32> = self.usbip.used_ports().difference(&before).copied().collect();
            if let Some(&p) = new.iter().min() {
                port = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let desc = srv
            .devices
            .iter()
            .find(|d| d.dev_id == dev_id)
            .map(|d| d.description.trim().to_string())
            .unwrap_or_else(|| dev_id.to_string());
        self.connections.lock().unwrap().insert(
            key.clone(),
            Connection { server_mac: mac.to_string(), dev_id: dev_id.to_string(), busid: busid.clone(), port, description: desc },
        );
        self.recently_released.lock().unwrap().remove(&key);
        drop(_guard);
        self.broadcast_servers();
        Ok(json!({"connected": true, "busid": busid, "port": port}))
    }

    async fn disconnect(self: &Arc<Self>, mac: &str, dev_id: &str) -> Result<Value> {
        let key = (mac.to_string(), dev_id.to_string());
        let conn = self.connections.lock().unwrap().remove(&key);
        let conn = match conn {
            Some(c) => c,
            None => return Ok(json!({"disconnected": true, "note": "was not connected"})),
        };
        if let Some(port) = conn.port {
            self.usbip.detach(port)?;
        }
        self.recently_released.lock().unwrap().insert(key.clone(), Instant::now());
        self.broadcast_servers();
        let me = self.clone();
        let (m, d) = key;
        tokio::spawn(async move { me.refresh_burst(&m, &d).await });
        Ok(json!({"disconnected": true}))
    }

    async fn refresh_burst(self: &Arc<Self>, mac: &str, dev_id: &str) {
        let deadline = Instant::now() + DISCONNECT_REFRESH_TIMEOUT;
        let mut delay = Duration::from_millis(400);
        while Instant::now() < deadline {
            tokio::time::sleep(delay).await;
            self.refresh_now().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            if self.connections.lock().unwrap().contains_key(&(mac.to_string(), dev_id.to_string())) {
                return;
            }
            match self.entry_for(mac, dev_id) {
                Some(e) if e.owner_ip.is_empty() => return,
                None => return,
                _ => {}
            }
            delay = (delay.mul_f32(1.4)).min(Duration::from_secs(2));
        }
    }

    // -- auto-reconnect ---------------------------------------------------- //
    async fn reconnect_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(RECONNECT_CHECK_INTERVAL).await;
            // First prune unplugged devices (no reconnect for what is gone), then handle
            // lost tunnels.
            self.prune_phantoms();
            self.check_connections();
        }
    }

    // -- phantom cleanup (device disappeared at the server) ---------------- //
    /// True while (mac, dev_id) is still listed in discovery. False once the device is gone
    /// from the device list (unplugged/ejected) OR the whole server has disappeared.
    fn device_present(&self, mac: &str, dev_id: &str) -> bool {
        match self.find_server_by_mac(mac) {
            Some(srv) => srv.devices.iter().any(|d| d.dev_id == dev_id),
            None => false,
        }
    }

    /// Detach vhci devices whose USB device disappeared at the server. Counterpart to
    /// `check_connections`: there the vhci *port* vanishes (tunnel dead -> reconnect); here
    /// the port stays but the *device* is gone from discovery -> reconnect is pointless, so
    /// clean up the port and drop the connection (no reconnect).
    fn prune_phantoms(self: &Arc<Self>) {
        let conns: Vec<Connection> = self.connections.lock().unwrap().values().cloned().collect();
        for c in conns {
            if self.device_present(&c.server_mac, &c.dev_id) {
                continue;
            }
            self.handle_device_gone(c);
        }
    }

    /// A connected device vanished from discovery (unplugged): detach the phantom vhci
    /// port, drop the connection, tell the GUI. No reconnect.
    fn handle_device_gone(self: &Arc<Self>, conn: Connection) {
        let key = (conn.server_mac.clone(), conn.dev_id.clone());
        {
            let mut conns = self.connections.lock().unwrap();
            match conns.get(&key) {
                Some(c) if c.port == conn.port => {}
                _ => return, // disconnected/replaced meanwhile
            }
            conns.remove(&key);
        }
        if let Some(port) = conn.port {
            let _ = self.usbip.detach(port); // clean up the leftover vhci device
        }
        let desc = if conn.description.is_empty() { conn.dev_id.clone() } else { conn.description.clone() };
        self.broadcast_servers();
        self.notice(&conn.server_mac, &conn.dev_id, &desc, "removed", "device");
    }

    fn check_connections(self: &Arc<Self>) {
        let conns: Vec<Connection> = self
            .connections
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.port.is_some())
            .cloned()
            .collect();
        if conns.is_empty() {
            return;
        }
        let ports = self.usbip.used_ports();
        for c in conns {
            if let Some(p) = c.port {
                if !ports.contains(&p) {
                    self.handle_tunnel_lost(c);
                }
            }
        }
    }

    fn handle_tunnel_lost(self: &Arc<Self>, conn: Connection) {
        let key = (conn.server_mac.clone(), conn.dev_id.clone());
        {
            let mut conns = self.connections.lock().unwrap();
            match conns.get(&key) {
                Some(c) if c.port == conn.port => {}
                _ => return, // replaced/removed meanwhile
            }
            conns.remove(&key);
        }
        {
            let mut rec = self.reconnecting.lock().unwrap();
            if rec.contains(&key) {
                return;
            }
            rec.insert(key.clone());
        }
        let entry = self.entry_for(&conn.server_mac, &conn.dev_id);
        let desc = entry.map(|e| e.description.trim().to_string()).unwrap_or_else(|| conn.dev_id.clone());
        self.broadcast_servers();
        if self.should_reconnect(&conn.server_mac, &conn.dev_id) {
            self.notice(&conn.server_mac, &conn.dev_id, &desc, "lost", "reconnect");
            let me = self.clone();
            tokio::spawn(async move { me.reconnect_worker(conn.server_mac, conn.dev_id, desc).await });
        } else {
            self.reconnecting.lock().unwrap().remove(&key);
            self.notice(&conn.server_mac, &conn.dev_id, &desc, "disconnected", "reconnect");
        }
    }

    fn should_reconnect(&self, mac: &str, dev_id: &str) -> bool {
        let entry = self.entry_for(mac, dev_id);
        match entry {
            Some(e) => {
                let key = device_key(&e);
                self.store.get().rule_for(mac, &key).map(|r| r.reconnect_on_error).unwrap_or(true)
            }
            None => true,
        }
    }

    async fn reconnect_worker(self: Arc<Self>, mac: String, dev_id: String, desc: String) {
        let key = (mac.clone(), dev_id.clone());
        for ms in RECONNECT_BACKOFF_MS {
            tokio::time::sleep(Duration::from_millis(*ms)).await;
            let srv = self.find_server_by_mac(&mac);
            let entry = srv.as_ref().and_then(|s| s.devices.iter().find(|d| d.dev_id == dev_id).cloned());
            let entry = match entry {
                Some(e) => e,
                None => continue, // not rediscovered yet
            };
            if !entry.owner_ip.is_empty() {
                if let Some(s) = &srv {
                    if entry.owner_ip != self.our_ip_for(&s.ip) {
                        self.notice(&mac, &dev_id, &desc, "taken", "reconnect");
                        self.reconnecting.lock().unwrap().remove(&key);
                        return;
                    }
                }
            }
            if self.connect(&mac, &dev_id).await.is_ok() {
                self.notice(&mac, &dev_id, &desc, "reconnected", "reconnect");
                self.reconnecting.lock().unwrap().remove(&key);
                return;
            }
        }
        self.notice(&mac, &dev_id, &desc, "failed", "reconnect");
        self.reconnecting.lock().unwrap().remove(&key);
    }

    // -- peer: receive side ------------------------------------------------ //
    fn on_peer_message(self: &Arc<Self>, addr: SocketAddr, data: Vec<u8>) {
        let msg = match peerproto::parse(&data) {
            Ok(m) => m,
            Err(_) => return,
        };
        if msg.msg_type == peerproto::TYPE_REQUEST {
            self.handle_peer_request(addr, msg);
        } else {
            self.handle_peer_response(msg);
        }
    }

    fn peer_send(&self, addr: SocketAddr, msg_type: u16, msg: &PeerMessage) {
        if let Some(p) = self.peer.lock().unwrap().clone() {
            p.send(addr, peerproto::build_from(msg_type, msg, &self.peer_user));
        }
    }

    fn find_held_for(&self, msg: &PeerMessage) -> Option<(String, String, String)> {
        let mac = msg.mac_str();
        let srv = self.find_server_by_mac(&mac)?;
        for d in &srv.devices {
            if d.id_vendor == msg.id_vendor
                && d.id_product == msg.id_product
                && d.description.trim() == msg.description.trim()
                && self.connections.lock().unwrap().contains_key(&(mac.clone(), d.dev_id.clone()))
            {
                return Some((mac, d.dev_id.clone(), device_key(d)));
            }
        }
        None
    }

    fn find_device_by_fields(&self, msg: &PeerMessage) -> Option<(String, String)> {
        let mac = msg.mac_str();
        let srv = self.find_server_by_mac(&mac)?;
        for d in &srv.devices {
            if d.id_vendor == msg.id_vendor
                && d.id_product == msg.id_product
                && d.description.trim() == msg.description.trim()
            {
                return Some((mac, d.dev_id.clone()));
            }
        }
        None
    }

    fn handle_peer_request(self: &Arc<Self>, addr: SocketAddr, msg: PeerMessage) {
        self.peer_send(addr, peerproto::TYPE_ACK, &msg); // immediate ACK
        let (mac, dev_id, key) = match self.find_held_for(&msg) {
            Some(v) => v,
            None => return, // we do not hold it
        };
        let rule = self.store.get().rule_for(&mac, &key).cloned();
        if let Some(r) = rule {
            if r.auto_disconnect_on_request {
                let timeout = r.auto_disconnect_timeout.max(0) as u64;
                if timeout == 0 {
                    let me = self.clone();
                    let m = mac.clone();
                    let d = dev_id.clone();
                    tokio::spawn(async move { me.accept_peer_request(addr, msg, &m, &d).await });
                } else {
                    self.arm_grace_timer(&mac, &dev_id, addr, msg.clone(), Duration::from_secs(timeout));
                    self.notice(&mac, &dev_id, msg.description.trim(), "auto_release_pending", "peer");
                }
                return;
            }
        }
        // Otherwise ask the user via the GUI (prompt reacts over IPC).
        self.pending_requests.lock().unwrap().insert((mac.clone(), dev_id.clone()), (addr, msg.clone()));
        self.broadcast(
            "access_request",
            json!({
                "mac": mac, "dev_id": dev_id, "key": key,
                "description": msg.description.trim(),
                "requester": msg.user, "requester_ip": addr.ip().to_string(),
            }),
        );
    }

    async fn accept_peer_request(self: &Arc<Self>, addr: SocketAddr, msg: PeerMessage, mac: &str, dev_id: &str) {
        self.peer_send(addr, peerproto::TYPE_ACCEPTED, &msg);
        let _ = self.disconnect(mac, dev_id).await;
        self.peer_send(addr, peerproto::TYPE_RELEASED, &msg);
    }

    fn arm_grace_timer(self: &Arc<Self>, mac: &str, dev_id: &str, addr: SocketAddr, msg: PeerMessage, timeout: Duration) {
        let key = (mac.to_string(), dev_id.to_string());
        let me = self.clone();
        let (m, d) = key.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            me.grace_fire(&m, &d).await;
        });
        let mut t = self.grace_timers.lock().unwrap();
        if let Some(old) = t.remove(&key) {
            old.handle.abort();
        }
        t.insert(key, GraceTimer { handle, addr, msg });
    }

    async fn grace_fire(self: &Arc<Self>, mac: &str, dev_id: &str) {
        let entry = self.grace_timers.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string()));
        if let Some(g) = entry {
            self.accept_peer_request(g.addr, g.msg.clone(), mac, dev_id).await;
            self.notice(mac, dev_id, g.msg.description.trim(), "auto_released", "peer");
        }
    }

    fn cancel_grace_timer(&self, mac: &str, dev_id: &str) {
        if let Some(g) = self.grace_timers.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string())) {
            g.handle.abort();
        }
    }

    async fn respond_access_request(self: &Arc<Self>, mac: &str, dev_id: &str, accept: bool) -> Result<Value> {
        let pending = self.pending_requests.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string()));
        let (addr, msg) = match pending {
            Some(v) => v,
            None => return Ok(json!({"ok": false, "note": "no open request"})),
        };
        if accept {
            self.accept_peer_request(addr, msg, mac, dev_id).await;
        } else {
            self.peer_send(addr, peerproto::TYPE_DENIED, &msg);
        }
        Ok(json!({"ok": true, "accepted": accept}))
    }

    // -- peer: send side --------------------------------------------------- //
    fn arm_request_timer(self: &Arc<Self>, mac: &str, dev_id: &str, delay: Duration) {
        let key = (mac.to_string(), dev_id.to_string());
        let me = self.clone();
        let (m, d) = key.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            me.on_request_timeout(&m, &d);
        });
        let mut o = self.outgoing.lock().unwrap();
        if let Some(out) = o.get_mut(&key) {
            if let Some(t) = out.timer.take() {
                t.abort();
            }
            out.timer = Some(handle);
        } else {
            handle.abort();
        }
    }

    fn clear_outgoing(&self, mac: &str, dev_id: &str) {
        if let Some(out) = self.outgoing.lock().unwrap().remove(&(mac.to_string(), dev_id.to_string())) {
            if let Some(t) = out.timer {
                t.abort();
            }
        }
    }

    async fn request_access(self: &Arc<Self>, mac: &str, dev_id: &str) -> Result<Value> {
        let srv = self.find_server_by_mac(mac).ok_or_else(|| anyhow::anyhow!("device server {mac} not found"))?;
        let entry = srv
            .devices
            .iter()
            .find(|d| d.dev_id == dev_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{dev_id} not present at {mac}"))?;
        let owner = entry.owner_ip.clone();
        if owner.is_empty() {
            bail!("device is not held by another host");
        }
        if owner == self.our_ip_for(&srv.ip) {
            bail!("device is already held by us");
        }
        if self.peer.lock().unwrap().is_none() {
            bail!("peer endpoint not available");
        }
        let pkt = peerproto::build(
            peerproto::TYPE_REQUEST,
            mac_array(&srv.mac)?,
            entry.id_vendor,
            entry.id_product,
            &peerproto::request_description(&entry.description),
            &entry.tag,
            &self.peer_user,
        );
        self.outgoing.lock().unwrap().insert(
            (mac.to_string(), dev_id.to_string()),
            Outgoing { acked: false, timer: None, owner: owner.clone() },
        );
        self.arm_request_timer(mac, dev_id, REQUEST_ACK_TIMEOUT);
        let addr: SocketAddr = format!("{}:{}", owner, peerproto::PORT).parse()?;
        if let Some(p) = self.peer.lock().unwrap().clone() {
            p.send(addr, pkt);
        }
        self.broadcast_req_status(mac, dev_id, "sent", json!({"owner_ip": owner}));
        Ok(json!({"requested": true, "owner_ip": owner}))
    }

    fn handle_peer_response(self: &Arc<Self>, msg: PeerMessage) {
        let (mac, dev_id) = match self.find_device_by_fields(&msg) {
            Some(v) => v,
            None => return,
        };
        let key = (mac.clone(), dev_id.clone());
        let state;
        let mut do_rearm = false;
        let mut remove = false;
        let mut released = false;
        {
            let mut o = self.outgoing.lock().unwrap();
            let out = match o.get_mut(&key) {
                Some(x) => x,
                None => return,
            };
            match msg.msg_type {
                t if t == peerproto::TYPE_ACK => {
                    out.acked = true;
                    do_rearm = true;
                    state = "acked";
                }
                t if t == peerproto::TYPE_ACCEPTED => state = "accepted",
                // REJECTED: the holder found no matching device - same outcome as DENIED.
                t if t == peerproto::TYPE_DENIED || t == peerproto::TYPE_REJECTED => {
                    state = "denied";
                    remove = true;
                }
                t if t == peerproto::TYPE_RELEASED => {
                    state = "released";
                    remove = true;
                    released = true;
                }
                _ => return,
            }
            if remove {
                if let Some(t) = out.timer.take() {
                    t.abort();
                }
            }
        }
        if remove {
            self.outgoing.lock().unwrap().remove(&key);
        }
        if do_rearm {
            self.arm_request_timer(&mac, &dev_id, REQUEST_RESULT_TIMEOUT);
        }
        self.broadcast_req_status(&mac, &dev_id, state, json!({"holder": msg.user}));
        if released {
            let me = self.clone();
            tokio::spawn(async move { me.autotake(&mac, &dev_id).await });
        }
    }

    fn on_request_timeout(self: &Arc<Self>, mac: &str, dev_id: &str) {
        {
            let contains = self.outgoing.lock().unwrap().contains_key(&(mac.to_string(), dev_id.to_string()));
            if !contains {
                return;
            }
        }
        self.clear_outgoing(mac, dev_id);
        self.broadcast_req_status(mac, dev_id, "no_response", Value::Null);
    }

    async fn autotake(self: &Arc<Self>, mac: &str, dev_id: &str) {
        self.broadcast_req_status(mac, dev_id, "connecting", Value::Null);
        match self.connect(mac, dev_id).await {
            Ok(_) => self.broadcast_req_status(mac, dev_id, "connected", Value::Null),
            Err(e) => self.broadcast_req_status(mac, dev_id, "error", json!({"note": e.to_string()})),
        }
    }
}

#[cfg(test)]
mod tests;
