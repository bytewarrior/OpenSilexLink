//! Minimal USB/IP server that exports a Silex USB device to the local kernel.
//!
//! The Linux client (`usbip list -r 127.0.0.1`, `usbip attach -r 127.0.0.1 -b <busid>`)
//! connects here; `vhci-hcd` imports the device and enumerates it over the URB stream,
//! which we translate 1:1 into the Silex tunnel.
//!
//! Concurrency: the reader routes each URB (via [`SilexDevice::session_for`]) to the queue
//! of its connection, so control, iso and interrupt do not block each other. A plain
//! bulk/control connection runs a synchronous worker; an iso connection runs a pipelined
//! worker; a connection with an interrupt endpoint runs a multiplex sender+reader (the
//! device holds interrupt-IN as a long-poll, so responses are matched to requests by seq).
//! UNLINK is answered immediately; a send lock serializes RET_SUBMIT/RET_UNLINK.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::timeout;

use super::device::SilexDevice;
use super::proto::{self as up, OpHeader, UsbDevice, UsbInterface, UsbipCmdSubmit, UsbipCmdUnlink, UsbipRetSubmit, UsbipRetUnlink};
use super::session::SilexSession;
use super::translate::{
    build_request, fill_ret_from_response, iso_fill_ret, iso_fill_ret_out, iso_out_request_params,
    iso_request_params, ret_for, submit_to_silex,
};

const EPIPE: i32 = -32;
const KEEPALIVE: Duration = Duration::from_secs(10);
// Max iso requests kept "in flight" (a deep, continuous pipeline buffer like SX Virtual
// Link, which keeps ~200 ahead). The kernel actually bounds the depth (submitted iso URBs);
// this is just a generous ceiling against unbounded look-ahead.
const ISO_MAX_INFLIGHT: usize = 64;

#[derive(Clone)]
struct DeviceSlot {
    busid: String,
    dev_id: String,
    busnum: u32,
    devnum: u32,
}

pub struct UsbipServer {
    server_ip: String,
    mac: [u8; 6],
    host_ip: Option<String>,
    bind: (String, u16),
    device_port: u16,
    devices: Vec<DeviceSlot>,
    active: Arc<Mutex<Vec<Arc<SilexDevice>>>>,
    listener: Mutex<Option<TcpListener>>,
}

impl UsbipServer {
    /// `devices` is a list of (busid, dev_id). busnum=1, devnum counts up from 2.
    pub fn new(
        server_ip: &str,
        mac: [u8; 6],
        host_ip: Option<String>,
        bind: (String, u16),
        device_port: u16,
        devices: Vec<(String, String)>,
    ) -> Self {
        let devices = devices
            .into_iter()
            .enumerate()
            .map(|(i, (busid, dev_id))| DeviceSlot {
                busid,
                dev_id,
                busnum: 1,
                devnum: 2 + i as u32,
            })
            .collect();
        Self {
            server_ip: server_ip.to_string(),
            mac,
            host_ip,
            bind,
            device_port,
            devices,
            active: Arc::new(Mutex::new(Vec::new())),
            listener: Mutex::new(None),
        }
    }

    /// Bind the listening socket up front so callers can attach without racing.
    /// Returns the bound port.
    pub async fn bind(&self) -> Result<u16> {
        let listener = TcpListener::bind((self.bind.0.as_str(), self.bind.1)).await?;
        let port = listener.local_addr()?.port();
        *self.listener.lock().await = Some(listener);
        Ok(port)
    }

    fn make_session(&self, dev_id: &str) -> Arc<SilexSession> {
        Arc::new(SilexSession::new(
            &self.server_ip,
            self.mac,
            dev_id,
            self.host_ip.clone(),
            self.device_port,
            KEEPALIVE,
        ))
    }

    /// A sub-interface session of a composite device: same sel index as the primary.
    fn make_sub_session(&self, dev_id: &str, dev_index: u8) -> Arc<SilexSession> {
        Arc::new(SilexSession::with_dev_index(
            &self.server_ip,
            self.mac,
            dev_id,
            self.host_ip.clone(),
            self.device_port,
            KEEPALIVE,
            Some(dev_index),
        ))
    }

    fn slot(&self, busid: &str) -> Option<&DeviceSlot> {
        self.devices.iter().find(|d| d.busid == busid)
    }

    /// Disconnect all active devices cleanly (against stuck reservations).
    pub async fn shutdown(&self) {
        let devices = { self.active.lock().await.clone() };
        for dev in devices {
            dev.disconnect_all().await;
        }
    }

    pub async fn serve(self: Arc<Self>) -> Result<()> {
        let listener = match self.listener.lock().await.take() {
            Some(l) => l,
            None => TcpListener::bind((self.bind.0.as_str(), self.bind.1)).await?,
        };
        let devs: Vec<String> = self.devices.iter().map(|d| format!("{}->{}", d.busid, d.dev_id)).collect();
        tracing::info!("USB/IP server on {}:{} (@ {}: {})", self.bind.0, self.bind.1, self.server_ip, devs.join(", "));
        loop {
            let (conn, addr) = listener.accept().await?;
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle(conn).await {
                    tracing::warn!("[{addr}] connection ended: {e}");
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, mut conn: TcpStream) -> Result<()> {
        let mut op = [0u8; OpHeader::SIZE];
        conn.read_exact(&mut op).await?;
        let header = OpHeader::parse(&op)?;
        match header.code {
            up::OP_REQ_DEVLIST => self.handle_devlist(conn).await,
            up::OP_REQ_IMPORT => self.handle_import(conn).await,
            other => bail!("unsupported OP code {other:#06x}"),
        }
    }

    async fn read_device_info(
        &self,
        sess: &SilexSession,
        slot: &DeviceSlot,
    ) -> Result<UsbDevice> {
        let desc = sess.get_device_descriptor().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        if desc.len() < 18 {
            bail!("short device descriptor");
        }
        let cfg = sess.control_in(0x80, 0x06, 0x0200, 0x0000, 9).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let bcd_usb = u16::from_le_bytes([desc[2], desc[3]]);
        Ok(UsbDevice {
            path: format!("/silex/{}", slot.dev_id),
            busid: slot.busid.clone(),
            busnum: slot.busnum,
            devnum: slot.devnum,
            speed: speed_from_bcd(bcd_usb),
            id_vendor: u16::from_le_bytes([desc[8], desc[9]]),
            id_product: u16::from_le_bytes([desc[10], desc[11]]),
            bcd_device: u16::from_le_bytes([desc[12], desc[13]]),
            b_device_class: desc[4],
            b_device_subclass: desc[5],
            b_device_protocol: desc[6],
            b_configuration_value: if cfg.len() > 5 { cfg[5] } else { 1 },
            b_num_configurations: desc[17],
            b_num_interfaces: if cfg.len() > 4 { cfg[4] } else { 1 },
        })
    }

    async fn probe_device(&self, slot: &DeviceSlot) -> Result<UsbDevice> {
        let sess = self.make_session(&slot.dev_id);
        sess.connect().await?;
        let info = self.read_device_info(&sess, slot).await;
        sess.disconnect().await;
        info
    }

    /// Read the full config descriptor and build the composite router (one session per
    /// sub-interface). A simple device yields a single-session router.
    async fn build_device(self: &Arc<Self>, primary: Arc<SilexSession>, slot: &DeviceSlot) -> Result<SilexDevice> {
        let full_cfg = primary.get_config_descriptor().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let me = self.clone();
        let dev_index = primary.dev_index;
        let make_sub = move |did: &str| me.make_sub_session(did, dev_index);
        SilexDevice::build(primary, &full_cfg, &slot.dev_id, make_sub).await
    }

    async fn handle_devlist(self: Arc<Self>, mut conn: TcpStream) -> Result<()> {
        let mut entries = Vec::new();
        for slot in &self.devices {
            let dev = self.probe_device(slot).await?;
            let ifaces = vec![UsbInterface {
                b_interface_class: if dev.b_device_class != 0 { dev.b_device_class } else { 8 },
                b_interface_subclass: if dev.b_device_subclass != 0 { dev.b_device_subclass } else { 6 },
                b_interface_protocol: 80,
            }];
            entries.push((dev, ifaces));
        }
        conn.write_all(&up::build_rep_devlist(&entries)).await?;
        Ok(())
    }

    async fn handle_import(self: Arc<Self>, mut conn: TcpStream) -> Result<()> {
        let mut busid_buf = [0u8; 32];
        conn.read_exact(&mut busid_buf).await?;
        let busid = crate::wire::cstr_field(&busid_buf, 0, Some(32));
        let slot = match self.slot(&busid) {
            Some(s) => s.clone(),
            None => {
                conn.write_all(&up::build_rep_import(None)).await?;
                return Ok(());
            }
        };
        // One session for probe AND the URB loop - no disconnect/reconnect, otherwise the
        // device rejects the immediate reconnect ("NG"). For composite devices,
        // SilexDevice::build connects an extra session per sub-interface.
        let sess = self.make_session(&slot.dev_id);
        let dev = match async {
            sess.connect().await?;
            self.read_device_info(&sess, &slot).await
        }
        .await
        {
            Ok(dev) => dev,
            Err(e) => {
                tracing::warn!("import failed (connect/probe): {e}");
                sess.disconnect().await;
                conn.write_all(&up::build_rep_import(None)).await?;
                return Ok(());
            }
        };
        let device = match self.build_device(sess.clone(), &slot).await {
            Ok(d) => Arc::new(d),
            Err(e) => {
                tracing::warn!("import failed (composite build): {e}");
                sess.disconnect().await;
                conn.write_all(&up::build_rep_import(None)).await?;
                return Ok(());
            }
        };
        self.active.lock().await.push(device.clone());
        // Start keepalive on every connection; the pump stops it for iso/mux connections
        // (which keepalive themselves) and leaves it running on plain connections.
        for s in device.sessions() {
            s.start_keepalive();
        }
        let result = async {
            conn.write_all(&up::build_rep_import(Some(&dev))).await?;
            urb_pump(conn, device.clone()).await
        }
        .await;
        {
            let mut act = self.active.lock().await;
            act.retain(|d| !Arc::ptr_eq(d, &device));
        }
        device.disconnect_all().await;
        result
    }
}

fn speed_from_bcd(bcd_usb: u16) -> u32 {
    // usb_device_speed: 2=full, 3=high
    if bcd_usb >= 0x0200 {
        3
    } else {
        2
    }
}

/// A request in flight on a mux connection: a real URB, or a keepalive (discarded).
enum Inflight {
    Cmd(UsbipCmdSubmit),
    Keepalive,
}

/// Shared state of one imported device's URB pump.
struct Pump {
    wr: Mutex<OwnedWriteHalf>, // USB/IP socket write half (send RET_SUBMIT/RET_UNLINK)
    cancelled: StdMutex<HashSet<u32>>,
    alive: AtomicBool,
    shutdown: Notify, // breaks the reader when a worker hits a fatal tunnel error
}

impl Pump {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn send(&self, data: &[u8]) -> std::io::Result<()> {
        self.wr.lock().await.write_all(data).await
    }

    /// True when the URB should be processed (not cancelled meanwhile).
    fn take(&self, seqnum: u32) -> bool {
        !self.cancelled.lock().unwrap().remove(&seqnum)
    }

    /// Send RET_SUBMIT (with a cancel re-check). false => end the connection/worker.
    async fn complete(&self, ret: &UsbipRetSubmit) -> bool {
        if self.cancelled.lock().unwrap().remove(&ret.seqnum) {
            return true;
        }
        if self.send(&ret.to_bytes()).await.is_err() {
            self.alive.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    async fn abort_tunnel(&self, cmd: &UsbipCmdSubmit, e: impl std::fmt::Display) {
        tracing::warn!(
            "URB seq={} tunnel error: {e} [ep={:#04x} {} iso={}]",
            cmd.seqnum, cmd.ep, if cmd.is_in() { "IN" } else { "OUT" }, cmd.is_iso()
        );
        let ret = UsbipRetSubmit {
            seqnum: cmd.seqnum,
            devid: cmd.devid,
            direction: cmd.direction,
            ep: cmd.ep,
            status: EPIPE,
            ..Default::default()
        };
        let _ = self.send(&ret.to_bytes()).await;
        self.alive.store(false, Ordering::SeqCst);
        self.shutdown.notify_one();
    }
}

fn contains(set: &[Arc<SilexSession>], s: &Arc<SilexSession>) -> bool {
    set.iter().any(|x| Arc::ptr_eq(x, s))
}

async fn urb_pump(conn: TcpStream, device: Arc<SilexDevice>) -> Result<()> {
    let (rd, wr) = conn.into_split();
    let pump = Arc::new(Pump {
        wr: Mutex::new(wr),
        cancelled: StdMutex::new(HashSet::new()),
        alive: AtomicBool::new(true),
        shutdown: Notify::new(),
    });

    let sessions = device.sessions();
    let iso = device.iso_sessions();
    let mux: Vec<Arc<SilexSession>> = device
        .interrupt_sessions()
        .into_iter()
        .filter(|s| !contains(&iso, s))
        .collect();

    // Iso and mux connections are served by their own task(s) with their own keepalive ->
    // stop the auto-keepalive so there is only one reader per socket.
    for s in iso.iter().chain(mux.iter()) {
        s.stop_keepalive();
    }

    let mut senders: Vec<(Arc<SilexSession>, mpsc::Sender<UsbipCmdSubmit>)> = Vec::new();
    let mut tasks = Vec::new();
    for s in &sessions {
        let (tx, rx) = mpsc::channel::<UsbipCmdSubmit>(64);
        senders.push((s.clone(), tx));
        if contains(&iso, s) {
            tasks.push(tokio::spawn(iso_worker(pump.clone(), s.clone(), rx)));
        } else if contains(&mux, s) {
            let inflight = Arc::new(StdMutex::new(HashMap::new()));
            tasks.push(tokio::spawn(mux_sender(pump.clone(), s.clone(), rx, inflight.clone(), device.clone())));
            tasks.push(tokio::spawn(mux_reader(pump.clone(), s.clone(), inflight.clone())));
        } else {
            tasks.push(tokio::spawn(plain_worker(pump.clone(), s.clone(), rx)));
        }
    }

    reader_loop(rd, pump.clone(), device.clone(), senders).await;
    // `senders` was moved into reader_loop and dropped there -> queue workers see their
    // channel close and exit. Abort any task still blocked on device I/O (mux long-poll)
    // and await it, so it releases the session read/write locks before disconnect_all.
    pump.alive.store(false, Ordering::SeqCst);
    for t in tasks {
        t.abort();
        let _ = t.await;
    }
    Ok(())
}

async fn reader_loop(
    mut rd: OwnedReadHalf,
    pump: Arc<Pump>,
    device: Arc<SilexDevice>,
    senders: Vec<(Arc<SilexSession>, mpsc::Sender<UsbipCmdSubmit>)>,
) {
    loop {
        let mut head = [0u8; UsbipCmdSubmit::HEADER_SIZE];
        tokio::select! {
            res = rd.read_exact(&mut head) => {
                if res.is_err() {
                    break;
                }
            }
            _ = pump.shutdown.notified() => break,
        }
        let command = match up::peek_command(&head) {
            Ok(c) => c,
            Err(_) => break,
        };
        if command == up::USBIP_CMD_SUBMIT {
            let mut cmd = match UsbipCmdSubmit::parse(&head, Vec::new()) {
                Ok(c) => c,
                Err(_) => break,
            };
            if !cmd.is_in() && cmd.transfer_buffer_length > 0 {
                let mut body = vec![0u8; cmd.transfer_buffer_length as usize];
                if rd.read_exact(&mut body).await.is_err() {
                    break;
                }
                cmd.transfer_buffer = body;
            }
            if cmd.is_iso() {
                // On ISO, number_of_packets descriptors (16 B each) follow the
                // transfer_buffer - otherwise the URB stream desyncs.
                let mut descs = vec![0u8; up::ISO_DESC_SIZE * cmd.number_of_packets as usize];
                if rd.read_exact(&mut descs).await.is_err() {
                    break;
                }
                if cmd.set_iso_from(&descs).is_err() {
                    break;
                }
            }
            let target = device.session_for(&cmd);
            if let Some((_s, tx)) = senders.iter().find(|(s, _)| Arc::ptr_eq(s, &target)) {
                if tx.send(cmd).await.is_err() {
                    break;
                }
            }
        } else if command == up::USBIP_CMD_UNLINK {
            let unlink = match UsbipCmdUnlink::parse(&head) {
                Ok(u) => u,
                Err(_) => break,
            };
            pump.cancelled.lock().unwrap().insert(unlink.unlink_seqnum);
            let bytes = UsbipRetUnlink { seqnum: unlink.seqnum, status: 0 }.to_bytes();
            if pump.send(&bytes).await.is_err() {
                break;
            }
        } else {
            break;
        }
    }
}

/// Synchronous worker for a plain bulk/control connection.
async fn plain_worker(pump: Arc<Pump>, sess: Arc<SilexSession>, mut rx: mpsc::Receiver<UsbipCmdSubmit>) {
    while let Some(cmd) = rx.recv().await {
        if !pump.take(cmd.seqnum) {
            continue;
        }
        match submit_to_silex(&sess, &cmd).await {
            Ok(ret) => {
                if !pump.complete(&ret).await {
                    return;
                }
            }
            Err(e) => {
                pump.abort_tunnel(&cmd, e).await;
                return;
            }
        }
    }
}

/// Iso connection like SX Virtual Link: send many requests continuously ahead and read the
/// responses trailing (a deep pipeline buffer against jitter). A single task owns the
/// socket; while idle it keepalives itself. Responses arrive FIFO -> in-flight is a deque.
async fn iso_worker(pump: Arc<Pump>, sess: Arc<SilexSession>, mut rx: mpsc::Receiver<UsbipCmdSubmit>) {
    let mut inflight: VecDeque<UsbipCmdSubmit> = VecDeque::new();
    let ka = if sess.keepalive_interval.is_zero() { KEEPALIVE } else { sess.keepalive_interval };
    // Load iso-OUT formats ONCE (a control transfer) while nothing is in flight -> controls
    // which OUT endpoints (16-bit audio only) may be compressed.
    if !sess.iso_out_format_loaded() {
        sess.load_iso_out_format().await;
    }
    while pump.is_alive() {
        // 1) send ahead: all immediately available URBs (up to the pipeline ceiling).
        while inflight.len() < ISO_MAX_INFLIGHT {
            match rx.try_recv() {
                Ok(cmd) => {
                    if !iso_enqueue(&pump, &sess, cmd, &mut inflight).await {
                        return;
                    }
                }
                Err(_) => break, // empty or disconnected
            }
        }
        // 2) fetch one response and complete the oldest URB.
        if !inflight.is_empty() {
            match sess.iso_recv().await {
                Ok((status1, packets, data)) => {
                    let cmd = inflight.pop_front().unwrap();
                    let ret = if cmd.is_in() {
                        iso_fill_ret(&cmd, ret_for(&cmd), status1, &packets, data)
                    } else {
                        iso_fill_ret_out(&cmd, ret_for(&cmd), status1)
                    };
                    if !pump.complete(&ret).await {
                        return;
                    }
                }
                Err(e) => {
                    let cmd = inflight[0].clone();
                    pump.abort_tunnel(&cmd, e).await;
                    return;
                }
            }
        } else {
            // Pipeline empty: block for the next URB; on idle (timeout) keepalive ourselves.
            match timeout(ka, rx.recv()).await {
                Ok(Some(cmd)) => {
                    if !iso_enqueue(&pump, &sess, cmd, &mut inflight).await {
                        return;
                    }
                }
                Ok(None) => return, // channel closed
                Err(_) => {
                    if sess.get_device_descriptor().await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Send a URB and add it to `inflight`. false => end the worker/connection.
async fn iso_enqueue(pump: &Pump, sess: &SilexSession, cmd: UsbipCmdSubmit, inflight: &mut VecDeque<UsbipCmdSubmit>) -> bool {
    if !pump.take(cmd.seqnum) {
        return true; // cancelled -> skip
    }
    let send = if cmd.is_in() {
        let (ep, packets, sf, mp, tl) = iso_request_params(sess, &cmd);
        sess.iso_send(ep, packets, sf, mp, tl).await
    } else {
        let (ep, packets, audio, sf, mp, nch) = iso_out_request_params(sess, &cmd);
        sess.iso_send_out(ep, packets, audio, sf, mp, nch).await
    };
    if let Err(e) = send {
        pump.abort_tunnel(&cmd, e).await;
        return false;
    }
    inflight.push_back(cmd);
    true
}

fn ep_info(device: &SilexDevice, cmd: &UsbipCmdSubmit) -> (Option<u8>, u16, u16) {
    let addr = ((cmd.ep as u8) & 0x7F) | if cmd.is_in() { 0x80 } else { 0x00 };
    let (mps, interval) = device.interrupt_params(addr);
    (device.ep_type_for(addr), mps, interval)
}

type MuxInflight = Arc<StdMutex<HashMap<u16, Inflight>>>;

/// Mux sender: take URBs (control/interrupt/bulk) off the queue and send them into the
/// tunnel without waiting for the response (the reader dispatches by seq). Interrupt-IN is
/// held open (long-poll) -> must not block control. While idle this sender keepalives.
async fn mux_sender(pump: Arc<Pump>, sess: Arc<SilexSession>, mut rx: mpsc::Receiver<UsbipCmdSubmit>, inflight: MuxInflight, device: Arc<SilexDevice>) {
    let ka = if sess.keepalive_interval.is_zero() { KEEPALIVE } else { sess.keepalive_interval };
    while pump.is_alive() {
        match timeout(ka, rx.recv()).await {
            Err(_) => {
                if !mux_keepalive(&pump, &sess, &inflight).await {
                    return;
                }
            }
            Ok(None) => return, // channel closed
            Ok(Some(cmd)) => {
                if !pump.take(cmd.seqnum) {
                    continue;
                }
                let (ep_type, mps, interval) = ep_info(&device, &cmd);
                let frame = build_request(&sess, &cmd, ep_type, mps, interval);
                let seq = frame.seq;
                inflight.lock().unwrap().insert(seq, Inflight::Cmd(cmd.clone()));
                if let Err(e) = sess.send_request(frame).await {
                    pump.abort_tunnel(&cmd, e).await;
                    return;
                }
            }
        }
    }
}

/// Harmless GET_DESCRIPTOR keepalive; its response is discarded by seq. false => dead.
async fn mux_keepalive(pump: &Pump, sess: &SilexSession, inflight: &StdMutex<HashMap<u16, Inflight>>) -> bool {
    let frame = sess.build_control_frame(0x80, 0x06, 0x0100, 0x0000, 0x12, Vec::new());
    let seq = frame.seq;
    inflight.lock().unwrap().insert(seq, Inflight::Keepalive);
    if sess.send_request(frame).await.is_err() {
        pump.alive.store(false, Ordering::SeqCst);
        return false;
    }
    true
}

/// Mux reader: read responses and dispatch each to the waiting URB by seq (responses are
/// NOT FIFO: a control answer can arrive before a still-open interrupt answer).
async fn mux_reader(pump: Arc<Pump>, sess: Arc<SilexSession>, inflight: MuxInflight) {
    while pump.is_alive() {
        let resp = match sess.recv_response().await {
            Ok(r) => r,
            Err(_) => {
                pump.alive.store(false, Ordering::SeqCst);
                return;
            }
        };
        let entry = inflight.lock().unwrap().remove(&resp.seq);
        match entry {
            Some(Inflight::Cmd(cmd)) => {
                if !pump.complete(&fill_ret_from_response(&cmd, &resp)).await {
                    return;
                }
            }
            _ => continue, // keepalive / unknown -> discard
        }
    }
}
