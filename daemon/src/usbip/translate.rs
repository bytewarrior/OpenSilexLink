//! Translation between USB/IP URBs and Silex tunnel transfers.
//!
//! The kernel (`vhci-hcd`) sends `USBIP_CMD_SUBMIT`; we run the transfer over the
//! [`SilexSession`] and answer with `USBIP_RET_SUBMIT`. Endpoint 0 => control transfer
//! (setup packet in the URB), otherwise bulk, interrupt or isochronous by endpoint type.

use anyhow::Result;

use crate::protocol::silex_frame::SilexFrame;

use super::device::EP_INTERRUPT;
use super::proto::{UsbipCmdSubmit, UsbipRetSubmit};
use super::session::{IsoInReq, SilexSession, TransferError};

// USB status (Silex status1) -> USB/IP urb status (negative errno)
const EPIPE: i32 = -32; // STALL
const EPROTO: i32 = -71;
const ETIMEDOUT: i32 = -110;

fn silex_status_to_errno(status1: u16) -> i32 {
    match status1 {
        0x0000 => 0,
        0xFFFD => EPIPE, // observed in the capture: STALL
        _ => EPROTO,
    }
}

pub fn ret_for(cmd: &UsbipCmdSubmit) -> UsbipRetSubmit {
    UsbipRetSubmit {
        seqnum: cmd.seqnum,
        devid: cmd.devid,
        direction: cmd.direction,
        ep: cmd.ep,
        ..Default::default()
    }
}

/// Run a CMD_SUBMIT over the session and build the RET_SUBMIT.
///
/// A read timeout fails only this URB (ETIMEDOUT); any other tunnel error is returned
/// as `Err`, which tells the caller the session is dead.
pub async fn submit_to_silex(session: &SilexSession, cmd: &UsbipCmdSubmit) -> Result<UsbipRetSubmit> {
    let mut ret = ret_for(cmd);

    if cmd.is_iso() {
        return submit_iso(session, cmd, ret).await;
    }

    let out_data = if cmd.is_in() { Vec::new() } else { cmd.transfer_buffer.clone() };
    let result = if cmd.ep == 0 {
        // Control: the setup packet is standard USB (little-endian).
        let bm_request_type = cmd.setup[0];
        let b_request = cmd.setup[1];
        let w_value = u16::from_le_bytes([cmd.setup[2], cmd.setup[3]]);
        let w_index = u16::from_le_bytes([cmd.setup[4], cmd.setup[5]]);
        let w_length = u16::from_le_bytes([cmd.setup[6], cmd.setup[7]]);
        session
            .control(bm_request_type, b_request, w_value, w_index, w_length, out_data)
            .await
    } else {
        session
            .bulk(cmd.ep as u8, cmd.is_in(), cmd.transfer_buffer_length, out_data)
            .await
    };

    match result {
        Ok((status1, data)) => {
            ret.status = silex_status_to_errno(status1);
            if cmd.is_in() {
                ret.actual_length = data.len() as u32;
                ret.data = data;
            } else {
                ret.actual_length = cmd.transfer_buffer_length;
            }
            Ok(ret)
        }
        Err(TransferError::Timeout) => {
            ret.status = ETIMEDOUT;
            Ok(ret)
        }
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// (bmRequestType, bRequest, wValue, wIndex, wLength) from the setup packet (LE).
fn control_setup(cmd: &UsbipCmdSubmit) -> (u8, u8, u16, u16, u16) {
    (
        cmd.setup[0],
        cmd.setup[1],
        u16::from_le_bytes([cmd.setup[2], cmd.setup[3]]),
        u16::from_le_bytes([cmd.setup[4], cmd.setup[5]]),
        u16::from_le_bytes([cmd.setup[6], cmd.setup[7]]),
    )
}

/// Build the Silex request frame for a (non-iso) URB WITHOUT sending it.
///
/// For the multiplex worker (control + interrupt on one connection): EP0 -> control,
/// interrupt EP -> interrupt request (long-poll), otherwise bulk.
pub fn build_request(session: &SilexSession, cmd: &UsbipCmdSubmit, ep_type: Option<u8>, mps: u16, interval: u16) -> SilexFrame {
    let out_data = if cmd.is_in() { Vec::new() } else { cmd.transfer_buffer.clone() };
    if cmd.ep == 0 {
        let (bmrt, breq, wv, wi, wl) = control_setup(cmd);
        return session.build_control_frame(bmrt, breq, wv, wi, wl, out_data);
    }
    if ep_type == Some(EP_INTERRUPT) {
        let length = if cmd.transfer_buffer_length != 0 { cmd.transfer_buffer_length } else { mps as u32 };
        return session.build_interrupt_frame(cmd.ep as u8, cmd.is_in(), length, 0x8000 | (mps & 0x7FFF), interval, out_data);
    }
    session.build_bulk_frame(cmd.ep as u8, cmd.is_in(), cmd.transfer_buffer_length, out_data)
}

/// Build the RET_SUBMIT from an already-received (non-iso) Silex response. Shared for
/// control/bulk/interrupt: IN -> data + actual_length, OUT -> sent length.
pub fn fill_ret_from_response(cmd: &UsbipCmdSubmit, resp: &SilexFrame) -> UsbipRetSubmit {
    let mut ret = ret_for(cmd);
    ret.status = silex_status_to_errno(resp.status1);
    if cmd.is_in() {
        ret.actual_length = resp.data.len() as u32;
        ret.data = resp.data.clone();
    } else {
        ret.actual_length = cmd.transfer_buffer_length;
    }
    ret
}

/// Silex iso-IN request parameters from an IN URB (the whole URB = ONE request; the
/// device accepts the full packet count on the interface's own connection - NO chunking).
/// Returns (endpoint, packets[(offset, length)], start_frame, maxpacket, total_length).
pub fn iso_request_params(session: &SilexSession, cmd: &UsbipCmdSubmit) -> IsoInReq {
    let mut local = Vec::with_capacity(cmd.iso_packets.len());
    let mut loff = 0u32;
    for (_o, length, _a, _s) in &cmd.iso_packets {
        local.push((loff, *length)); // local, packed offsets (as in the capture)
        loff += *length;
    }
    let maxlen = local.iter().map(|(_o, l)| *l).max().unwrap_or(0);
    let maxpacket = 0x8000 | (maxlen as u16 & 0x7FFF);
    // total_length = npkts * (wMaxPacketSize + 12): the device reserves payload + a
    // 12-byte descriptor per packet.
    let total_length = local.len() as u32 * (maxlen + 12);
    // Linux sets start_frame=0 on URB_ISO_ASAP; the device needs a valid, monotonically
    // rising frame number (1 ms units) -> ~1 frame per 8 packets.
    let frames = ((local.len() + 7) / 8).max(1) as u32;
    let start_frame = if cmd.start_frame != 0 { cmd.start_frame } else { session.alloc_iso_frames(frames) };
    (cmd.ep as u8, local, start_frame, maxpacket, total_length)
}

/// Build the RET_SUBMIT from a Silex iso response.
///
/// The device (and we) deliver audio PACKED (packet i right after i-1). USB/IP expects
/// exactly that: packed data + descriptors with the ORIGINAL submit offsets; the vhci
/// client unpacks them to those offsets via usbip_pad_iso. So do NOT return the packed
/// offsets, but the submit offsets from the URB.
pub fn iso_fill_ret(cmd: &UsbipCmdSubmit, mut ret: UsbipRetSubmit, status1: u16, resp_packets: &[(u32, u32, u32)], data: Vec<u8>) -> UsbipRetSubmit {
    ret.status = silex_status_to_errno(status1);
    ret.actual_length = data.len() as u32; // packed (sum of actual_length)
    ret.data = data;
    ret.start_frame = cmd.start_frame;
    let mut out_packets = Vec::with_capacity(cmd.iso_packets.len().min(resp_packets.len()));
    for ((sub_off, sub_len, _a, _s), (_ro, alen, st)) in cmd.iso_packets.iter().zip(resp_packets.iter()) {
        out_packets.push((*sub_off, *sub_len, *alen, silex_status_to_errno(*st as u16) as u32));
    }
    ret.number_of_packets = out_packets.len() as u32;
    ret.iso_packets = out_packets;
    ret
}

/// Silex iso-OUT parameters (playback) from an OUT URB. Pulls the audio PACKED from the
/// transfer_buffer (each URB packet its slice) and builds packed descriptors. `nch` is set
/// ONLY when the endpoint belongs to an Audio/AudioStreaming interface with a 16-bit
/// format (then lossless compression is allowed); otherwise (video, other iso) it runs raw
/// 1:1. Returns (endpoint, packets, audio, start_frame, maxpacket, nch|None).
pub fn iso_out_request_params(session: &SilexSession, cmd: &UsbipCmdSubmit) -> (u8, Vec<(u32, u32)>, Vec<u8>, u32, u16, Option<u8>) {
    let mut packets = Vec::with_capacity(cmd.iso_packets.len());
    let mut audio = Vec::new();
    let mut loff = 0u32;
    for (off, length, _a, _s) in &cmd.iso_packets {
        packets.push((loff, *length));
        let start = *off as usize;
        let end = (start + *length as usize).min(cmd.transfer_buffer.len());
        if start < end {
            audio.extend_from_slice(&cmd.transfer_buffer[start..end]);
        }
        loff += *length;
    }
    let maxlen = packets.iter().map(|(_o, l)| *l).max().unwrap_or(0);
    let (maxpacket, use_nch) = match session.iso_out_info(cmd.ep as u8) {
        Some((mps, nch, subframe)) => {
            let use_nch = if nch != 0 && subframe > 0 && subframe <= 2 { Some(nch) } else { None };
            (0x8000 | (mps & 0x7FFF), use_nch) // only 16-bit audio may be compressed
        }
        None => (0x8000 | (maxlen as u16 & 0x7FFF), None), // unknown -> raw, from URB
    };
    let frames = ((packets.len() + 7) / 8).max(1) as u32;
    let start_frame = if cmd.start_frame != 0 { cmd.start_frame } else { session.alloc_iso_frames(frames) };
    (cmd.ep as u8, packets, audio, start_frame, maxpacket, use_nch)
}

/// RET_SUBMIT for OUT iso: the response is a bare 12-byte ACK (no data). All packets count
/// as sent (actual_length = requested length).
pub fn iso_fill_ret_out(cmd: &UsbipCmdSubmit, mut ret: UsbipRetSubmit, status1: u16) -> UsbipRetSubmit {
    let err = silex_status_to_errno(status1);
    ret.status = err;
    ret.data = Vec::new();
    ret.start_frame = cmd.start_frame;
    let total: u32 = cmd.iso_packets.iter().map(|(_o, l, _a, _s)| *l).sum();
    ret.actual_length = if err == 0 { total } else { 0 };
    ret.iso_packets = cmd
        .iso_packets
        .iter()
        .map(|(off, length, _a, _s)| (*off, *length, if err == 0 { *length } else { 0 }, err as u32))
        .collect();
    ret.number_of_packets = ret.iso_packets.len() as u32;
    ret
}

/// Synchronous iso transfer (fallback / non-batch path).
async fn submit_iso(session: &SilexSession, cmd: &UsbipCmdSubmit, ret: UsbipRetSubmit) -> Result<UsbipRetSubmit> {
    if !cmd.is_in() {
        let (ep, packets, audio, sf, mp, nch) = iso_out_request_params(session, cmd);
        if let Err(e) = session.iso_send_out(ep, packets, audio, sf, mp, nch).await {
            return iso_error(cmd, ret, e);
        }
        return match session.iso_recv().await {
            Ok((status1, _packets, _data)) => Ok(iso_fill_ret_out(cmd, ret, status1)),
            Err(e) => iso_error(cmd, ret, e),
        };
    }
    let params = iso_request_params(session, cmd);
    match session.iso_batch(vec![params]).await {
        Ok(mut results) => {
            let (status1, resp_packets, data) = results.remove(0);
            Ok(iso_fill_ret(cmd, ret, status1, &resp_packets, data))
        }
        Err(e) => iso_error(cmd, ret, e),
    }
}

/// Map a tunnel error on an iso transfer: timeout fails only this URB (ETIMEDOUT), any
/// other error kills the session.
fn iso_error(cmd: &UsbipCmdSubmit, mut ret: UsbipRetSubmit, e: TransferError) -> Result<UsbipRetSubmit> {
    match e {
        TransferError::Timeout => {
            ret.number_of_packets = cmd.number_of_packets;
            ret.start_frame = cmd.start_frame;
            ret.status = ETIMEDOUT;
            Ok(ret)
        }
        other => Err(anyhow::anyhow!("{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso_cmd(is_in: bool, packets: &[(u32, u32, u32, u32)], buffer: Vec<u8>) -> UsbipCmdSubmit {
        UsbipCmdSubmit {
            seqnum: 1,
            devid: 0,
            direction: if is_in { crate::usbip::proto::USBIP_DIR_IN } else { crate::usbip::proto::USBIP_DIR_OUT },
            ep: 1,
            transfer_flags: 0,
            transfer_buffer_length: buffer.len() as u32,
            start_frame: 0,
            number_of_packets: packets.len() as u32,
            interval: 1,
            setup: [0u8; 8],
            transfer_buffer: buffer,
            iso_packets: packets.to_vec(),
        }
    }

    #[test]
    fn status_mapping() {
        assert_eq!(silex_status_to_errno(0x0000), 0);
        assert_eq!(silex_status_to_errno(0xFFFD), EPIPE);
        assert_eq!(silex_status_to_errno(0x1234), EPROTO);
    }

    #[test]
    fn iso_fill_ret_uses_submit_offsets() {
        // Submit offsets 0 and 500; device replies with packed offsets 0 and 190.
        let cmd = iso_cmd(true, &[(0, 200, 0, 0), (500, 200, 0, 0)], Vec::new());
        let resp = [(0u32, 190u32, 0u32), (190, 188, 0)];
        let ret = iso_fill_ret(&cmd, ret_for(&cmd), 0, &resp, vec![0u8; 378]);
        assert_eq!(ret.number_of_packets, 2);
        // returned descriptors keep the SUBMIT offsets, with the device's actual_length
        assert_eq!(ret.iso_packets[0], (0, 200, 190, 0));
        assert_eq!(ret.iso_packets[1], (500, 200, 188, 0));
        assert_eq!(ret.actual_length, 378);
    }

    #[test]
    fn iso_fill_ret_out_marks_all_sent() {
        let cmd = iso_cmd(false, &[(0, 100, 0, 0), (100, 120, 0, 0)], vec![0u8; 220]);
        let ret = iso_fill_ret_out(&cmd, ret_for(&cmd), 0);
        assert_eq!(ret.actual_length, 220);
        assert_eq!(ret.iso_packets, vec![(0, 100, 100, 0), (100, 120, 120, 0)]);
        assert!(ret.data.is_empty());
    }
}
