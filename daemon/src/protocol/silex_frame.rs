//! Silex USB tunnel framing codec (TCP/19540).
//!
//! Wire format reconstructed from packet captures. A Silex device server tunnels raw
//! USB transfers over TCP; each frame is a header plus an optional USB payload. All
//! multi-byte fields are BIG-endian.
//!
//! Common 8-byte prefix::
//!
//!     +0  u16  opcode   low byte: 0x01=control, 0x02=interrupt, 0x03=bulk, 0x04=iso;
//!                        bit 0x8000 set = OUT
//!     +2  u16  seq      monotonic sequence; a response mirrors the request's
//!     +4  u16  sel      request: 0x03<<8 | endpoint; response: status1
//!     +6  u16  tag      request: 0x0205 (constant); response: part of length field
//!
//! Control request (len 20 [+ wLength on OUT]): +8 u16 0x0008, u8 bmRequestType,
//! u8 bRequest, u16 wValue, u16 wIndex, u32 wLength, then OUT data.
//!
//! Interrupt request (len 16 [+ len on OUT]): +8 u16 0x8000|wMaxPacketSize,
//! u16 bInterval, u32 length, then OUT data. The device holds interrupt-IN requests
//! open (long-poll) and answers only when the endpoint yields a report.
//!
//! Bulk request (len 16 [+ len on OUT]): +8 u32 0x82000000, u32 length, then OUT data.
//!
//! Isochronous request (24-byte header): +4 u16 field_a 0x0001, u16 sel, u16 tag 0x0205,
//! u16 0x8000|wMaxPacketSize, u32 start_frame, u16 npkts, u16 flags, u32 length, then
//! npkts descriptors {u32 offset, u32 length, u32 status}. IN carries descriptors only;
//! OUT carries descriptors + audio (or a compressed payload with flags 0x8000). The
//! response payload is {u16 npkts, npkts descriptors {offset, actual_length, status},
//! packed audio}.
//!
//! Response (len 12 + length): +4 u16 status1, u32 length, u16 status2, then data.
//!
//! The device index lives in the HIGH byte of the sel word of every request
//! (dev01 -> 0x03, dev02 -> 0x04, ...); it is preserved here for a lossless round-trip.

use anyhow::{bail, Result};

use crate::wire::{need, rd_u16, rd_u32};

pub const DIR_OUT_BIT: u16 = 0x8000;
pub const REQUEST_TAG: u16 = 0x0205; // tag word in requests
pub const CTRL_SETUP_MARKER: u16 = 0x0008; // marker word in control requests
pub const BULK_MARKER: u32 = 0x8200_0000; // marker dword in bulk requests
pub const ISO_OUT_COMPRESSED_FLAG: u16 = 0x8000; // iso-OUT header field +18: audio is compressed
const ISO_FIELD_A: u16 = 0x0001; // iso request +4: fixed template word (constant in the capture)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferType {
    Control = 0x01,
    Interrupt = 0x02,
    Bulk = 0x03,
    Isochronous = 0x04,
}

impl TransferType {
    fn from_opcode(opcode: u16) -> Option<Self> {
        match opcode & 0xFF {
            0x01 => Some(TransferType::Control),
            0x02 => Some(TransferType::Interrupt),
            0x03 => Some(TransferType::Bulk),
            0x04 => Some(TransferType::Isochronous),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SilexFrame {
    pub opcode: u16,
    pub seq: u16,
    pub is_request: bool,
    pub data: Vec<u8>,

    // Request fields
    pub endpoint: Option<u8>, // from the sel word (requests)
    pub bm_request_type: Option<u8>,
    pub b_request: Option<u8>,
    pub w_value: Option<u16>,
    pub w_index: Option<u16>,
    pub w_length: Option<u32>,

    // Response fields
    pub status1: u16, // sel word [4:6] in responses
    pub status2: u16, // [10:12]

    // Isochronous fields (opcode low byte 0x04).
    // Request: `iso_packets` = [(offset, length), ...] (requested packets).
    // Response: `iso_resp_packets` = [(offset, actual_length, status), ...].
    pub iso_packets: Vec<(u32, u32)>,
    pub iso_resp_packets: Vec<(u32, u32, u32)>,
    pub start_frame: u32, // request +12; incremented per URB
    pub maxpacket: u16,   // iso/interrupt request; already includes the 0x8000 flag
    pub total_length: u32, // request +20; transfer_buffer_length
    pub interval: u16,    // interrupt request +10; bInterval (poll interval)
    // Iso-OUT: prebuilt compressed payload (sub-header + codes + literals). When set,
    // the frame is sent compressed (field +18 bit 0x8000, descriptors folded in);
    // otherwise raw (header + descriptors + `data` audio).
    pub iso_out_payload: Option<Vec<u8>>,

    // Preserved header words for a lossless round-trip.
    pub sel: Option<u16>,   // sel word [4:6] in requests (high byte = device index)
    pub tag: u16,           // tag word [6:8]
    pub marker: Option<u32>,
}

impl SilexFrame {
    pub fn transfer_type(&self) -> TransferType {
        TransferType::from_opcode(self.opcode).expect("valid opcode")
    }

    /// True when the host sends data in this transfer (OUT).
    pub fn is_out(&self) -> bool {
        self.opcode & DIR_OUT_BIT != 0
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.opcode.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        let ttype = self.transfer_type();

        if self.is_request && ttype == TransferType::Isochronous {
            // Iso request: opcode seq field_a sel tag maxpkt | start_frame npkts flags total | ...
            let sel = self
                .sel
                .unwrap_or((0x03u16 << 8) | (self.endpoint.unwrap_or(0) as u16));
            out.extend_from_slice(&ISO_FIELD_A.to_be_bytes());
            out.extend_from_slice(&sel.to_be_bytes());
            out.extend_from_slice(&self.tag.to_be_bytes());
            out.extend_from_slice(&self.maxpacket.to_be_bytes());
            if !self.is_out() {
                // IN (recording): header + descriptors, no payload; +20 = transfer
                // buffer length (reservation), +18 = 0.
                out.extend_from_slice(&self.start_frame.to_be_bytes());
                out.extend_from_slice(&(self.iso_packets.len() as u16).to_be_bytes());
                out.extend_from_slice(&0u16.to_be_bytes());
                out.extend_from_slice(&self.total_length.to_be_bytes());
                for (off, length) in &self.iso_packets {
                    out.extend_from_slice(&off.to_be_bytes());
                    out.extend_from_slice(&length.to_be_bytes());
                    out.extend_from_slice(&0u32.to_be_bytes());
                }
                return out;
            }
            // OUT (playback): either COMPRESSED (descriptors folded into the entropy
            // stream, field +18 = 0x8000) or RAW (header + descriptors + audio, +18 = 0).
            // Field +20 = length of the payload AFTER the 24-byte header.
            let (body, flags): (Vec<u8>, u16) = match &self.iso_out_payload {
                Some(p) => (p.clone(), ISO_OUT_COMPRESSED_FLAG),
                None => {
                    let mut b = Vec::new();
                    for (off, length) in &self.iso_packets {
                        b.extend_from_slice(&off.to_be_bytes());
                        b.extend_from_slice(&length.to_be_bytes());
                        b.extend_from_slice(&0u32.to_be_bytes());
                    }
                    b.extend_from_slice(&self.data);
                    (b, 0)
                }
            };
            out.extend_from_slice(&self.start_frame.to_be_bytes());
            out.extend_from_slice(&(self.iso_packets.len() as u16).to_be_bytes());
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(&body);
            return out;
        }

        if self.is_request {
            let sel = self
                .sel
                .unwrap_or((0x03u16 << 8) | (self.endpoint.unwrap_or(0) as u16));
            out.extend_from_slice(&sel.to_be_bytes());
            out.extend_from_slice(&self.tag.to_be_bytes());
            match ttype {
                TransferType::Control => {
                    out.extend_from_slice(&CTRL_SETUP_MARKER.to_be_bytes());
                    out.push(self.bm_request_type.unwrap_or(0));
                    out.push(self.b_request.unwrap_or(0));
                    out.extend_from_slice(&self.w_value.unwrap_or(0).to_be_bytes());
                    out.extend_from_slice(&self.w_index.unwrap_or(0).to_be_bytes());
                    out.extend_from_slice(&self.w_length.unwrap_or(0).to_be_bytes());
                }
                TransferType::Interrupt => {
                    // opcode seq sel tag | maxpkt(0x8000|mps) interval length | [OUT data]
                    let length = self.w_length.unwrap_or(if self.is_out() {
                        self.data.len() as u32
                    } else {
                        0
                    });
                    out.extend_from_slice(&self.maxpacket.to_be_bytes());
                    out.extend_from_slice(&self.interval.to_be_bytes());
                    out.extend_from_slice(&length.to_be_bytes());
                    if self.is_out() {
                        out.extend_from_slice(&self.data);
                    }
                    return out;
                }
                TransferType::Bulk => {
                    out.extend_from_slice(&BULK_MARKER.to_be_bytes());
                    let length = if self.is_out() {
                        self.data.len() as u32
                    } else {
                        self.w_length.unwrap_or(0)
                    };
                    out.extend_from_slice(&length.to_be_bytes());
                }
                TransferType::Isochronous => unreachable!("iso handled above"),
            }
            if self.is_out() {
                out.extend_from_slice(&self.data);
            }
            return out;
        }

        // Response: status1(u16) length(u32) status2(u16)
        out.extend_from_slice(&self.status1.to_be_bytes());
        out.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.status2.to_be_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Parse exactly one frame at `offset`; returns (frame, next_offset).
    pub fn parse(buf: &[u8], offset: usize) -> Result<(SilexFrame, usize)> {
        need(buf, offset + 12, "SilexFrame")?;
        let opcode = rd_u16(buf, offset);
        let seq = rd_u16(buf, offset + 2);
        let w4 = rd_u16(buf, offset + 4);
        let w6 = rd_u16(buf, offset + 6);
        let ttype = match TransferType::from_opcode(opcode) {
            Some(t) => t,
            None => bail!("unknown transfer type opcode={opcode:#06x} @ {offset}"),
        };
        let is_request = w6 == REQUEST_TAG;
        let out = opcode & DIR_OUT_BIT != 0;

        if is_request {
            let mut f = SilexFrame {
                opcode,
                seq,
                is_request: true,
                endpoint: Some((w4 & 0xFF) as u8),
                sel: Some(w4),
                tag: w6,
                ..Default::default()
            };
            let (body, dlen) = match ttype {
                TransferType::Control => {
                    need(buf, offset + 20, "control request header")?;
                    f.marker = Some(rd_u16(buf, offset + 8) as u32);
                    f.bm_request_type = Some(buf[offset + 10]);
                    f.b_request = Some(buf[offset + 11]);
                    f.w_value = Some(rd_u16(buf, offset + 12));
                    f.w_index = Some(rd_u16(buf, offset + 14));
                    let wlen = rd_u32(buf, offset + 16);
                    f.w_length = Some(wlen);
                    (offset + 20, if out { wlen as usize } else { 0 })
                }
                TransferType::Interrupt => {
                    need(buf, offset + 16, "interrupt request header")?;
                    f.maxpacket = rd_u16(buf, offset + 8);
                    f.interval = rd_u16(buf, offset + 10);
                    let length = rd_u32(buf, offset + 12);
                    f.w_length = Some(length); // requested/transferred length
                    (offset + 16, if out { length as usize } else { 0 })
                }
                TransferType::Bulk => {
                    need(buf, offset + 16, "bulk request header")?;
                    f.marker = Some(rd_u32(buf, offset + 8));
                    let length = rd_u32(buf, offset + 12);
                    f.w_length = Some(length);
                    (offset + 16, if out { length as usize } else { 0 })
                }
                TransferType::Isochronous => bail!("iso request parsing unsupported @ {offset}"),
            };
            need(buf, body + dlen, "request payload")?;
            f.data = buf[body..body + dlen].to_vec();
            return Ok((f, body + dlen));
        }

        // Response: status1=w4 ([4:6]); length u32 @[6:10]; status2 u16 @[10:12]
        let dlen = rd_u32(buf, offset + 6) as usize;
        let status2 = rd_u16(buf, offset + 10);
        let body = offset + 12;
        need(buf, body + dlen, "response payload")?;
        let payload = &buf[body..body + dlen];
        let mut f = SilexFrame {
            opcode,
            seq,
            is_request: false,
            status1: w4,
            status2,
            tag: w6,
            ..Default::default()
        };
        if ttype == TransferType::Isochronous && dlen >= 2 {
            // Payload: u16 npkts | npkts*{offset, actual_length, status} | packed audio
            let npkts = rd_u16(payload, 0) as usize;
            let mut packets = Vec::with_capacity(npkts);
            let mut p = 2;
            for _ in 0..npkts {
                need(payload, p + 12, "iso response descriptor")?;
                packets.push((rd_u32(payload, p), rd_u32(payload, p + 4), rd_u32(payload, p + 8)));
                p += 12;
            }
            f.iso_resp_packets = packets;
            f.data = payload[p..].to_vec();
        } else {
            f.data = payload.to_vec();
        }
        Ok((f, body + dlen))
    }
}

/// Split a (reassembled) directional turn into individual frames.
#[cfg(test)]
pub fn iter_frames(data: &[u8]) -> Result<Vec<SilexFrame>> {
    let mut frames = Vec::new();
    let mut off = 0;
    while off < data.len() {
        let (frame, next) = SilexFrame::parse(data, off)?;
        frames.push(frame);
        off = next;
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: &SilexFrame) {
        let bytes = frame.to_bytes();
        let (parsed, next) = SilexFrame::parse(&bytes, 0).unwrap();
        assert_eq!(next, bytes.len());
        assert_eq!(parsed.to_bytes(), bytes);
    }

    #[test]
    fn control_in_request_roundtrip() {
        // GET_DESCRIPTOR(device), dev02 (sel high byte 0x04), ep0, wLength 18.
        let f = SilexFrame {
            opcode: 0x0001,
            seq: 0x0001,
            is_request: true,
            endpoint: Some(0),
            bm_request_type: Some(0x80),
            b_request: Some(0x06),
            w_value: Some(0x0200),
            w_index: Some(0),
            w_length: Some(18),
            sel: Some(0x0400),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), 20); // control IN request has no payload
        // sel high byte = device index 0x04
        assert_eq!(bytes[4], 0x04);
        roundtrip(&f);
    }

    #[test]
    fn control_out_request_carries_payload() {
        let f = SilexFrame {
            opcode: 0x0001 | DIR_OUT_BIT,
            seq: 2,
            is_request: true,
            endpoint: Some(0),
            bm_request_type: Some(0x00),
            b_request: Some(0x09),
            w_value: Some(1),
            w_index: Some(0),
            w_length: Some(0),
            data: vec![],
            sel: Some(0x0300),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        roundtrip(&f);
    }

    #[test]
    fn bulk_out_request_with_cbw() {
        let mut cbw = b"USBC".to_vec();
        cbw.extend_from_slice(&[0u8; 27]); // 31-byte CBW
        let f = SilexFrame {
            opcode: 0x0003 | DIR_OUT_BIT,
            seq: 5,
            is_request: true,
            endpoint: Some(1),
            data: cbw.clone(),
            sel: Some(0x0301),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let bytes = f.to_bytes();
        // 16-byte header + 31-byte payload; bulk marker present
        assert_eq!(rd_u32(&bytes, 8), BULK_MARKER);
        assert_eq!(rd_u32(&bytes, 12), 31);
        assert_eq!(&bytes[16..], &cbw[..]);
        roundtrip(&f);
    }

    #[test]
    fn bulk_in_request_requests_length_only() {
        let f = SilexFrame {
            opcode: 0x0003, // IN
            seq: 6,
            is_request: true,
            endpoint: Some(0x82),
            w_length: Some(512),
            sel: Some(0x0382),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), 16); // IN request: header only
        assert_eq!(rd_u32(&bytes, 12), 512);
        roundtrip(&f);
    }

    #[test]
    fn response_roundtrip_and_length_is_u32() {
        let f = SilexFrame {
            opcode: 0x0001,
            seq: 1,
            is_request: false,
            status1: 0,
            status2: 0,
            data: (0u8..18).collect(),
            ..Default::default()
        };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), 12 + 18);
        assert_eq!(rd_u32(&bytes, 6), 18); // length is u32 at [6:10]
        let (parsed, next) = SilexFrame::parse(&bytes, 0).unwrap();
        assert_eq!(next, bytes.len());
        assert!(!parsed.is_request);
        assert_eq!(parsed.data.len(), 18);
        roundtrip(&f);
    }

    #[test]
    fn iter_frames_splits_concatenation() {
        let a = SilexFrame {
            opcode: 0x0001, seq: 1, is_request: false, data: vec![1, 2, 3],
            ..Default::default()
        };
        let b = SilexFrame {
            opcode: 0x0001, seq: 2, is_request: false, data: vec![4, 5],
            ..Default::default()
        };
        let mut stream = a.to_bytes();
        stream.extend_from_slice(&b.to_bytes());
        let frames = iter_frames(&stream).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, vec![1, 2, 3]);
        assert_eq!(frames[1].data, vec![4, 5]);
    }

    #[test]
    fn too_short_is_error() {
        assert!(SilexFrame::parse(&[0u8; 4], 0).is_err());
    }

    #[test]
    fn interrupt_in_request_roundtrip() {
        // Interrupt IN, ep 0x84, mps 8 -> maxpacket 0x8008, interval 10, length 8.
        let f = SilexFrame {
            opcode: 0x0002,
            seq: 3,
            is_request: true,
            endpoint: Some(0x84),
            w_length: Some(8),
            maxpacket: 0x8008,
            interval: 10,
            sel: Some(0x0384),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), 16); // IN request: header only
        assert_eq!(rd_u16(&bytes, 8), 0x8008);
        assert_eq!(rd_u16(&bytes, 10), 10);
        assert_eq!(rd_u32(&bytes, 12), 8);
        roundtrip(&f);
    }

    #[test]
    fn interrupt_out_request_carries_payload() {
        let f = SilexFrame {
            opcode: 0x0002 | DIR_OUT_BIT,
            seq: 4,
            is_request: true,
            endpoint: Some(0x04),
            maxpacket: 0x8008,
            interval: 4,
            data: vec![1, 2, 3, 4],
            w_length: Some(4),
            sel: Some(0x0304),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), 16 + 4);
        assert_eq!(&bytes[16..], &[1, 2, 3, 4]);
        roundtrip(&f);
    }

    #[test]
    fn iso_in_request_encodes_header_and_descriptors() {
        let f = SilexFrame {
            opcode: 0x0004,
            seq: 7,
            is_request: true,
            endpoint: Some(0x83),
            iso_packets: vec![(0, 192), (192, 192)],
            start_frame: 1000,
            maxpacket: 0x8000 | 192,
            total_length: 2 * (192 + 12),
            sel: Some(0x0383),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let b = f.to_bytes();
        assert_eq!(b.len(), 24 + 24); // 24-byte header + 2*12 descriptors, no payload
        assert_eq!(rd_u16(&b, 4), 0x0001); // field_a
        assert_eq!(rd_u16(&b, 6), 0x0383); // sel
        assert_eq!(rd_u16(&b, 8), REQUEST_TAG); // tag
        assert_eq!(rd_u16(&b, 10), 0x8000 | 192); // maxpacket
        assert_eq!(rd_u32(&b, 12), 1000); // start_frame
        assert_eq!(rd_u16(&b, 16), 2); // npkts
        assert_eq!(rd_u16(&b, 18), 0); // flags
        assert_eq!(rd_u32(&b, 20), 2 * (192 + 12)); // total_length
        assert_eq!(rd_u32(&b, 24), 0); // desc0.offset
        assert_eq!(rd_u32(&b, 28), 192); // desc0.length
        assert_eq!(rd_u32(&b, 32), 0); // desc0.status
    }

    #[test]
    fn iso_out_request_raw_appends_audio() {
        let f = SilexFrame {
            opcode: 0x8004,
            seq: 8,
            is_request: true,
            endpoint: Some(0x02),
            iso_packets: vec![(0, 4)],
            data: vec![0xAA, 0xBB, 0xCC, 0xDD],
            start_frame: 5,
            maxpacket: 0x8000 | 4,
            sel: Some(0x0302),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let b = f.to_bytes();
        assert_eq!(rd_u16(&b, 18), 0); // flags: raw
        assert_eq!(rd_u32(&b, 20), 12 + 4); // body length
        assert_eq!(b.len(), 24 + 12 + 4);
        assert_eq!(&b[24 + 12..], &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn iso_out_request_compressed_sets_flag() {
        let f = SilexFrame {
            opcode: 0x8004,
            seq: 9,
            is_request: true,
            endpoint: Some(0x02),
            iso_packets: vec![(0, 4)],
            iso_out_payload: Some(vec![2, 1, 0, 0, 0xFF]),
            start_frame: 5,
            maxpacket: 0x8000 | 4,
            sel: Some(0x0302),
            tag: REQUEST_TAG,
            ..Default::default()
        };
        let b = f.to_bytes();
        assert_eq!(rd_u16(&b, 18), ISO_OUT_COMPRESSED_FLAG);
        assert_eq!(rd_u32(&b, 20), 5); // payload length
        assert_eq!(&b[24..], &[2, 1, 0, 0, 0xFF]);
    }

    #[test]
    fn iso_response_parses_descriptors_and_audio() {
        // npkts=1, {offset=0, actual_length=3, status=0}, audio=[9,8,7]
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u16.to_be_bytes());
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&3u32.to_be_bytes());
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&[9, 8, 7]);
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x0004u16.to_be_bytes()); // opcode
        buf.extend_from_slice(&1u16.to_be_bytes()); // seq
        buf.extend_from_slice(&0u16.to_be_bytes()); // status1
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes()); // length
        buf.extend_from_slice(&0u16.to_be_bytes()); // status2
        buf.extend_from_slice(&payload);
        let (f, next) = SilexFrame::parse(&buf, 0).unwrap();
        assert_eq!(next, buf.len());
        assert!(!f.is_request);
        assert_eq!(f.iso_resp_packets, vec![(0, 3, 0)]);
        assert_eq!(f.data, vec![9, 8, 7]);
    }
}
