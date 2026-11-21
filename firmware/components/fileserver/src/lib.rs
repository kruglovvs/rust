//! Read-only HTTP file and status server for the SD card, on the ESP-IDF
//! HTTP server.
//!
//! The crate owns everything between the router and the card: path mapping,
//! `Range` handling, the JSON bodies, the file stream and the heap guard. The
//! rest of the firmware only calls [`start`] with a [`Config`].
//!
//! Every decision is a pure function over borrowed data so the device test
//! binary can exercise it without a socket or a real card; only [`status`] and
//! [`server`] touch device APIs, and [`plan`] is what the `EspHttpServer`
//! handlers execute.

pub mod body;
pub mod json;
pub mod path;
pub mod plan;
pub mod range;
pub mod router;
pub mod server;
pub mod status;

pub use server::{start, Config, Server};
