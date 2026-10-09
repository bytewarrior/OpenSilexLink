//! Silex UDP discovery/control (UDP/19540).
//!
//! Reconstructed from packet captures. All multi-byte fields big-endian. Every message
//! starts with `opcode (u16)` and `subcode (u16)`.
//!
//! Device -> host: 0x0002/0x9506 announcement, 0x0001/0x0000 device-list reply,
//! 0x0003/0x0000 ACK, 0x2988/0x01xx descriptor broadcast.
//! Host -> device: 0x0001/0x0001 command QUERY, 0x0003/0x0001 command COMMIT,
//! 0x0004/0x9506 poll/keepalive.
//!
//! Commands are ASCII ("GET USB_DEVICE_LIST", "EXEC SCANNER CONNECT:",
//! "EXEC SCANNER DISCONNECT:") and run in two phases: QUERY then COMMIT.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{bail, Result};

use crate::wire::{cstr_field, rd_u16, rd_u32, read_cstr};

pub const PORT: u16 = 19540;

// Operation/resource codes (appear in the commit [84:86] and in responses [4:8]).
pub const OP_USB_DEVICE_LIST: u16 = 0x0083;
pub const OP_SCANNER: u16 = 0x0003;

pub const TRAILER: u16 = 0x0200; // [86:88] in all commands (bytes 02 00)
pub const CONNECT_TOKEN: [u8; 5] = [0x4c, 0x54, 0x02, 0xc0, 0x53]; // observed; purpose unknown
pub const EXEC_FLAGS: [u8; 2] = [0x01, 0x06];

pub const CMD_GET_LIST: &[u8] = b"GET USB_DEVICE_LIST";
pub const CMD_CONNECT: &[u8] = b"EXEC SCANNER CONNECT:";
pub const CMD_DISCONNECT: &[u8] = b"EXEC SCANNER DISCONNECT:";

// Observed total sizes of the command buffers.
pub const SIZE_GET: usize = 88;
pub const SIZE_EXEC_QUERY: usize = 510;
pub const SIZE_EXEC_COMMIT: usize = 88;

// USB base class -> type text (like SX Virtual Link). English source strings; the
// client's German catalog translates them.
pub const USB_CLASS_AUDIO: u8 = 0x01;
pub const USB_CLASS_HID: u8 = 0x03;
pub const USB_CLASS_IMAGE: u8 = 0x06;
pub const USB_CLASS_PRINTER: u8 = 0x07;
pub const USB_CLASS_STORAGE: u8 = 0x08;
pub const USB_CLASS_VIDEO: u8 = 0x0e;
pub const USB_CLASS_MISC: u8 = 0xef; // bDeviceClass 0xEF = Misc/IAD = composite device
pub const USB_CLASS_VENDOR: u8 = 0xff;

pub const USB_CLASS_TYPE_DEFAULT: &str = "Other USB device";

/// Device-type text from a USB class byte (for the "Type" column). Matches the type
/// table SX Virtual Link derives from the interface class.
pub fn class_to_type(class: u8) -> &'static str {
    match class {
        USB_CLASS_AUDIO => "Audio",
        USB_CLASS_HID => "Mouse, Keyboard",
        USB_CLASS_IMAGE => "Imaging device",
        USB_CLASS_PRINTER => "Printer",
        USB_CLASS_STORAGE => "Mass storage device",
        USB_CLASS_VIDEO => "Video",
        _ => USB_CLASS_TYPE_DEFAULT,
    }
}

// Fixed layout of one list entry, measured FROM the class marker (0x??83). Reversed
// from the live device list: status(1) 0x83(1) vid(2 BE) pid(2 BE) tag(16, NUL-pad)
// bDeviceClass(1) bInterfaceClass(1) 4x00. The marker sits exactly 4 bytes (owner IP)
// after the description NUL; the status high byte varies (0x80/0x84/0x87), only the
// low byte 0x83 is constant (so do NOT search for b"\x80\x83").
const ENTRY_TAG_OFFSET: usize = 6; // marker+6: tag field
const ENTRY_TAG_WIDTH: usize = 16;
const ENTRY_DEVCLASS_OFFSET: usize = 22; // marker+22: bDeviceClass
const ENTRY_IFCLASS_OFFSET: usize = 23; // marker+23: bInterfaceClass (type-relevant)
const ENTRY_STRIDE_FROM_MARKER: usize = 28; // marker -> next entry

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Query = 0x0001,
    Commit = 0x0003,
}

impl Phase {
    fn from_u16(v: u16) -> Option<Self> {
        match v {
            0x0001 => Some(Phase::Query),
            0x0003 => Some(Phase::Commit),
            _ => None,
        }
    }
}

/// IPv6 link-local address (fe80::/64) from the MAC via EUI-64.
///
/// The device server does not report IPv6 in the protocol, but the link-local address
/// is derivable from the MAC (invert the U/L bit, insert ff:fe).
pub fn mac_to_link_local(mac: &[u8]) -> String {
    if mac.len() != 6 {
        return String::new();
    }
    let mut eui = [0u8; 8];
    eui[0] = mac[0] ^ 0x02;
    eui[1] = mac[1];
    eui[2] = mac[2];
    eui[3] = 0xff;
    eui[4] = 0xfe;
    eui[5] = mac[3];
    eui[6] = mac[4];
    eui[7] = mac[5];
    let mut addr = [0u8; 16];
    addr[0] = 0xfe;
    addr[1] = 0x80;
    addr[8..].copy_from_slice(&eui);
    Ipv6Addr::from(addr).to_string()
}

fn ipv4(buf: &[u8], off: usize) -> String {
    if off + 4 > buf.len() {
        return String::new();
    }
    Ipv4Addr::new(buf[off], buf[off + 1], buf[off + 2], buf[off + 3]).to_string()
}

fn find_sub(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

// --------------------------------------------------------------------------- //
//  Message data structures
// --------------------------------------------------------------------------- //
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub model: String,
    pub mac: Vec<u8>,
    pub ip: String,
    pub version: String,
    pub attached_device: String,
    pub hostname: String,
}

impl Announcement {
    const MODEL_OFF: usize = 36;
    const MAC_OFF: usize = 56;
    const IP_OFF: usize = 66;
    const VERSION_OFF: usize = 72;
    const ATTACHED_OFF: usize = 84;
    const HOSTNAME_OFF: usize = 116;

    fn parse(buf: &[u8]) -> Announcement {
        let mac = if buf.len() >= Self::MAC_OFF + 6 {
            buf[Self::MAC_OFF..Self::MAC_OFF + 6].to_vec()
        } else {
            Vec::new()
        };
        Announcement {
            model: cstr_field(buf, Self::MODEL_OFF, None),
            mac,
            ip: ipv4(buf, Self::IP_OFF),
            version: cstr_field(buf, Self::VERSION_OFF, None),
            attached_device: cstr_field(buf, Self::ATTACHED_OFF, None),
            hostname: cstr_field(buf, Self::HOSTNAME_OFF, None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceEntry {
    pub dev_id: String, // internal Silex id, e.g. "dev01"
    pub description: String,
    pub id_vendor: u16,
    pub id_product: u16,
    pub tag: String,      // <speed letter><index>[M<if>], e.g. "H112" or "H111M0"
    pub owner_ip: String, // holder IP ("" = free)
    pub usb_class: u8,    // bInterfaceClass (marker+23) - the type-relevant byte
    pub dev_class: u8,    // bDeviceClass (marker+22); 0xEF = composite device
}

impl UsbDeviceEntry {
    pub fn new(dev_id: &str, description: &str, id_vendor: u16, id_product: u16, tag: &str) -> Self {
        Self {
            dev_id: dev_id.into(),
            description: description.into(),
            id_vendor,
            id_product,
            tag: tag.into(),
            owner_ip: String::new(),
            usb_class: 0,
            dev_class: 0,
        }
    }

    /// sel high byte for the tunnel (dev01->0x03, dev02->0x04, devNN->NN+2).
    pub fn dev_index(&self) -> u8 {
        let digits: String = self.dev_id.chars().rev().take_while(|c| c.is_ascii_digit())
            .collect::<String>().chars().rev().collect();
        match digits.parse::<u32>() {
            Ok(n) => (n + 2) as u8,
            Err(_) => 3,
        }
    }

    /// USB speed from the first tag character (H/F/L), otherwise "".
    pub fn speed(&self) -> &'static str {
        match self.tag.chars().next().map(|c| c.to_ascii_uppercase()) {
            Some('L') => "1.5 Mbps (Low-Speed)",
            Some('F') => "12 Mbps (Full-Speed)",
            Some('H') => "480 Mbps (High-Speed)",
            _ => "",
        }
    }

    /// Interface index from the tag suffix "M<n>" (composite device), else None.
    ///
    /// The device server explodes a composite device into one list entry per USB
    /// interface; the entries share VID:PID and the tag stem, the suffix M0/M1/...
    /// counts the interface (dev02="H111M0", dev03="H111M1", ...).
    pub fn interface_index(&self) -> Option<u32> {
        let m = self.tag.rfind('M')?;
        let suffix = &self.tag[m + 1..];
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            suffix.parse().ok()
        } else {
            None
        }
    }

    /// Tag without the "M<n>" interface suffix (identical for all sub-entries of a
    /// composite device) - part of the merge key.
    pub fn tag_base(&self) -> String {
        match self.interface_index() {
            Some(_) => self.tag[..self.tag.rfind('M').unwrap()].to_string(),
            None => self.tag.clone(),
        }
    }

    /// Sub-interface of a composite device (bDeviceClass 0xEF or tag suffix present).
    pub fn is_composite(&self) -> bool {
        self.dev_class == USB_CLASS_MISC || self.interface_index().is_some()
    }

    /// Device type from the USB interface class (for the "Type" column).
    pub fn device_type(&self) -> &'static str {
        class_to_type(self.usb_class)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceListResponse {
    pub op_code: u32,
    pub count: u8,
    pub devices: Vec<UsbDeviceEntry>,
}

impl DeviceListResponse {
    fn parse(buf: &[u8]) -> DeviceListResponse {
        // Per entry: id(NUL), description(NUL), 4-byte owner IP, then the fixed
        // 28-byte block from the class marker (see ENTRY_*). The 4 bytes directly
        // BEFORE the marker are the holder IP (0.0.0.0 = free). IMPORTANT: the marker
        // high byte varies (0x80/0x84/...), only the low byte 0x83 is constant.
        let op_code = if buf.len() >= 8 { rd_u32(buf, 4) } else { 0 };
        let count = if buf.len() > 8 { buf[8] } else { 0 };
        let mut devices = Vec::new();
        let mut pos = 9usize;
        for _ in 0..count {
            // advance to the next entry: first ASCII letter (start of "devNN")
            while pos < buf.len() && !(0x41..=0x7A).contains(&buf[pos]) {
                pos += 1;
            }
            if pos >= buf.len() {
                break;
            }
            let (dev_id, p) = read_cstr(buf, pos);
            let (description, p) = read_cstr(buf, p);
            pos = p;
            let mut mk = pos + 4; // 4-byte owner IP, then the marker
            // Validate the marker; on mismatch resync forward on the 0x83 low byte
            // (defensive, in case padding ever slips in between).
            if !(mk + 1 < buf.len() && buf[mk + 1] == 0x83) {
                match find_sub(buf, &[0x83], pos) {
                    Some(nxt) if nxt >= 1 => mk = nxt - 1,
                    _ => break,
                }
            }
            if mk + ENTRY_STRIDE_FROM_MARKER > buf.len() {
                break;
            }
            let owner_ip = {
                let o = ipv4(buf, mk - 4);
                if o == "0.0.0.0" { String::new() } else { o }
            };
            let id_vendor = rd_u16(buf, mk + 2);
            let id_product = rd_u16(buf, mk + 4);
            let tag = cstr_field(buf, mk + ENTRY_TAG_OFFSET, Some(ENTRY_TAG_WIDTH));
            let dev_class = buf[mk + ENTRY_DEVCLASS_OFFSET];
            let usb_class = buf[mk + ENTRY_IFCLASS_OFFSET];
            devices.push(UsbDeviceEntry {
                dev_id,
                description,
                id_vendor,
                id_product,
                tag,
                owner_ip,
                usb_class,
                dev_class,
            });
            pos = mk + ENTRY_STRIDE_FROM_MARKER;
        }
        DeviceListResponse { op_code, count, devices }
    }
}

// --------------------------------------------------------------------------- //
//  Composite devices: merge + type reduction (modelled on SX Virtual Link)
// --------------------------------------------------------------------------- //
/// Priority order SX Virtual Link uses to pick ONE
/// representative class for type+icon from ALL interface classes of a (composite)
/// device - first match wins. HID (0x03) sits near the end on purpose, because many
/// devices (audio, printers, ...) carry only control buttons as HID; audio (0x01)
/// therefore beats HID. That is why SX VL shows the Sony PS-HX500 as "Audio".
const CLASS_PRIORITY: [u8; 5] = [
    USB_CLASS_PRINTER, // 0x07
    USB_CLASS_VIDEO,   // 0x0e
    USB_CLASS_AUDIO,   // 0x01
    USB_CLASS_STORAGE, // 0x08
    USB_CLASS_HID,     // 0x03
];

/// Representative USB class for type/icon (SX-VL priority chain).
///
/// `if_classes` = all interface classes of the physical device (exactly one for a
/// single-interface device). Name heuristics like the original: "scan" -> imaging,
/// "displaylink" -> video.
pub fn representative_class(if_classes: &[u8], description: &str) -> u8 {
    let desc = description.to_ascii_lowercase();
    if if_classes.contains(&USB_CLASS_PRINTER) {
        return USB_CLASS_PRINTER;
    }
    if desc.contains("scan") {
        return USB_CLASS_IMAGE;
    }
    if desc.contains("displaylink") {
        return USB_CLASS_VIDEO;
    }
    for cls in CLASS_PRIORITY {
        if if_classes.contains(&cls) {
            return cls;
        }
    }
    // no priority hit: first "real" class (0x00/0xEF = container only), else 0xFF
    for &c in if_classes {
        if c != 0x00 && c != USB_CLASS_MISC {
            return c;
        }
    }
    USB_CLASS_VENDOR
}

/// One physical USB device - collapses the sub-interface entries of a composite
/// device (same VID:PID + tag stem) into ONE display row, like SX Virtual Link does.
/// For non-composite devices it holds exactly one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalDevice {
    pub entries: Vec<UsbDeviceEntry>,
}

impl LogicalDevice {
    /// Leading entry (smallest interface index = "M0"); its dev_id is used to connect.
    pub fn primary(&self) -> &UsbDeviceEntry {
        self.entries
            .iter()
            .min_by_key(|e| e.interface_index().map(|i| i as i64).unwrap_or(-1))
            .expect("logical device always has at least one entry")
    }

    pub fn is_composite(&self) -> bool {
        self.entries.len() > 1 || self.entries[0].is_composite()
    }

    pub fn if_classes(&self) -> Vec<u8> {
        self.entries.iter().map(|e| e.usb_class).collect()
    }

    pub fn representative_class(&self) -> u8 {
        representative_class(&self.if_classes(), &self.primary().description)
    }

    pub fn device_type(&self) -> &'static str {
        class_to_type(self.representative_class())
    }

    pub fn owner_ip(&self) -> String {
        self.entries
            .iter()
            .find(|e| !e.owner_ip.is_empty())
            .map(|e| e.owner_ip.clone())
            .unwrap_or_default()
    }

    // convenient proxies onto the leading entry
    pub fn dev_id(&self) -> &str {
        &self.primary().dev_id
    }
    pub fn description(&self) -> &str {
        &self.primary().description
    }
    pub fn id_vendor(&self) -> u16 {
        self.primary().id_vendor
    }
    pub fn id_product(&self) -> u16 {
        self.primary().id_product
    }
    pub fn tag(&self) -> &str {
        &self.primary().tag
    }
    pub fn speed(&self) -> &'static str {
        self.primary().speed()
    }
}

/// Groups the sub-interfaces of a composite device (same VID:PID + tag stem) into one
/// [`LogicalDevice`] each; first-seen order is preserved.
pub fn logical_devices(entries: &[UsbDeviceEntry]) -> Vec<LogicalDevice> {
    let mut groups: Vec<Vec<UsbDeviceEntry>> = Vec::new();
    let mut index: std::collections::HashMap<(u16, u16, String), usize> =
        std::collections::HashMap::new();
    for e in entries {
        if e.is_composite() {
            let key = (e.id_vendor, e.id_product, e.tag_base());
            match index.get(&key) {
                Some(&gi) => groups[gi].push(e.clone()),
                None => {
                    index.insert(key, groups.len());
                    groups.push(vec![e.clone()]);
                }
            }
        } else {
            groups.push(vec![e.clone()]);
        }
    }
    groups.into_iter().map(|entries| LogicalDevice { entries }).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleResponse {
    pub opcode: u16,
    pub op_code: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorBroadcast {
    pub subcode: u16,
    pub ip: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub phase: Phase,
    pub command: Vec<u8>,
    pub op_code: u16,
    pub mac: Option<Vec<u8>>,
    pub host_ip: Option<String>,
    pub dev_id: Option<String>,
    pub token: Vec<u8>,
    pub size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Announcement(Announcement),
    Poll,
    Descriptor(DescriptorBroadcast),
    Command(CommandRequest),
    DeviceList(DeviceListResponse),
    Simple(SimpleResponse),
    Unknown { opcode: u16, subcode: u16 },
}

// --------------------------------------------------------------------------- //
//  Parser
// --------------------------------------------------------------------------- //
pub fn parse(buf: &[u8]) -> Result<Message> {
    if buf.len() < 4 {
        bail!("UDP message too short");
    }
    let opcode = rd_u16(buf, 0);
    let subcode = rd_u16(buf, 2);

    if opcode == 0x0002 {
        return Ok(Message::Announcement(Announcement::parse(buf)));
    }
    if opcode == 0x0004 {
        return Ok(Message::Poll);
    }
    if opcode == 0x2988 {
        return Ok(Message::Descriptor(DescriptorBroadcast {
            subcode,
            ip: ipv4(buf, 4),
        }));
    }
    if opcode == 0x0001 || opcode == 0x0003 {
        if subcode == 0x0001 {
            return parse_command(opcode, buf).map(Message::Command);
        }
        if subcode == 0x0000 {
            let op_code = if buf.len() >= 8 { rd_u32(buf, 4) } else { 0 };
            if buf.len() >= 15 && &buf[9..12] == b"dev" {
                return Ok(Message::DeviceList(DeviceListResponse::parse(buf)));
            }
            return Ok(Message::Simple(SimpleResponse {
                opcode,
                op_code,
                text: cstr_field(buf, 8, None),
            }));
        }
    }
    Ok(Message::Unknown { opcode, subcode })
}

fn parse_command(opcode: u16, buf: &[u8]) -> Result<CommandRequest> {
    let phase = Phase::from_u16(opcode).ok_or_else(|| anyhow::anyhow!("bad phase"))?;
    let body = &buf[4..];
    let command: Vec<u8> = if body.starts_with(b"EXEC") {
        match body.iter().position(|&b| b == b':') {
            Some(colon) => body[..colon + 1].to_vec(),
            None => body.split(|&b| b == 0).next().unwrap_or(&[]).to_vec(),
        }
    } else {
        body.split(|&b| b == 0).next().unwrap_or(&[]).to_vec()
    };
    let op_code = if buf.len() >= 86 { rd_u16(buf, 84) } else { 0 };
    let mut req = CommandRequest {
        phase,
        command: command.clone(),
        op_code,
        mac: None,
        host_ip: None,
        dev_id: None,
        token: Vec::new(),
        size: buf.len(),
    };
    if command == CMD_CONNECT || command == CMD_DISCONNECT {
        let mut off = 4 + command.len();
        if off + 2 > buf.len() || buf[off..off + 2] != EXEC_FLAGS {
            bail!("EXEC command missing flags");
        }
        off += 2;
        req.mac = Some(buf[off..off + 6].to_vec());
        off += 6;
        req.host_ip = Some(ipv4(buf, off));
        off += 4;
        let (dev_id, p) = read_cstr(buf, off);
        off = p;
        req.dev_id = Some(dev_id);
        if command == CMD_CONNECT {
            let end = (off + CONNECT_TOKEN.len()).min(buf.len());
            req.token = buf[off..end].to_vec();
        }
    }
    Ok(req)
}

// --------------------------------------------------------------------------- //
//  Builders (host -> device)
// --------------------------------------------------------------------------- //
fn build_command(opcode: u16, command: &[u8], size: usize, op_code: u16, params: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    buf[0..2].copy_from_slice(&opcode.to_be_bytes());
    buf[2..4].copy_from_slice(&0x0001u16.to_be_bytes());
    buf[4..4 + command.len()].copy_from_slice(command);
    let off = 4 + command.len();
    buf[off..off + params.len()].copy_from_slice(params);
    buf[84..86].copy_from_slice(&op_code.to_be_bytes());
    buf[86..88].copy_from_slice(&TRAILER.to_be_bytes());
    buf
}

pub fn build_get_device_list(phase: Phase, op_code: Option<u16>) -> Vec<u8> {
    let op_code = op_code.unwrap_or(match phase {
        Phase::Commit => OP_USB_DEVICE_LIST,
        Phase::Query => 0,
    });
    build_command(phase as u16, CMD_GET_LIST, SIZE_GET, op_code, &[])
}

fn exec_params(mac: &[u8], host_ip: &str, dev_id: &str, token: &[u8]) -> Result<Vec<u8>> {
    let ip: Ipv4Addr = host_ip.parse().map_err(|_| anyhow::anyhow!("bad host ip"))?;
    let mut p = Vec::new();
    p.extend_from_slice(&EXEC_FLAGS);
    p.extend_from_slice(mac);
    p.extend_from_slice(&ip.octets());
    p.extend_from_slice(dev_id.as_bytes());
    p.push(0);
    p.extend_from_slice(token);
    Ok(p)
}

pub fn build_connect(
    mac: &[u8],
    host_ip: &str,
    dev_id: &str,
    phase: Phase,
    token: &[u8],
    op_code: Option<u16>,
) -> Result<Vec<u8>> {
    let size = if phase == Phase::Commit { SIZE_EXEC_COMMIT } else { SIZE_EXEC_QUERY };
    let op_code = op_code.unwrap_or(match phase {
        Phase::Commit => OP_SCANNER,
        Phase::Query => 0,
    });
    let params = exec_params(mac, host_ip, dev_id, token)?;
    Ok(build_command(phase as u16, CMD_CONNECT, size, op_code, &params))
}

pub fn build_disconnect(
    mac: &[u8],
    host_ip: &str,
    dev_id: &str,
    phase: Phase,
    op_code: Option<u16>,
) -> Result<Vec<u8>> {
    let size = if phase == Phase::Commit { SIZE_EXEC_COMMIT } else { SIZE_EXEC_QUERY };
    let op_code = op_code.unwrap_or(match phase {
        Phase::Commit => OP_SCANNER,
        Phase::Query => 0,
    });
    let params = exec_params(mac, host_ip, dev_id, &[])?;
    Ok(build_command(phase as u16, CMD_DISCONNECT, size, op_code, &params))
}

pub fn build_poll() -> Vec<u8> {
    let mut buf = vec![0u8; SIZE_GET];
    buf[0..2].copy_from_slice(&0x0004u16.to_be_bytes());
    buf[2..4].copy_from_slice(&0x9506u16.to_be_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real device-list bytes; the owner IP is replaced by a documentation address
    // (c0000286 = 192.0.2.134).
    const OWNER_LIST_HEX: &str = concat!(
        "00010000000000830264657630310020202020202020202055534220466c6173",
        "68204d656d6f727900000000008083093065444831313100000000000000000000",
        "000000080000000064657630320047656e65726963204d6173732053746f726167",
        "652044657669636500c00002868083058f6387483131320000000000000000000000",
        "00000800000000",
    );

    // Device list from a capture with a full-speed device (dev01 = "STLink", tag F111,
    // class 0xff) and a high-speed stick (dev02 = "USB Flash Memory", tag H112, class 0x08).
    const FULLSPEED_LIST_HEX: &str = concat!(
        "00010000000000870264657630310053544d6963726f656c656374726f6e6963732053544d3332",
        "2053544c696e6b00000000008083048337484631313100000000000000000000000000ff000000",
        "0064657630320020202020202020202055534220466c617368204d656d6f727900000000008083",
        "0930654448313132000000000000000000000000000800000000",
    );

    fn hx(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    #[test]
    fn device_list_parses_owner_ip() {
        let msg = parse(&hx(OWNER_LIST_HEX)).unwrap();
        let list = match msg {
            Message::DeviceList(l) => l,
            other => panic!("expected device list, got {other:?}"),
        };
        assert_eq!(list.count, 2);
        let by: std::collections::HashMap<_, _> =
            list.devices.iter().map(|d| (d.dev_id.clone(), d)).collect();
        assert_eq!(by["dev01"].owner_ip, ""); // free
        assert_eq!(by["dev02"].owner_ip, "192.0.2.134"); // held
        assert_eq!(by["dev02"].id_vendor, 0x058f);
        assert!(by["dev01"].description.contains("USB Flash Memory"));
        assert_eq!(by["dev02"].description, "Generic Mass Storage Device");
    }

    #[test]
    fn device_speed_and_class_full_vs_high() {
        let msg = parse(&hx(FULLSPEED_LIST_HEX)).unwrap();
        let list = match msg {
            Message::DeviceList(l) => l,
            other => panic!("expected device list, got {other:?}"),
        };
        let by: std::collections::HashMap<_, _> =
            list.devices.iter().map(|d| (d.dev_id.clone(), d)).collect();
        assert_eq!(by["dev01"].tag, "F111");
        assert_eq!(by["dev01"].speed(), "12 Mbps (Full-Speed)");
        assert_eq!(by["dev01"].usb_class, 0xff);
        assert_eq!(by["dev01"].device_type(), "Other USB device");
        assert_eq!(by["dev02"].tag, "H112");
        assert_eq!(by["dev02"].speed(), "480 Mbps (High-Speed)");
        assert_eq!(by["dev02"].usb_class, 0x08);
        assert_eq!(by["dev02"].device_type(), "Mass storage device");
    }

    // Real live device list from the server (2026-09) with a COMPOSITE device:
    // dev01 = USB stick (class 0x08); dev02/03/04 = Sony PS-HX500 (one physical device,
    // split into one item per interface: tag suffix M0/M1/M2, bDeviceClass 0xEF,
    // interface classes Audio 0x01 / Audio 0x01 / HID 0x03). dev03/dev04 have marker
    // 0x8483 (high byte varies!) - breaks the old b"\x80\x83" search.
    const COMPOSITE_LIST_HEX: &str = concat!(
        "00010000000001010464657630310020202020202020202055534220466c617368204d656d6f7279",
        "000000000080830930654448313132000000000000000000000000000800000000646576303200",
        "536f6e7920436f72706f726174696f6e2050532d485835303000000000008083054c0baa4831313",
        "14d3000000000000000000000ef0100000000646576303300536f6e7920436f72706f726174696f",
        "6e2050532d485835303000000000008483054c0baa483131314d3100000000000000000000ef0100",
        "000000646576303400536f6e7920436f72706f726174696f6e2050532d48583530300000000000848",
        "3054c0baa483131314d3200000000000000000000ef0300000000",
    );

    #[test]
    fn composite_device_all_entries_parsed() {
        // All 4 raw items parse correctly - including dev04 (previously swallowed) and
        // the classes from the second byte (bInterfaceClass), regardless of tag length.
        let list = match parse(&hx(COMPOSITE_LIST_HEX)).unwrap() {
            Message::DeviceList(l) => l,
            other => panic!("expected device list, got {other:?}"),
        };
        assert_eq!(list.count, 4);
        assert_eq!(list.devices.len(), 4);
        let by: std::collections::HashMap<_, _> =
            list.devices.iter().map(|d| (d.dev_id.clone(), d)).collect();
        assert_eq!(by["dev01"].usb_class, 0x08);
        assert!(!by["dev01"].is_composite());
        assert_eq!(by["dev02"].usb_class, 0x01);
        assert_eq!(by["dev02"].dev_class, 0xef);
        assert_eq!(by["dev03"].usb_class, 0x01); // marker was 0x8483
        assert_eq!(by["dev04"].usb_class, 0x03);
        assert_eq!(by["dev04"].interface_index(), Some(2));
        assert_eq!(by["dev04"].tag_base(), "H111");
        assert!(by["dev04"].is_composite());
        assert_eq!((by["dev02"].id_vendor, by["dev02"].id_product), (0x054c, 0x0baa));
    }

    #[test]
    fn composite_device_merged_to_audio() {
        // SX-VL behaviour: the composite device becomes ONE row, type Audio (not HID).
        let list = match parse(&hx(COMPOSITE_LIST_HEX)).unwrap() {
            Message::DeviceList(l) => l,
            other => panic!("expected device list, got {other:?}"),
        };
        let lds = logical_devices(&list.devices);
        assert_eq!(lds.len(), 2); // stick + Sony
        let stick = &lds[0];
        let sony = &lds[1];
        assert!(!stick.is_composite());
        assert_eq!(stick.device_type(), "Mass storage device");
        assert!(sony.is_composite());
        assert_eq!(sony.dev_id(), "dev02"); // leading interface M0
        assert_eq!(sony.if_classes(), vec![0x01, 0x01, 0x03]);
        assert_eq!(sony.representative_class(), 0x01);
        assert_eq!(sony.device_type(), "Audio");
    }

    #[test]
    fn representative_class_priority() {
        assert_eq!(representative_class(&[0x01, 0x03], ""), 0x01); // Audio beats HID
        assert_eq!(representative_class(&[0x03, 0x08], ""), 0x08); // Storage beats HID
        assert_eq!(representative_class(&[0x07, 0x01, 0x03], ""), 0x07); // Printer beats all
        assert_eq!(representative_class(&[0x03], ""), 0x03); // only HID -> HID
        assert_eq!(representative_class(&[0x08], ""), 0x08); // single device unchanged
        assert_eq!(representative_class(&[0xff], ""), 0xff); // Vendor -> Other
        assert_eq!(representative_class(&[0xff], "Canon Scanner"), 0x06); // "scan" -> Imaging
    }

    #[test]
    fn dev_index_high_byte() {
        assert_eq!(UsbDeviceEntry::new("dev01", "", 0, 0, "").dev_index(), 3);
        assert_eq!(UsbDeviceEntry::new("dev02", "", 0, 0, "").dev_index(), 4);
        assert_eq!(UsbDeviceEntry::new("dev10", "", 0, 0, "").dev_index(), 12);
    }

    #[test]
    fn mac_to_link_local_eui64() {
        assert_eq!(
            mac_to_link_local(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
            "fe80::211:22ff:fe33:4455"
        );
        assert_eq!(mac_to_link_local(&[]), "");
        assert_eq!(mac_to_link_local(&[0, 1, 2]), "");
    }

    #[test]
    fn get_list_command_roundtrip() {
        for phase in [Phase::Query, Phase::Commit] {
            let pkt = build_get_device_list(phase, None);
            assert_eq!(pkt.len(), SIZE_GET);
            let msg = parse(&pkt).unwrap();
            let cmd = match msg {
                Message::Command(c) => c,
                other => panic!("expected command, got {other:?}"),
            };
            assert_eq!(cmd.command, CMD_GET_LIST);
            let rebuilt = build_get_device_list(cmd.phase, Some(cmd.op_code));
            assert_eq!(rebuilt, pkt);
        }
    }

    #[test]
    fn connect_disconnect_command_roundtrip() {
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        let connect = build_connect(&mac, "192.0.2.99", "dev02", Phase::Query, &CONNECT_TOKEN, None).unwrap();
        assert_eq!(connect.len(), SIZE_EXEC_QUERY);
        let cmd = match parse(&connect).unwrap() {
            Message::Command(c) => c,
            other => panic!("expected command, got {other:?}"),
        };
        assert_eq!(cmd.command, CMD_CONNECT);
        assert_eq!(cmd.mac.as_deref(), Some(&mac[..]));
        assert_eq!(cmd.host_ip.as_deref(), Some("192.0.2.99"));
        assert_eq!(cmd.dev_id.as_deref(), Some("dev02"));
        assert_eq!(cmd.token, CONNECT_TOKEN);
        let rebuilt = build_connect(
            cmd.mac.as_deref().unwrap(),
            cmd.host_ip.as_deref().unwrap(),
            cmd.dev_id.as_deref().unwrap(),
            cmd.phase,
            &cmd.token,
            Some(cmd.op_code),
        )
        .unwrap();
        assert_eq!(rebuilt, connect);

        let disc = build_disconnect(&mac, "192.0.2.99", "dev02", Phase::Commit, None).unwrap();
        assert_eq!(disc.len(), SIZE_EXEC_COMMIT);
        let cmd = match parse(&disc).unwrap() {
            Message::Command(c) => c,
            other => panic!("expected command, got {other:?}"),
        };
        assert_eq!(cmd.command, CMD_DISCONNECT);
        let rebuilt = build_disconnect(
            cmd.mac.as_deref().unwrap(),
            cmd.host_ip.as_deref().unwrap(),
            cmd.dev_id.as_deref().unwrap(),
            cmd.phase,
            Some(cmd.op_code),
        )
        .unwrap();
        assert_eq!(rebuilt, disc);
    }

    #[test]
    fn poll_roundtrip() {
        let pkt = build_poll();
        assert_eq!(pkt.len(), SIZE_GET);
        assert!(matches!(parse(&pkt).unwrap(), Message::Poll));
    }

    #[test]
    fn too_short_is_error() {
        assert!(parse(&[0x00, 0x01]).is_err());
    }
}
