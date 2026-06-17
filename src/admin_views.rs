//! Serializable view structs for admin dashboard API sections.
//!
//! Each section of the dashboard has a corresponding view struct that snapshots
//! the server's live state into owned, serializable data. These are built
//! synchronously (mirroring [`crate::admin::AdminRegistry::snapshot`]) with no
//! DashMap guards held across `.await` boundaries (invariant I-7).

use serde::Serialize;

/// Summary section: version, control port, feature flags, server uptime, per-section counts.
#[derive(Serialize, Clone)]
pub struct SummaryView {
    /// Server version string (semver - branch - sha8).
    pub version: String,
    /// Control port the server listens on.
    pub control_port: u16,
    /// Whether control connection is TLS.
    pub tls: bool,
    /// Whether UDP direct-path brokering is enabled.
    pub udp: bool,
    /// Whether VPN brokering is enabled.
    pub vpn_enabled: bool,
    /// Whether vhost reverse-proxy is enabled.
    pub vhost_enabled: bool,
    /// Seconds since server started.
    pub uptime_secs: u64,
    /// Number of live public tunnels.
    pub live_tunnels: usize,
    /// Number of live vhost providers.
    pub live_vhost: usize,
    /// Number of live VPN links (cfg vpn).
    #[cfg(feature = "vpn")]
    pub live_vpn_links: usize,
}

/// Public tunnel entry (role=Public).
#[derive(Serialize, Clone)]
pub struct TunnelView {
    /// Stable id.
    pub id: u64,
    /// Remote peer address (string).
    pub peer: String,
    /// Allocated public port.
    pub public_port: Option<u16>,
    /// Operator notes.
    pub notes: Option<String>,
    /// HTTP Basic auth enforced.
    pub basic_auth: bool,
    /// TLS termination.
    pub https: bool,
    /// Force HTTP→HTTPS redirect.
    pub force_https: bool,
    /// UDP direct-path enabled.
    pub udp: bool,
    /// VPN overlay address (if applicable).
    pub overlay: Option<String>,
    /// Direct QUIC path active.
    pub vpn_direct: bool,
    /// Active proxied connections.
    pub active: usize,
    /// Seconds since connection registered.
    pub uptime_secs: u64,
    /// Relay tx bytes.
    pub relay_tx_bytes: u64,
    /// Relay rx bytes.
    pub relay_rx_bytes: u64,
}

/// Secret tunnel entry (role=SecretProvider or SecretConsumer).
#[derive(Serialize, Clone)]
pub struct SecretView {
    /// Stable id.
    pub id: u64,
    /// SecretProvider or SecretConsumer.
    pub role: String,
    /// Remote peer address (string).
    pub peer: String,
    /// Secret tunnel id.
    pub secret_id: Option<String>,
    /// HTTP Basic auth enforced.
    pub basic_auth: bool,
    /// UDP direct-path enabled.
    pub udp: bool,
    /// Active proxied connections.
    pub active: usize,
    /// Seconds since connection registered.
    pub uptime_secs: u64,
    /// Relay tx bytes.
    pub relay_tx_bytes: u64,
    /// Relay rx bytes.
    pub relay_rx_bytes: u64,
}

/// Vhost subdomain provider.
#[derive(Serialize, Clone)]
pub struct VhostView {
    /// Subdomain label.
    pub subdomain: String,
    /// Active proxied connections.
    pub active: usize,
    /// Number of parallel carrier TCP streams.
    pub carriers: u16,
    /// Count of direct QUIC stream opens (QUIC-only).
    pub direct_stream_opens: u64,
    /// Injected request-header names (sanitized, no sensitive values).
    pub request_headers: Vec<String>,
    /// Injected response-header names (sanitized, no sensitive values).
    pub response_headers: Vec<String>,
    /// TLS termination.
    pub tls: bool,
}

/// VPN link (listener or connector).
#[cfg(feature = "vpn")]
#[derive(Serialize, Clone)]
pub struct VpnLinkView {
    /// Stable link id.
    pub id: u64,
    /// VpnListener or VpnConnector.
    pub role: String,
    /// Real peer address.
    pub peer: String,
    /// Overlay CIDR (e.g., "10.99.0.1/32").
    pub overlay: Option<String>,
    /// Advertised routes (as strings).
    pub advertised: Vec<String>,
    /// Number of parallel carrier QUIC connections.
    pub carriers: u16,
    /// Direct QUIC path active.
    pub direct: bool,
    /// Relay path active.
    pub relay: bool,
    /// Relay tx bytes.
    pub relay_tx_bytes: u64,
    /// Relay rx bytes.
    pub relay_rx_bytes: u64,
    /// Hub peers (if this is a hub listener).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_peers: Option<Vec<VpnPeerView>>,
}

/// VPN hub peer (member of a hub listener).
#[cfg(feature = "vpn")]
#[derive(Serialize, Clone)]
pub struct VpnPeerView {
    /// Monotonic peer id within the hub.
    pub peer_id: u32,
    /// Peer's overlay address.
    pub overlay: String,
    /// Real peer address.
    pub peer: String,
    /// Advertised routes (as strings).
    pub advertised: Vec<String>,
}

/// TLS certificate expiry.
#[derive(Serialize, Clone)]
pub struct CertView {
    /// Label (e.g., "control", "vhost:example.com").
    pub label: String,
    /// Path to cert file.
    pub path: Option<String>,
    /// X.509 subject CN.
    pub subject: Option<String>,
    /// Subject Alt Names.
    pub sans: Vec<String>,
    /// not_before (RFC3339).
    pub not_before: Option<String>,
    /// not_after (RFC3339).
    pub not_after: Option<String>,
    /// Signed integer days remaining (negative = expired).
    pub days_remaining: i64,
    /// True if days_remaining <= 30.
    pub expiring: bool,
    /// Error message if parsing failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Server startup configuration (sanitized, D11).
#[derive(Serialize, Clone)]
pub struct ConfigView {
    /// Port range forwarded.
    pub port_range: String,
    /// Control port.
    pub control_port: u16,
    /// Max concurrent proxied connections per client.
    pub max_conns: u32,
    /// Max parallel carriers per tunnel.
    pub max_carriers: u16,
    /// Bind address for control/tunnels.
    pub bind_addr: String,
    /// Bind address for tunnel listeners.
    pub bind_tunnels: String,
    /// UDP direct path enabled.
    pub udp: bool,
    /// UDP socket buffer tuning (informational).
    pub udp_socket_send_buffer: Option<usize>,
    /// UDP socket receive buffer size.
    pub udp_socket_recv_buffer: Option<usize>,
    /// VPN enabled.
    #[cfg(feature = "vpn")]
    pub vpn_enabled: bool,
    /// VPN overlay pool CIDR.
    #[cfg(feature = "vpn")]
    pub vpn_pool: Option<String>,
    /// Max concurrent VPN links.
    #[cfg(feature = "vpn")]
    pub vpn_max_links: u32,
    /// Overlay subnet prefix per hub.
    #[cfg(feature = "vpn")]
    pub vpn_hub_prefix: u8,
    /// Vhost reverse-proxy enabled.
    pub vhost_enabled: bool,
    /// Vhost base domain.
    pub vhost_base_domain: Option<String>,
    /// Vhost HTTP port.
    pub vhost_http_port: Option<u16>,
    /// Vhost HTTPS port.
    pub vhost_https_port: Option<u16>,
    /// TLS enabled on control port.
    pub tls: bool,
}

/// Server metrics: uptime, memory, bandwidth, live counts.
#[derive(Serialize, Clone)]
pub struct MetricsView {
    /// Seconds since server started.
    pub uptime_secs: u64,
    /// Process RSS in bytes (Option: Linux only, None on other platforms).
    pub mem_rss_bytes: Option<u64>,
    /// Cumulative bytes sent (over all tunnels, all time).
    pub bandwidth_tx_bytes: u64,
    /// Cumulative bytes received (over all tunnels, all time).
    pub bandwidth_rx_bytes: u64,
    /// Number of live public tunnels.
    pub live_tunnels: usize,
    /// Number of live vhost providers.
    pub live_vhost: usize,
    /// Number of live VPN links (cfg vpn).
    #[cfg(feature = "vpn")]
    pub live_vpn_links: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_views_serialize_stable() {
        // Test that each struct serializes with expected snake_case keys.
        let summary = SummaryView {
            version: "1.0.0 - main - abc1234".into(),
            control_port: 7835,
            tls: true,
            udp: false,
            vpn_enabled: false,
            vhost_enabled: true,
            uptime_secs: 42,
            live_tunnels: 1,
            live_vhost: 2,
            #[cfg(feature = "vpn")]
            live_vpn_links: 0,
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert!(json["version"].is_string());
        assert!(json["control_port"].is_number());
        assert!(json["tls"].is_boolean());
        assert!(json["uptime_secs"].is_number());
        assert!(json["live_tunnels"].is_number());

        let tunnel = TunnelView {
            id: 1,
            peer: "10.0.0.1:54321".into(),
            public_port: Some(9000),
            notes: Some("test".into()),
            basic_auth: false,
            https: true,
            force_https: false,
            udp: false,
            overlay: None,
            vpn_direct: false,
            active: 0,
            uptime_secs: 10,
            relay_tx_bytes: 1024,
            relay_rx_bytes: 2048,
        };
        let json = serde_json::to_value(&tunnel).unwrap();
        assert!(json["public_port"].is_number());
        assert!(json["relay_tx_bytes"].is_number());

        let config = ConfigView {
            port_range: "5000-6000".into(),
            control_port: 7835,
            max_conns: 100,
            max_carriers: 4,
            bind_addr: "0.0.0.0".into(),
            bind_tunnels: "0.0.0.0".into(),
            udp: false,
            udp_socket_send_buffer: None,
            udp_socket_recv_buffer: None,
            #[cfg(feature = "vpn")]
            vpn_enabled: false,
            #[cfg(feature = "vpn")]
            vpn_pool: None,
            #[cfg(feature = "vpn")]
            vpn_max_links: 100,
            #[cfg(feature = "vpn")]
            vpn_hub_prefix: 24,
            vhost_enabled: false,
            vhost_base_domain: None,
            vhost_http_port: None,
            vhost_https_port: None,
            tls: false,
        };
        let json = serde_json::to_value(&config).unwrap();
        assert!(json["control_port"].is_number());
        // Never serializes admin_token, key material, passwords (tested separately in 0.3)
        assert!(json.get("admin_token").is_none());
        assert!(
            json.get("secret").is_none(),
            "secret must not be serialized"
        );
        assert!(json.get("key").is_none(), "key must not be serialized");
        assert!(
            json.get("password").is_none(),
            "password must not be serialized"
        );
        assert!(json.get("token").is_none(), "token must not be serialized");
    }
}
