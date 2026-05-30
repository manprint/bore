//! Control-channel endpoint parsing and connection.
//!
//! The client's `--to` value selects where and how to reach the server's control
//! port. This module turns it into a concrete connection.

use anyhow::Result;
use tokio::net::TcpStream;

use crate::client::connect_with_timeout;
use crate::shared::CONTROL_PORT;

/// A parsed control endpoint derived from a `--to` value.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Host to connect to.
    pub host: String,
    /// Control port to connect to.
    pub port: u16,
}

impl Endpoint {
    /// Parse a `--to` value of the form `host` or `host:port`.
    ///
    /// Without an explicit port the default control port ([`CONTROL_PORT`]) is
    /// used, preserving the historical behaviour.
    pub fn parse(to: &str) -> Self {
        if let Some((host, port)) = to.rsplit_once(':') {
            if !host.is_empty() {
                if let Ok(port) = port.parse::<u16>() {
                    return Endpoint {
                        host: host.to_string(),
                        port,
                    };
                }
            }
        }
        Endpoint {
            host: to.to_string(),
            port: CONTROL_PORT,
        }
    }
}

/// Open a control connection to the endpoint.
pub async fn connect(endpoint: &Endpoint) -> Result<TcpStream> {
    connect_with_timeout(&endpoint.host, endpoint.port).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_only_uses_default_port() {
        let endpoint = Endpoint::parse("bore.tld");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, CONTROL_PORT);
    }

    #[test]
    fn parse_host_port() {
        let endpoint = Endpoint::parse("bore.tld:1000");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, 1000);
    }

    #[test]
    fn parse_non_numeric_port_is_treated_as_host() {
        let endpoint = Endpoint::parse("bore.tld:nope");
        assert_eq!(endpoint.host, "bore.tld:nope");
        assert_eq!(endpoint.port, CONTROL_PORT);
    }
}
