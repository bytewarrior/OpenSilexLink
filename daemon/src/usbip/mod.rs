//! USB/IP server side: expose the tunnelled Silex devices to the local kernel.
//!
//! * [`proto`] — USB/IP protocol structs (submit/unlink, op import).
//! * [`session`] — the Silex TCP tunnel to the device server (dev index, keepalive).
//! * [`device`] — composite-device router: one Silex session per sub-interface.
//! * [`translate`] — URB <-> Silex transfer translation and status mapping.
//! * [`server`] — USB/IP server bound on 127.0.0.1:3240 for `usbip attach`.

pub mod device;
pub mod proto;
pub mod server;
pub mod session;
pub mod translate;
