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

mod adaptive_nat;
pub mod admin;
pub mod admin_api;
pub mod admin_http;
pub mod admin_views;
pub mod auth;
pub mod basicauth;
pub mod certinfo;
pub mod client;
pub mod edge;
pub mod holepunch;
pub mod mux;
pub mod pool;
#[cfg(feature = "udp")]
pub mod portmap;
pub mod prefixed;
pub mod reconnect;
pub mod secret;
pub mod server;
pub mod shared;
pub mod ssh_jump;
#[cfg(feature = "ssh-gateway")]
pub mod sshgw;
#[cfg(feature = "ssh-gateway")]
pub mod sshgw_auth;
pub mod transfer;
pub mod transport;
pub mod udp_diagnostic;
pub mod vhost;
#[cfg(all(
    feature = "vpn",
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "android"
    )
))]
pub mod vpn;
#[cfg(feature = "vpn")]
pub mod vpn_server;
pub mod weblog;
