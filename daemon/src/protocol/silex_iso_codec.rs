//! Silex iso-OUT audio codec (lossless compression, opcode 0x8004).
//!
//! The Windows driver compresses isochronous OUT audio (host -> device, playback) before
//! tunneling it over TCP. The device decompresses it and writes raw PCM to the physical
//! USB endpoint. Reconstructed from a playback capture: two lossless stages (DPCM then
//! entropy packing), byte-exact against the capture.
//!
//! Two-stage, LOSSLESS scheme for 16-bit PCM:
//!
//! 1. **Per-channel DPCM** ([`dpcm_encode`]): the first `nch` samples stay raw, then each
//!    channel stores the difference to the previous sample of the same channel.
//! 2. **Entropy bit-packing** ([`entropy_encode`]): a variable prefix code over the high
//!    byte of each 16-bit difference; the low byte (and, for large differences, the high
//!    byte too) goes into a separate literal stream.
//!
//! | high byte | code   | literals          |
//! |-----------|--------|-------------------|
//! | 0x00      | `00`   | low byte          |
//! | 0xFF      | `01`   | low byte          |
//! | 0x01      | `100`  | low byte          |
//! | 0xFE      | `101`  | low byte          |
//! | 0x02      | `1100` | low byte          |
//! | 0xFD      | `1101` | low byte          |
//! | else      | `111`  | low byte + high   |
//!
//! Frame payload (after the 24-byte iso header, field +18 = 0x8000):
//! `+0 u8 nch`, `+1 u8 0x01`, `+2 u16 code_len (BE)`, then `code_len` code bytes
//! (MSB-first), then the literal bytes. The entropy input is
//! `[npkts*12 descriptors][DPCM audio]` — on compression the descriptors are folded into
//! the stream; in the raw frame they sit uncompressed behind the header instead.

/// High byte of the 16-bit difference -> (code word in a 16-bit register MSB-first, code
/// length in bits). Values reconstructed from the capture; the fallback is the escape code.
fn enc_code(high: u8) -> (u16, u32) {
    match high {
        0x00 => (0x0000, 2),
        0xFF => (0x4000, 2),
        0x01 => (0x8000, 3),
        0xFE => (0xA000, 3),
        0x02 => (0xC000, 4),
        0xFD => (0xD000, 4),
        _ => (0xE000, 3), // escape
    }
}

fn is_known(high: u8) -> bool {
    matches!(high, 0x00 | 0xFF | 0x01 | 0xFE | 0x02 | 0xFD)
}

#[cfg(test)]
fn getbit(codes: &[u8], bitpos: &mut usize) -> u8 {
    let b = (codes[*bitpos >> 3] >> (7 - (*bitpos & 7))) & 1;
    *bitpos += 1;
    b
}

// --------------------------------------------------------------------------- //
//  DPCM (per-channel differences)
// --------------------------------------------------------------------------- //
/// 16-bit PCM (little-endian) -> per-channel differences. Length is unchanged.
pub fn dpcm_encode(raw: &[u8], nch: usize) -> Vec<u8> {
    let n = raw.len() / 2;
    if nch == 0 {
        return raw[..n * 2].to_vec();
    }
    let vals: Vec<i16> = (0..n).map(|i| i16::from_le_bytes([raw[2 * i], raw[2 * i + 1]])).collect();
    let mut out = vec![0i16; n];
    let mut hist = vec![0i16; nch];
    let m = nch.min(n);
    for i in 0..m {
        out[i] = vals[i];
        hist[i] = vals[i];
    }
    for i in nch..n {
        let ch = i % nch;
        out[i] = vals[i].wrapping_sub(hist[ch]); // 16-bit wrap, like the driver
        hist[ch] = vals[i];
    }
    pack_i16(&out)
}

/// Inverse of [`dpcm_encode`].
#[cfg(test)]
pub fn dpcm_decode(deltas: &[u8], nch: usize) -> Vec<u8> {
    let n = deltas.len() / 2;
    if nch == 0 {
        return deltas[..n * 2].to_vec();
    }
    let vals: Vec<i16> = (0..n).map(|i| i16::from_le_bytes([deltas[2 * i], deltas[2 * i + 1]])).collect();
    let mut out = vec![0i16; n];
    let mut hist = vec![0i16; nch];
    let m = nch.min(n);
    for i in 0..m {
        out[i] = vals[i];
        hist[i] = vals[i];
    }
    for i in nch..n {
        let ch = i % nch;
        let s = hist[ch].wrapping_add(vals[i]);
        out[i] = s;
        hist[ch] = s;
    }
    pack_i16(&out)
}

fn pack_i16(vals: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 2);
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

// --------------------------------------------------------------------------- //
//  Entropy bit-packing
// --------------------------------------------------------------------------- //
/// Byte stream (even length) -> (code stream, literal stream).
pub fn entropy_encode(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut codes = Vec::new();
    let mut lits = Vec::new();
    let mut acc: u32 = 0; // 16-bit accumulator, fills MSB-first
    let mut nbits: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        let low = data[i];
        let high = data[i + 1];
        lits.push(low);
        let (code, clen) = enc_code(high);
        if !is_known(high) {
            lits.push(high);
        }
        acc = (acc | ((code as u32) >> nbits)) & 0xFFFF;
        nbits += clen;
        if nbits >= 8 {
            codes.push(((acc >> 8) & 0xFF) as u8);
            nbits -= 8;
            acc = (acc << 8) & 0xFFFF;
        }
        i += 2;
    }
    if nbits != 0 {
        codes.push(((acc >> 8) & 0xFF) as u8);
    }
    (codes, lits)
}

/// Inverse of [`entropy_encode`]; yields `nunits * 2` bytes.
#[cfg(test)]
pub fn entropy_decode(codes: &[u8], lits: &[u8], nunits: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(nunits * 2);
    let mut bitpos = 0usize;
    let mut lit = 0usize;
    for _ in 0..nunits {
        let low: u8;
        let high: u8;
        if getbit(codes, &mut bitpos) == 0 {
            high = if getbit(codes, &mut bitpos) == 0 { 0x00 } else { 0xFF };
            low = lits[lit];
            lit += 1;
        } else if getbit(codes, &mut bitpos) == 0 {
            // 10x
            high = if getbit(codes, &mut bitpos) == 0 { 0x01 } else { 0xFE };
            low = lits[lit];
            lit += 1;
        } else if getbit(codes, &mut bitpos) == 0 {
            // 110x
            high = if getbit(codes, &mut bitpos) == 0 { 0x02 } else { 0xFD };
            low = lits[lit];
            lit += 1;
        } else {
            // 111 escape
            low = lits[lit];
            lit += 1;
            high = lits[lit];
            lit += 1;
        }
        out.push(low);
        out.push(high);
    }
    out
}

// --------------------------------------------------------------------------- //
//  Frame payload
// --------------------------------------------------------------------------- //
/// Build the compressed iso-OUT payload (sub-header + codes + literals).
///
/// `desc_bytes` = `npkts*12` raw iso descriptors, `audio` = concatenated 16-bit PCM,
/// `nch` = channel count. Returns `None` when compression does NOT get smaller (then send
/// raw - exactly the driver's fallback decision: compress only when it saves bytes).
pub fn compress_iso_out(desc_bytes: &[u8], audio: &[u8], nch: u8) -> Option<Vec<u8>> {
    let mut stream = desc_bytes.to_vec();
    stream.extend_from_slice(&dpcm_encode(audio, nch as usize));
    let (codes, lits) = entropy_encode(&stream);
    let mut payload = Vec::with_capacity(4 + codes.len() + lits.len());
    payload.push(nch);
    payload.push(0x01);
    payload.extend_from_slice(&(codes.len() as u16).to_be_bytes());
    payload.extend_from_slice(&codes);
    payload.extend_from_slice(&lits);
    if payload.len() < desc_bytes.len() + audio.len() {
        Some(payload)
    } else {
        None
    }
}

/// Inverse of [`compress_iso_out`]. Returns (descriptor bytes, audio). Needs `npkts` (from
/// the header) to split the leading descriptors from the audio; the audio length follows
/// from the descriptor length fields in the decompressed stream.
#[cfg(test)]
pub fn decompress_iso_out(payload: &[u8], npkts: usize) -> (Vec<u8>, Vec<u8>) {
    let nch = payload[0];
    let code_len = u16::from_be_bytes([payload[2], payload[3]]) as usize;
    let codes = &payload[4..4 + code_len];
    let lits = &payload[4 + code_len..];
    // Decode the descriptors first (npkts*12 B = npkts*6 units), then determine the audio
    // length from their length fields.
    let desc = entropy_decode(codes, lits, npkts * 6);
    let mut buflen: u32 = 0;
    for k in 0..npkts {
        buflen += u32::from_be_bytes([desc[k * 12 + 4], desc[k * 12 + 5], desc[k * 12 + 6], desc[k * 12 + 7]]);
    }
    let full = entropy_decode(codes, lits, npkts * 6 + (buflen as usize) / 2);
    let desc_bytes = full[..npkts * 12].to_vec();
    let audio = dpcm_decode(&full[npkts * 12..], nch as usize);
    (desc_bytes, audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_audio(nch: usize, frames: usize) -> Vec<u8> {
        // Slowly varying, highly correlated -> small per-channel deltas (compresses well).
        let mut out = Vec::new();
        for i in 0..frames {
            for c in 0..nch {
                let v = (i as i16).wrapping_mul(3).wrapping_add(c as i16 * 7);
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn dpcm_is_lossless() {
        for nch in [1usize, 2, 4] {
            let audio = sample_audio(nch, 64);
            let back = dpcm_decode(&dpcm_encode(&audio, nch), nch);
            assert_eq!(back, audio);
        }
    }

    #[test]
    fn entropy_is_lossless() {
        let audio = sample_audio(2, 100);
        let (codes, lits) = entropy_encode(&audio);
        assert_eq!(entropy_decode(&codes, &lits, audio.len() / 2), audio);
    }

    #[test]
    fn compress_roundtrip_and_smaller() {
        let nch = 2u8;
        let audio = sample_audio(nch as usize, 256);
        let mut desc = Vec::new();
        desc.extend_from_slice(&0u32.to_be_bytes()); // offset
        desc.extend_from_slice(&(audio.len() as u32).to_be_bytes()); // length
        desc.extend_from_slice(&0u32.to_be_bytes()); // status
        let payload = compress_iso_out(&desc, &audio, nch).expect("correlated audio compresses");
        assert!(payload.len() < desc.len() + audio.len());
        assert_eq!(payload[0], nch);
        assert_eq!(payload[1], 0x01);
        let (d2, a2) = decompress_iso_out(&payload, 1);
        assert_eq!(d2, desc);
        assert_eq!(a2, audio);
    }

    #[test]
    fn incompressible_returns_none() {
        // Two packets of tiny random-ish data: overhead exceeds any savings.
        let audio = vec![0x12u8, 0x80, 0x34, 0x7F];
        let mut desc = Vec::new();
        desc.extend_from_slice(&0u32.to_be_bytes());
        desc.extend_from_slice(&4u32.to_be_bytes());
        desc.extend_from_slice(&0u32.to_be_bytes());
        assert!(compress_iso_out(&desc, &audio, 2).is_none());
    }
}
