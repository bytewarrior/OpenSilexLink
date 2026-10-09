//! USB/IP wire protocol (server side). Kernel doc: usb/usbip_protocol.rst.
//!
//! We implement the *server* role: the local Linux client (`usbip attach`, kernel
//! `vhci-hcd`) connects to us and imports a device. The URB stream (CMD_SUBMIT/
//! RET_SUBMIT, CMD_UNLINK/RET_UNLINK) then flows over the same TCP connection and we
//! translate it into Silex frames.
//!
//! All fields big-endian. Two phases:
//!
//! 1. OP phase (negotiation), 8-byte header: u16 version (0x0111), u16 code, u32 status.
//!    - OP_REQ_DEVLIST 0x8005 / OP_REP_DEVLIST 0x0005
//!    - OP_REQ_IMPORT  0x8003 / OP_REP_IMPORT  0x0003 (+ busid[32] in the request)
//! 2. URB phase, 48-byte header (usbip_header): basic(20) + submit/ret(28).

use anyhow::{ensure, Result};

use crate::wire::{need, rd_u32, rd_u16};

pub const USBIP_VERSION: u16 = 0x0111;

/// One iso_packet_descriptor on the USB/IP wire: offset, length, actual_length,
/// status - each u32 BE (16 bytes). Follows the transfer_buffer on ISO URBs.
pub const ISO_DESC_SIZE: usize = 16;

// OP codes
pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

// URB commands
pub const USBIP_CMD_SUBMIT: u32 = 0x0000_0001;
pub const USBIP_RET_SUBMIT: u32 = 0x0000_0003;
pub const USBIP_CMD_UNLINK: u32 = 0x0000_0002;
pub const USBIP_RET_UNLINK: u32 = 0x0000_0004;

// Direction
#[cfg(test)]
pub const USBIP_DIR_OUT: u32 = 0;
pub const USBIP_DIR_IN: u32 = 1;

pub const TCP_PORT: u16 = 3240;

// --------------------------------------------------------------------------- //
//  OP phase
// --------------------------------------------------------------------------- //
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

impl OpHeader {
    pub const SIZE: usize = 8;

    pub fn new(version: u16, code: u16, status: u32) -> Self {
        Self { version, code, status }
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        need(buf, 8, "OpHeader")?;
        Ok(Self { version: rd_u16(buf, 0), code: rd_u16(buf, 2), status: rd_u32(buf, 4) })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
        out
    }
}

/// usbip_usb_device (312 bytes) - describes an exported device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub b_device_class: u8,
    pub b_device_subclass: u8,
    pub b_device_protocol: u8,
    pub b_configuration_value: u8,
    pub b_num_configurations: u8,
    pub b_num_interfaces: u8,
}

impl UsbDevice {
    pub const SIZE: usize = 256 + 32 + 4 * 3 + 2 * 3 + 6;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        push_fixed(&mut out, self.path.as_bytes(), 256);
        push_fixed(&mut out, self.busid.as_bytes(), 32);
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&self.speed.to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.push(self.b_device_class);
        out.push(self.b_device_subclass);
        out.push(self.b_device_protocol);
        out.push(self.b_configuration_value);
        out.push(self.b_num_configurations);
        out.push(self.b_num_interfaces);
        out
    }

    #[cfg(test)]
    pub fn parse(buf: &[u8], off: usize) -> Result<Self> {
        need(buf, off + Self::SIZE, "UsbDevice")?;
        let path = crate::wire::cstr_field(buf, off, Some(256));
        let busid = crate::wire::cstr_field(buf, off + 256, Some(32));
        let busnum = rd_u32(buf, off + 288);
        let devnum = rd_u32(buf, off + 292);
        let speed = rd_u32(buf, off + 296);
        let id_vendor = rd_u16(buf, off + 300);
        let id_product = rd_u16(buf, off + 302);
        let bcd_device = rd_u16(buf, off + 304);
        Ok(Self {
            path,
            busid,
            busnum,
            devnum,
            speed,
            id_vendor,
            id_product,
            bcd_device,
            b_device_class: buf[off + 306],
            b_device_subclass: buf[off + 307],
            b_device_protocol: buf[off + 308],
            b_configuration_value: buf[off + 309],
            b_num_configurations: buf[off + 310],
            b_num_interfaces: buf[off + 311],
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbInterface {
    pub b_interface_class: u8,
    pub b_interface_subclass: u8,
    pub b_interface_protocol: u8,
}

impl UsbInterface {
    pub fn to_bytes(&self) -> Vec<u8> {
        vec![
            self.b_interface_class,
            self.b_interface_subclass,
            self.b_interface_protocol,
            0,
        ]
    }
}

/// OP_REP_IMPORT: on success with the device description, otherwise status=1.
pub fn build_rep_import(dev: Option<&UsbDevice>) -> Vec<u8> {
    match dev {
        None => OpHeader::new(USBIP_VERSION, OP_REP_IMPORT, 1).to_bytes(),
        Some(d) => {
            let mut out = OpHeader::new(USBIP_VERSION, OP_REP_IMPORT, 0).to_bytes();
            out.extend_from_slice(&d.to_bytes());
            out
        }
    }
}

pub fn build_rep_devlist(devices: &[(UsbDevice, Vec<UsbInterface>)]) -> Vec<u8> {
    let mut out = OpHeader::new(USBIP_VERSION, OP_REP_DEVLIST, 0).to_bytes();
    out.extend_from_slice(&(devices.len() as u32).to_be_bytes());
    for (dev, ifaces) in devices {
        out.extend_from_slice(&dev.to_bytes());
        for iface in ifaces {
            out.extend_from_slice(&iface.to_bytes());
        }
    }
    out
}

/// Read the busid from an OP_REQ_IMPORT (after the 8-byte header).
#[cfg(test)]
pub fn parse_import_busid(buf: &[u8]) -> Result<String> {
    need(buf, 8 + 32, "OP_REQ_IMPORT busid")?;
    Ok(crate::wire::cstr_field(buf, 8, Some(32)))
}

// --------------------------------------------------------------------------- //
//  URB phase
// --------------------------------------------------------------------------- //
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbipCmdSubmit {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: u32,
    pub number_of_packets: u32,
    pub interval: u32,
    pub setup: [u8; 8],
    pub transfer_buffer: Vec<u8>, // filled only for OUT
    /// ISO: (offset, length, actual_length, status) per packet; on the wire it
    /// follows the transfer_buffer. Filled in by the reader via [`set_iso_from`].
    pub iso_packets: Vec<(u32, u32, u32, u32)>,
}

impl UsbipCmdSubmit {
    pub const HEADER_SIZE: usize = 48;

    pub fn parse(head: &[u8], transfer_buffer: Vec<u8>) -> Result<Self> {
        need(head, Self::HEADER_SIZE, "CMD_SUBMIT")?;
        let command = rd_u32(head, 0);
        ensure!(command == USBIP_CMD_SUBMIT, "not CMD_SUBMIT: {command:#x}");
        let mut setup = [0u8; 8];
        setup.copy_from_slice(&head[40..48]);
        Ok(Self {
            seqnum: rd_u32(head, 4),
            devid: rd_u32(head, 8),
            direction: rd_u32(head, 12),
            ep: rd_u32(head, 16),
            transfer_flags: rd_u32(head, 20),
            transfer_buffer_length: rd_u32(head, 24),
            start_frame: rd_u32(head, 28),
            number_of_packets: rd_u32(head, 32),
            interval: rd_u32(head, 36),
            setup,
            transfer_buffer,
            iso_packets: Vec::new(),
        })
    }

    pub fn is_in(&self) -> bool {
        self.direction == USBIP_DIR_IN
    }

    /// number_of_packets = 0xFFFFFFFF (or 0) => not isochronous.
    pub fn is_iso(&self) -> bool {
        self.number_of_packets > 0 && self.number_of_packets < 0x1000
    }

    /// Take `number_of_packets` descriptors (16 bytes each) from `raw`.
    pub fn set_iso_from(&mut self, raw: &[u8]) -> Result<()> {
        need(raw, self.number_of_packets as usize * ISO_DESC_SIZE, "iso descriptors")?;
        self.iso_packets = (0..self.number_of_packets as usize)
            .map(|i| {
                let b = i * ISO_DESC_SIZE;
                (rd_u32(raw, b), rd_u32(raw, b + 4), rd_u32(raw, b + 8), rd_u32(raw, b + 12))
            })
            .collect();
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbipRetSubmit {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub status: i32,
    pub actual_length: u32,
    pub start_frame: u32,
    pub number_of_packets: u32,
    pub error_count: u32,
    pub data: Vec<u8>, // IN data (appended only for IN transfers)
    /// ISO: (offset, length, actual_length, status) per packet; appended AFTER the
    /// data. number_of_packets must then equal iso_packets.len().
    pub iso_packets: Vec<(u32, u32, u32, u32)>,
}

impl UsbipRetSubmit {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48 + self.data.len() + self.iso_packets.len() * ISO_DESC_SIZE);
        out.extend_from_slice(&USBIP_RET_SUBMIT.to_be_bytes());
        out.extend_from_slice(&self.seqnum.to_be_bytes());
        out.extend_from_slice(&self.devid.to_be_bytes());
        out.extend_from_slice(&self.direction.to_be_bytes());
        out.extend_from_slice(&self.ep.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&self.actual_length.to_be_bytes());
        out.extend_from_slice(&self.start_frame.to_be_bytes());
        out.extend_from_slice(&self.number_of_packets.to_be_bytes());
        out.extend_from_slice(&self.error_count.to_be_bytes());
        out.extend_from_slice(&[0u8; 8]); // setup padding
        out.extend_from_slice(&self.data);
        for (offset, length, actual_length, status) in &self.iso_packets {
            out.extend_from_slice(&offset.to_be_bytes());
            out.extend_from_slice(&length.to_be_bytes());
            out.extend_from_slice(&actual_length.to_be_bytes());
            out.extend_from_slice(&status.to_be_bytes());
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbipCmdUnlink {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub unlink_seqnum: u32,
}

impl UsbipCmdUnlink {
    pub fn parse(head: &[u8]) -> Result<Self> {
        need(head, 24, "CMD_UNLINK")?;
        let command = rd_u32(head, 0);
        ensure!(command == USBIP_CMD_UNLINK, "not CMD_UNLINK: {command:#x}");
        Ok(Self {
            seqnum: rd_u32(head, 4),
            devid: rd_u32(head, 8),
            direction: rd_u32(head, 12),
            ep: rd_u32(head, 16),
            unlink_seqnum: rd_u32(head, 20),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsbipRetUnlink {
    pub seqnum: u32,
    pub status: i32,
}

impl UsbipRetUnlink {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48);
        out.extend_from_slice(&USBIP_RET_UNLINK.to_be_bytes());
        out.extend_from_slice(&self.seqnum.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // devid
        out.extend_from_slice(&0u32.to_be_bytes()); // direction
        out.extend_from_slice(&0u32.to_be_bytes()); // ep
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&[0u8; 24]);
        out
    }
}

/// Return the command field (first 4 bytes) of a 48-byte URB header.
pub fn peek_command(head: &[u8]) -> Result<u32> {
    need(head, 4, "URB header")?;
    Ok(rd_u32(head, 0))
}

fn push_fixed(out: &mut Vec<u8>, bytes: &[u8], width: usize) {
    let n = bytes.len().min(width);
    out.extend_from_slice(&bytes[..n]);
    out.extend(std::iter::repeat(0u8).take(width - n));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_header_roundtrip() {
        let h = OpHeader::new(USBIP_VERSION, OP_REP_IMPORT, 0);
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), OpHeader::SIZE);
        assert_eq!(OpHeader::parse(&bytes).unwrap(), h);
        // version 0x0111, code 0x0003, status 0
        assert_eq!(bytes, [0x01, 0x11, 0x00, 0x03, 0, 0, 0, 0]);
    }

    #[test]
    fn usb_device_roundtrip() {
        let dev = UsbDevice {
            path: "/sys/devices/silex/1-1".into(),
            busid: "1-1".into(),
            busnum: 1,
            devnum: 2,
            speed: 3,
            id_vendor: 0x0930,
            id_product: 0x6544,
            bcd_device: 0x0100,
            b_device_class: 0,
            b_device_subclass: 0,
            b_device_protocol: 0,
            b_configuration_value: 1,
            b_num_configurations: 1,
            b_num_interfaces: 1,
        };
        let bytes = dev.to_bytes();
        assert_eq!(bytes.len(), UsbDevice::SIZE);
        assert_eq!(UsbDevice::parse(&bytes, 0).unwrap(), dev);
    }

    #[test]
    fn rep_import_success_and_failure() {
        assert_eq!(build_rep_import(None).len(), OpHeader::SIZE);
        let dev = UsbDevice {
            path: "p".into(), busid: "1-1".into(), busnum: 1, devnum: 1, speed: 3,
            id_vendor: 1, id_product: 2, bcd_device: 0, b_device_class: 8,
            b_device_subclass: 6, b_device_protocol: 0x50, b_configuration_value: 1,
            b_num_configurations: 1, b_num_interfaces: 1,
        };
        assert_eq!(build_rep_import(Some(&dev)).len(), OpHeader::SIZE + UsbDevice::SIZE);
    }

    #[test]
    fn import_busid_parse() {
        let mut buf = OpHeader::new(USBIP_VERSION, OP_REQ_IMPORT, 0).to_bytes();
        let mut busid = [0u8; 32];
        busid[..3].copy_from_slice(b"1-2");
        buf.extend_from_slice(&busid);
        assert_eq!(parse_import_busid(&buf).unwrap(), "1-2");
    }

    #[test]
    fn cmd_submit_parse_and_ret_roundtrip() {
        // Build a CMD_SUBMIT header (control IN, ep0, len 18) and parse it back.
        let mut head = Vec::new();
        head.extend_from_slice(&USBIP_CMD_SUBMIT.to_be_bytes());
        head.extend_from_slice(&7u32.to_be_bytes()); // seqnum
        head.extend_from_slice(&0u32.to_be_bytes()); // devid
        head.extend_from_slice(&USBIP_DIR_IN.to_be_bytes());
        head.extend_from_slice(&0u32.to_be_bytes()); // ep
        head.extend_from_slice(&0u32.to_be_bytes()); // flags
        head.extend_from_slice(&18u32.to_be_bytes()); // length
        head.extend_from_slice(&0u32.to_be_bytes()); // start_frame
        head.extend_from_slice(&0u32.to_be_bytes()); // number_of_packets
        head.extend_from_slice(&0u32.to_be_bytes()); // interval
        head.extend_from_slice(&[0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]); // setup
        let cmd = UsbipCmdSubmit::parse(&head, Vec::new()).unwrap();
        assert_eq!(cmd.seqnum, 7);
        assert!(cmd.is_in());
        assert_eq!(cmd.transfer_buffer_length, 18);
        assert_eq!(cmd.setup, [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]);

        let ret = UsbipRetSubmit { seqnum: 7, actual_length: 3, data: vec![1, 2, 3], ..Default::default() };
        let bytes = ret.to_bytes();
        assert_eq!(bytes.len(), 48 + 3);
        assert_eq!(rd_u32(&bytes, 0), USBIP_RET_SUBMIT);
        assert_eq!(rd_u32(&bytes, 4), 7);
        assert_eq!(&bytes[48..], &[1, 2, 3]);
    }

    #[test]
    fn unlink_parse_and_ret() {
        let mut head = Vec::new();
        head.extend_from_slice(&USBIP_CMD_UNLINK.to_be_bytes());
        head.extend_from_slice(&9u32.to_be_bytes());
        head.extend_from_slice(&0u32.to_be_bytes());
        head.extend_from_slice(&0u32.to_be_bytes());
        head.extend_from_slice(&0u32.to_be_bytes());
        head.extend_from_slice(&5u32.to_be_bytes()); // unlink_seqnum
        head.extend_from_slice(&[0u8; 24]);
        let u = UsbipCmdUnlink::parse(&head).unwrap();
        assert_eq!(u.seqnum, 9);
        assert_eq!(u.unlink_seqnum, 5);
        let r = UsbipRetUnlink { seqnum: 9, status: 0 }.to_bytes();
        assert_eq!(r.len(), 48);
        assert_eq!(rd_u32(&r, 0), USBIP_RET_UNLINK);
    }

    #[test]
    fn iso_cmd_submit_and_ret() {
        let mut head = Vec::new();
        head.extend_from_slice(&USBIP_CMD_SUBMIT.to_be_bytes());
        head.extend_from_slice(&11u32.to_be_bytes()); // seqnum
        head.extend_from_slice(&0u32.to_be_bytes()); // devid
        head.extend_from_slice(&USBIP_DIR_IN.to_be_bytes());
        head.extend_from_slice(&0x81u32.to_be_bytes()); // ep
        head.extend_from_slice(&0u32.to_be_bytes()); // flags
        head.extend_from_slice(&384u32.to_be_bytes()); // length
        head.extend_from_slice(&0u32.to_be_bytes()); // start_frame
        head.extend_from_slice(&2u32.to_be_bytes()); // number_of_packets
        head.extend_from_slice(&1u32.to_be_bytes()); // interval
        head.extend_from_slice(&[0u8; 8]); // setup
        let mut cmd = UsbipCmdSubmit::parse(&head, Vec::new()).unwrap();
        assert!(cmd.is_iso());

        let mut raw = Vec::new();
        for (o, l, a, s) in [(0u32, 192u32, 0u32, 0u32), (192, 192, 0, 0)] {
            for v in [o, l, a, s] {
                raw.extend_from_slice(&v.to_be_bytes());
            }
        }
        cmd.set_iso_from(&raw).unwrap();
        assert_eq!(cmd.iso_packets, vec![(0, 192, 0, 0), (192, 192, 0, 0)]);

        let ret = UsbipRetSubmit {
            seqnum: 11,
            number_of_packets: 2,
            iso_packets: vec![(0, 192, 190, 0), (192, 192, 188, 0)],
            ..Default::default()
        };
        let bytes = ret.to_bytes();
        assert_eq!(bytes.len(), 48 + 2 * ISO_DESC_SIZE);
        assert_eq!(rd_u32(&bytes, 48), 0); // desc0.offset
        assert_eq!(rd_u32(&bytes, 48 + 8), 190); // desc0.actual_length
    }

    #[test]
    fn is_iso_rejects_sentinels() {
        let mk = |n: u32| {
            let mut head = vec![0u8; UsbipCmdSubmit::HEADER_SIZE];
            head[0..4].copy_from_slice(&USBIP_CMD_SUBMIT.to_be_bytes());
            head[32..36].copy_from_slice(&n.to_be_bytes()); // number_of_packets
            UsbipCmdSubmit::parse(&head, Vec::new()).unwrap()
        };
        assert!(!mk(0).is_iso());
        assert!(!mk(0xFFFF_FFFF).is_iso());
        assert!(mk(8).is_iso());
    }
}
