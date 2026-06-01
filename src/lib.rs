//! A modern, simple TCP tunnel in Rust that exposes local ports to a remote
//! server, bypassing standard NAT connection firewalls.
//!
//! This is the library crate documentation. If you're looking for usage
//! information about the binary, see the command below.
//!
//! ```shell
//! $ bore help
//! ```
//!
//! There are two components to the crate, offering implementations of the
//! server network daemon and client local forwarding proxy. Both are public
//! members and can be run programmatically with a Tokio 1.0 runtime.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admin;
pub mod admin_http;
pub mod auth;
pub mod basicauth;
pub mod client;
pub mod edge;
pub mod holepunch;
pub mod mux;
pub mod prefixed;
pub mod reconnect;
pub mod secret;
pub mod server;
pub mod shared;
pub mod transport;
