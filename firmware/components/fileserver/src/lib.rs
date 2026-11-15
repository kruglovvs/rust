//! Read-only HTTP/1.1 file and status server for the SD card.
//!
//! The crate owns everything between the listen socket and the card: request
//! parsing, path mapping, `Range` handling, routing, the JSON bodies, the file
//! stream and the connection model. The rest of the firmware only calls
//! [`start`] with a [`Config`].
//!
//! Every decision is a pure function over borrowed data so the device test
//! binary can exercise it without a socket or a real card; only [`status`] and
//! [`server`] touch device APIs.

pub mod body;
pub mod http;
pub mod json;
pub mod path;
pub mod range;
pub mod router;
pub mod server;
pub mod status;

pub use server::{start, Config, Server};
