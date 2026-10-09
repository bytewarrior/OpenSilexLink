//! Router for composite devices: one Silex connection per sub-interface.
//!
//! The device server does NOT tunnel a composite device over a single connection. It
//! lists each interface as its own sub-device (dev01=interface0, dev02=interface1, ...)
//! and requires a separate connection per interface (its own `EXEC SCANNER CONNECT`
//! reservation + its own TCP socket to :19540). The iso stream of an interface MUST run on
//! that interface's connection - an iso request on the control connection is answered by
//! the device with a connection reset. Control/EP0 may run entirely on the first (control)
//! connection. ALL frames carry the primary's sel index.
//!
//! (Live reversed on a UAC2 device: IF0=AudioControl, IF1=AudioStreaming/iso EP0x83,
//! IF2=HID/interrupt EP0x84 -> three connections dev01/02/03.)

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use super::proto::UsbipCmdSubmit;
use super::session::SilexSession;

// USB endpoint types (bmAttributes & 0x03). Control and bulk need no special routing;
// they are listed to complete the table.
#[allow(dead_code)]
pub const EP_CONTROL: u8 = 0;
pub const EP_ISO: u8 = 1;
#[allow(dead_code)]
pub const EP_BULK: u8 = 2;
pub const EP_INTERRUPT: u8 = 3;

/// `{bEndpointAddress: (bInterfaceNumber, ep_type)}` from a configuration descriptor.
pub fn parse_endpoint_map(config_desc: &[u8]) -> HashMap<u8, (u8, u8)> {
    let mut result = HashMap::new();
    let mut cur_iface = 0u8;
    let n = config_desc.len();
    let mut i = 0;
    while i + 1 < n {
        let blen = config_desc[i] as usize;
        let btype = config_desc[i + 1];
        if blen == 0 {
            break;
        }
        if btype == 0x04 && i + 2 < n {
            cur_iface = config_desc[i + 2]; // INTERFACE
        } else if btype == 0x05 && i + 3 < n {
            result.insert(config_desc[i + 2], (cur_iface, config_desc[i + 3] & 0x03)); // ENDPOINT
        }
        i += blen;
    }
    result
}

/// `{bEndpointAddress: (wMaxPacketSize, bInterval)}` from a configuration descriptor. For
/// interrupt requests the device wants 0x8000|wMaxPacketSize and bInterval straight from
/// the endpoint descriptor (capture-verified).
pub fn parse_endpoint_params(config_desc: &[u8]) -> HashMap<u8, (u16, u16)> {
    let mut result = HashMap::new();
    let n = config_desc.len();
    let mut i = 0;
    while i + 1 < n {
        let blen = config_desc[i] as usize;
        let btype = config_desc[i + 1];
        if blen == 0 {
            break;
        }
        if btype == 0x05 && i + 6 < n {
            let mps = config_desc[i + 4] as u16 | ((config_desc[i + 5] as u16) << 8);
            result.insert(config_desc[i + 2], (mps, config_desc[i + 6] as u16));
        }
        i += blen;
    }
    result
}

/// dev_id of sub-interface `iface` (interface 0 = primary): dev01 -> dev0{1+iface}.
pub fn sub_dev_id(primary_dev_id: &str, iface: u8) -> String {
    let bytes = primary_dev_id.as_bytes();
    let mut start = bytes.len();
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == bytes.len() {
        return primary_dev_id.to_string(); // no trailing digits
    }
    let digits = &primary_dev_id[start..];
    let width = digits.len();
    let num: u64 = digits.parse::<u64>().unwrap_or(0) + iface as u64;
    format!("{}{:0width$}", &primary_dev_id[..start], num, width = width)
}

/// Bundles the Silex connections of a device and routes URBs by endpoint.
///
/// A simple (non-composite) device has exactly one connection - then the router behaves
/// like a single [`SilexSession`].
pub struct SilexDevice {
    pub primary: Arc<SilexSession>,
    endpoints: HashMap<u8, (u8, u8)>, // addr -> (iface, ep_type)
    iface_sessions: HashMap<u8, Arc<SilexSession>>,
    ep_params: HashMap<u8, (u16, u16)>, // addr -> (wMaxPacketSize, bInterval)
    all: Vec<Arc<SilexSession>>,        // unique order, primary first
}

impl SilexDevice {
    /// Build the router: connect a separate session for each interface that has endpoints
    /// (interface 0 = the already-connected `primary`). `make_sub(dev_id)` creates a not
    /// yet connected session with the primary's sel index.
    pub async fn build<F>(
        primary: Arc<SilexSession>,
        full_config_desc: &[u8],
        primary_dev_id: &str,
        make_sub: F,
    ) -> Result<SilexDevice>
    where
        F: Fn(&str) -> Arc<SilexSession>,
    {
        let endpoints = parse_endpoint_map(full_config_desc);
        let ep_params = parse_endpoint_params(full_config_desc);
        let mut ifaces: Vec<u8> = endpoints.values().map(|(ifc, _t)| *ifc).collect();
        ifaces.sort_unstable();
        ifaces.dedup();

        let mut iface_sessions: HashMap<u8, Arc<SilexSession>> = HashMap::new();
        iface_sessions.insert(0, primary.clone());
        for ifc in ifaces {
            if iface_sessions.contains_key(&ifc) {
                continue;
            }
            let sub = make_sub(&sub_dev_id(primary_dev_id, ifc));
            if let Err(e) = sub.connect().await {
                // sub-connect failed -> release everything new
                for (i, s) in &iface_sessions {
                    if *i != 0 {
                        s.disconnect().await;
                    }
                }
                return Err(e);
            }
            iface_sessions.insert(ifc, sub);
        }

        // unique order, primary first
        let mut all: Vec<Arc<SilexSession>> = vec![primary.clone()];
        let mut keys: Vec<u8> = iface_sessions.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            let s = &iface_sessions[&k];
            if !all.iter().any(|x| Arc::ptr_eq(x, s)) {
                all.push(s.clone());
            }
        }
        Ok(SilexDevice { primary, endpoints, iface_sessions, ep_params, all })
    }

    /// Single-session device (no config descriptor scan). The whole URB stream runs on the
    /// one connection.
    #[cfg(test)]
    pub fn single(primary: Arc<SilexSession>) -> SilexDevice {
        let mut iface_sessions = HashMap::new();
        iface_sessions.insert(0, primary.clone());
        SilexDevice {
            all: vec![primary.clone()],
            primary,
            endpoints: HashMap::new(),
            iface_sessions,
            ep_params: HashMap::new(),
        }
    }

    /// Target connection for a URB. EP0/control -> control connection (primary). Non-zero
    /// endpoint (bulk/iso/interrupt) -> the connection of its interface.
    pub fn session_for(&self, cmd: &UsbipCmdSubmit) -> Arc<SilexSession> {
        if cmd.ep == 0 {
            return self.primary.clone();
        }
        let addr = ((cmd.ep as u8) & 0x7F) | if cmd.is_in() { 0x80 } else { 0x00 };
        match self.endpoints.get(&addr) {
            Some((iface, _t)) => self.iface_sessions.get(iface).cloned().unwrap_or_else(|| self.primary.clone()),
            None => self.primary.clone(),
        }
    }

    /// Endpoint type (EP_CONTROL/ISO/BULK/INTERRUPT) of an EP address, or None.
    pub fn ep_type_for(&self, addr: u8) -> Option<u8> {
        self.endpoints.get(&addr).map(|(_i, t)| *t)
    }

    /// (wMaxPacketSize, bInterval) of an interrupt EP; default (8, 10) as fallback.
    pub fn interrupt_params(&self, addr: u8) -> (u16, u16) {
        self.ep_params.get(&addr).copied().unwrap_or((8, 10))
    }

    /// Connections serving at least one interrupt endpoint (run over the mux worker: the
    /// device holds interrupt-IN as a long-poll, so control/keepalive on the same
    /// connection must not block).
    pub fn interrupt_sessions(&self) -> Vec<Arc<SilexSession>> {
        let mut out: Vec<Arc<SilexSession>> = Vec::new();
        for (iface, ep_type) in self.endpoints.values() {
            if *ep_type == EP_INTERRUPT {
                if let Some(s) = self.iface_sessions.get(iface) {
                    if !out.iter().any(|x| Arc::ptr_eq(x, s)) {
                        out.push(s.clone());
                    }
                }
            }
        }
        out
    }

    /// Connections serving (only) iso endpoints -> pipelined worker. Interfaces with extra
    /// non-iso endpoints are conservatively excluded and run synchronously.
    pub fn iso_sessions(&self) -> Vec<Arc<SilexSession>> {
        let mut iso: Vec<Arc<SilexSession>> = Vec::new();
        let mut other: Vec<Arc<SilexSession>> = Vec::new();
        for (iface, ep_type) in self.endpoints.values() {
            if let Some(s) = self.iface_sessions.get(iface) {
                let bucket = if *ep_type == EP_ISO { &mut iso } else { &mut other };
                if !bucket.iter().any(|x| Arc::ptr_eq(x, s)) {
                    bucket.push(s.clone());
                }
            }
        }
        iso.into_iter().filter(|s| !other.iter().any(|o| Arc::ptr_eq(o, s))).collect()
    }

    pub fn sessions(&self) -> Vec<Arc<SilexSession>> {
        self.all.clone()
    }

    #[cfg(test)]
    pub fn is_composite(&self) -> bool {
        self.all.len() > 1
    }

    pub async fn disconnect_all(&self) {
        for s in self.all.iter().rev() {
            s.disconnect().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usbip::proto::{USBIP_DIR_IN, USBIP_DIR_OUT};
    use std::time::Duration;

    fn session(dev_id: &str) -> Arc<SilexSession> {
        Arc::new(SilexSession::new("192.0.2.1", [0; 6], dev_id, Some("192.0.2.9".into()), 19540, Duration::ZERO))
    }

    fn cmd(ep: u32, dir_in: bool) -> UsbipCmdSubmit {
        let mut head = vec![0u8; UsbipCmdSubmit::HEADER_SIZE];
        head[0..4].copy_from_slice(&crate::usbip::proto::USBIP_CMD_SUBMIT.to_be_bytes());
        head[12..16].copy_from_slice(&(if dir_in { USBIP_DIR_IN } else { USBIP_DIR_OUT }).to_be_bytes());
        head[16..20].copy_from_slice(&ep.to_be_bytes());
        UsbipCmdSubmit::parse(&head, Vec::new()).unwrap()
    }

    fn composite() -> SilexDevice {
        let primary = session("dev01");
        let s1 = session("dev02");
        let s2 = session("dev03");
        let mut endpoints = HashMap::new();
        endpoints.insert(0x83u8, (1u8, EP_ISO)); // iso IN on interface 1
        endpoints.insert(0x84u8, (2u8, EP_INTERRUPT)); // interrupt IN on interface 2
        let mut ep_params = HashMap::new();
        ep_params.insert(0x84u8, (16u16, 4u16));
        let mut iface_sessions = HashMap::new();
        iface_sessions.insert(0, primary.clone());
        iface_sessions.insert(1, s1.clone());
        iface_sessions.insert(2, s2.clone());
        SilexDevice {
            all: vec![primary.clone(), s1, s2],
            primary,
            endpoints,
            iface_sessions,
            ep_params,
        }
    }

    #[test]
    fn sub_dev_id_increments_trailing_number() {
        assert_eq!(sub_dev_id("dev01", 0), "dev01");
        assert_eq!(sub_dev_id("dev01", 1), "dev02");
        assert_eq!(sub_dev_id("dev09", 2), "dev11");
        assert_eq!(sub_dev_id("nodigits", 1), "nodigits");
    }

    #[test]
    fn parse_endpoint_map_reads_interface_and_type() {
        let cfg = [
            9, 0x04, 1, 0, 1, 0x01, 0x02, 0, 0, // INTERFACE 1, audio streaming
            9, 0x05, 0x83, 0x01, 0xC8, 0x00, 0x01, 0, 0, // ENDPOINT 0x83 iso IN
        ];
        let m = parse_endpoint_map(&cfg);
        assert_eq!(m.get(&0x83), Some(&(1u8, EP_ISO)));
        let p = parse_endpoint_params(&cfg);
        assert_eq!(p.get(&0x83), Some(&(200u16, 1u16)));
    }

    #[test]
    fn session_for_routes_by_endpoint() {
        let dev = composite();
        assert!(Arc::ptr_eq(&dev.session_for(&cmd(0, true)), &dev.primary)); // control
        // iso IN on ep 3 (addr 0x83) -> interface 1 session (dev02)
        assert_eq!(dev.session_for(&cmd(3, true)).dev_id, "dev02");
        // interrupt IN on ep 4 (addr 0x84) -> interface 2 session (dev03)
        assert_eq!(dev.session_for(&cmd(4, true)).dev_id, "dev03");
        // unknown endpoint -> primary
        assert!(Arc::ptr_eq(&dev.session_for(&cmd(7, false)), &dev.primary));
    }

    #[test]
    fn iso_and_interrupt_session_classification() {
        let dev = composite();
        assert!(dev.is_composite());
        let iso = dev.iso_sessions();
        assert_eq!(iso.len(), 1);
        assert_eq!(iso[0].dev_id, "dev02");
        let irq = dev.interrupt_sessions();
        assert_eq!(irq.len(), 1);
        assert_eq!(irq[0].dev_id, "dev03");
        assert_eq!(dev.interrupt_params(0x84), (16, 4));
        assert_eq!(dev.interrupt_params(0x99), (8, 10)); // default
    }

    #[test]
    fn single_device_is_not_composite() {
        let dev = SilexDevice::single(session("dev01"));
        assert!(!dev.is_composite());
        assert!(dev.iso_sessions().is_empty());
        assert!(dev.interrupt_sessions().is_empty());
        assert!(Arc::ptr_eq(&dev.session_for(&cmd(3, true)), &dev.primary));
    }
}
