//! Small big-endian read helpers for the wire codecs.
//!
//! Parsers must bounds-check with [`need`] before calling the `rd_*` helpers; the
//! helpers themselves index directly and would panic on a short buffer.

use anyhow::{bail, Result};

/// Ensure `buf` has at least `n` bytes, else a parse error mentioning `what`.
pub fn need(buf: &[u8], n: usize, what: &str) -> Result<()> {
    if buf.len() < n {
        bail!("{what}: buffer too short ({} < {n})", buf.len());
    }
    Ok(())
}

#[inline]
pub fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

#[inline]
pub fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Read a NUL-terminated ASCII string starting at `off`; returns (text, offset_after_nul).
/// If no NUL is found, reads to the end of the buffer.
pub fn read_cstr(buf: &[u8], off: usize) -> (String, usize) {
    if off >= buf.len() {
        return (String::new(), off + 1);
    }
    let end = buf[off..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| off + p)
        .unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[off..end]).into_owned();
    (s, end + 1)
}

/// Read a fixed-width field and cut at the first NUL (like a C char array).
pub fn cstr_field(buf: &[u8], off: usize, width: Option<usize>) -> String {
    let end = match width {
        Some(w) => (off + w).min(buf.len()),
        None => buf.len(),
    };
    if off >= end {
        return String::new();
    }
    let chunk = &buf[off..end];
    let cut = chunk.iter().position(|&b| b == 0).unwrap_or(chunk.len());
    String::from_utf8_lossy(&chunk[..cut]).into_owned()
}
