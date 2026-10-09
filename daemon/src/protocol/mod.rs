//! Silex wire-protocol codecs (pure, socket-free — unit-testable against captured bytes).
//!
//! * [`silex_frame`] — TCP URB frame codec (raw USB request blocks over TCP :19540).
//! * [`silex_iso_codec`] — lossless companding codec for isochronous OUT audio (opcode 0x8004).
//! * [`udp_discovery`] — UDP ASCII discovery/control commands and the device-list parser.
//! * [`peer`] — host-to-host "request use" negotiation on UDP :49300.

pub mod peer;
pub mod silex_frame;
pub mod silex_iso_codec;
pub mod udp_discovery;
