//! Wi-Fi bring-up, mDNS discovery, SNTP, and the pure session lifecycle
//! policy. `net` holds the device glue; `session` is the host-testable state
//! machine it feeds.

pub mod net;
pub mod session;

pub use net::*;
