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
    /// Number of live public tunnels (Role::Public).
    pub public_tunnels: usize,
    /// Number of live secret tunnels (Role::SecretProvider + Role::SecretConsumer).
    pub secret_tunnels: usize,
    /// Number of live vhost domains.
    pub vhost_domains: usize,
    /// Number of live VPN links (cfg vpn).
    #[cfg(feature = "vpn")]
    pub vpn_links: usize,
    /// Vhost HTTP listener port (when vhost enabled).
    pub vhost_http_port: Option<u16>,
    /// Vhost HTTPS listener port (when vhost enabled).
    pub vhost_https_port: Option<u16>,
    /// Vhost QUIC direct-path port (when vhost enabled).
    pub vhost_quic_port: Option<u16>,
    /// Port range forwarded.
    pub port_range: String,
    /// Bind address for tunnel listeners.
    pub bind_tunnels: String,
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
    /// Number of parallel TCP carrier connections (1 = single-connection path).
    pub carriers: u16,
    /// Client runs with `--auto-reconnect`.
    pub auto_reconnect: bool,
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
    /// Operator notes.
    pub notes: Option<String>,
    /// HTTP Basic auth enforced.
    pub basic_auth: bool,
    /// Number of parallel TCP carrier connections (1 = single-connection path).
    pub carriers: u16,
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
    /// Request header key-value pairs.
    pub request_header_pairs: Vec<(String, String)>,
    /// Response header key-value pairs.
    pub response_header_pairs: Vec<(String, String)>,
    /// Size of the direct QUIC connection pool.
    pub direct_pool: usize,
    /// TLS termination.
    pub tls: bool,
}

/// VPN link (listener or connector).
#[cfg(feature = "vpn")]
#[derive(Serialize, Clone)]
pub struct VpnLinkView {
    /// Stable per-connection id.
    pub id: u64,
    /// Shared VPN link identifier (the `--id`); listener and connector(s) of the
    /// same tunnel share it. Used by the UI to group the two sides together.
    pub link_id: String,
    /// VpnListener or VpnConnector.
    pub role: String,
    /// Real peer address.
    pub peer: String,
    /// Operator note supplied with `--notes` on this side.
    pub notes: Option<String>,
    /// Overlay CIDR (e.g., "10.99.0.1/32").
    pub overlay: Option<String>,
    /// Advertised routes (as strings).
    pub advertised: Vec<String>,
    /// Number of parallel carrier QUIC connections.
    pub carriers: u16,
    /// Direct QUIC path active.
    pub direct: bool,
    /// Active path ("direct" if direct, else "relay").
    pub path: String,
    /// Relay tx bytes.
    pub relay_tx_bytes: u64,
    /// Relay rx bytes.
    pub relay_rx_bytes: u64,
    /// Seconds since connection registered.
    pub uptime_secs: u64,
    /// Link mode (1:1 for single peer, hub for multi-peer).
    pub mode: String,
    /// Auto-reconnect enabled.
    pub auto_reconnect: bool,
    /// Display-only: relay-only mode (no direct QUIC).
    pub relay_only: bool,
    /// Display-only: MTU pinning enabled.
    pub pin_mtu: bool,
    /// Display-only: TUN interface MTU.
    pub mtu: Option<u16>,
    /// Display-only: forward-accept iptables rule inserted.
    pub forward_accept: bool,
    /// Display-only: NAT masquerade enabled.
    pub nat_masquerade: bool,
    /// Display-only: route accept/refuse policy summary.
    pub route_policy: Option<String>,
    /// Display-only: client's `--nat-udp-preferred-port` (None when unset).
    pub nat_udp_port: Option<u16>,
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
    /// UDP stream receive window (human string, e.g., "16MiB").
    pub udp_stream_receive_window: String,
    /// UDP connection receive window (human string, e.g., "16MiB").
    pub udp_connection_receive_window: String,
    /// UDP send window (human string, e.g., "64MiB").
    pub udp_send_window: String,
    /// Max native QUIC bidi streams the server allows.
    pub udp_max_streams: u32,
    /// Bind domain for control/tunnel endpoints.
    pub bind_domain: Option<String>,
    /// HSTS header value for HTTPS control port.
    pub control_hsts: String,
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
    /// VPN UDP hole-punch timeout in seconds.
    #[cfg(feature = "vpn")]
    pub vpn_punch_timeout: Option<u64>,
    /// Vhost reverse-proxy enabled.
    pub vhost_enabled: bool,
    /// Vhost base domain.
    pub vhost_base_domain: Option<String>,
    /// Vhost HTTP port.
    pub vhost_http_port: Option<u16>,
    /// Vhost HTTPS port.
    pub vhost_https_port: Option<u16>,
    /// Vhost QUIC port (UDP direct path).
    pub vhost_quic_port: Option<u16>,
    /// Vhost frontend mode (http, https, both, redirect-https, auto).
    pub vhost_mode: Option<String>,
    /// Vhost configuration file path.
    pub vhost_config: Option<String>,
    /// Vhost certificate file path.
    pub vhost_cert_file: Option<String>,
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
    /// Number of live public tunnels (Role::Public).
    pub public_tunnels: usize,
    /// Number of live secret tunnels (Role::SecretProvider + Role::SecretConsumer).
    pub secret_tunnels: usize,
    /// Number of live vhost domains.
    pub vhost_domains: usize,
    /// Number of live VPN links (cfg vpn).
    #[cfg(feature = "vpn")]
    pub vpn_links: usize,
    /// Total active connections across all tunnels.
    pub active_connections: usize,
    /// Authentication / handshake failures.
    pub auth_failures: u64,
    /// Connection rejections (semaphore exhaustion).
    pub conn_rejections: u64,
    /// Direct-to-relay fallback count.
    pub direct_fallbacks: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_ovrports() {
        // Phase 0.1: SummaryView serializes the 5 new port fields.
        let summary = SummaryView {
            version: "1.0.0 - main - abc1234".into(),
            control_port: 7835,
            tls: true,
            udp: false,
            vpn_enabled: false,
            vhost_enabled: true,
            uptime_secs: 42,
            public_tunnels: 1,
            secret_tunnels: 2,
            vhost_domains: 2,
            #[cfg(feature = "vpn")]
            vpn_links: 0,
            vhost_http_port: Some(80),
            vhost_https_port: Some(443),
            vhost_quic_port: Some(443),
            port_range: "5000-6000".into(),
            bind_tunnels: "0.0.0.0".into(),
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["vhost_http_port"], 80);
        assert_eq!(json["vhost_https_port"], 443);
        assert_eq!(json["vhost_quic_port"], 443);
        assert_eq!(json["port_range"], "5000-6000");
        assert_eq!(json["bind_tunnels"], "0.0.0.0");
    }

    #[test]
    fn t_vpnview() {
        // Phase 0.2: VpnLinkView has uptime_secs, path, mode, auto_reconnect; no relay field.
        #[cfg(feature = "vpn")]
        {
            let vpn = VpnLinkView {
                id: 1,
                link_id: "site-a".into(),
                role: "vpnlistener".into(),
                peer: "10.0.0.1:1234".into(),
                notes: Some("edge".into()),
                overlay: Some("10.99.0.1/32".into()),
                advertised: vec!["10.0.0.0/24".into()],
                carriers: 4,
                direct: false,
                path: "relay".into(),
                relay_tx_bytes: 1024,
                relay_rx_bytes: 2048,
                uptime_secs: 600,
                mode: "1:1".into(),
                auto_reconnect: true,
                relay_only: false,
                pin_mtu: false,
                mtu: Some(1350),
                forward_accept: true,
                nat_masquerade: false,
                route_policy: None,
                nat_udp_port: Some(443),
                hub_peers: None,
            };
            let json = serde_json::to_value(&vpn).unwrap();
            assert_eq!(json["path"], "relay");
            assert_eq!(json["uptime_secs"], 600);
            assert_eq!(json["mode"], "1:1");
            assert_eq!(json["auto_reconnect"], true);
            assert_eq!(json["link_id"], "site-a");
            assert_eq!(json["carriers"], 4);
            assert_eq!(json["nat_udp_port"], 443);
            assert_eq!(json["notes"], "edge");
            assert_eq!(json["forward_accept"], true);
            assert!(json["relay"].is_null(), "relay field must not exist");
        }
    }

    #[test]
    fn t_cfgpaths() {
        // Phase 0.3: ConfigView has vhost_config and vhost_cert_file paths.
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
            udp_stream_receive_window: "16MiB".into(),
            udp_connection_receive_window: "16MiB".into(),
            udp_send_window: "64MiB".into(),
            udp_max_streams: 4096,
            bind_domain: None,
            control_hsts: "max-age=31536000".into(),
            #[cfg(feature = "vpn")]
            vpn_enabled: false,
            #[cfg(feature = "vpn")]
            vpn_pool: None,
            #[cfg(feature = "vpn")]
            vpn_max_links: 100,
            #[cfg(feature = "vpn")]
            vpn_hub_prefix: 24,
            #[cfg(feature = "vpn")]
            vpn_punch_timeout: Some(10),
            vhost_enabled: true,
            vhost_base_domain: Some("example.com".into()),
            vhost_http_port: Some(80),
            vhost_https_port: Some(443),
            vhost_quic_port: Some(443),
            vhost_mode: Some("https".into()),
            vhost_config: Some("/etc/bore/vhost.toml".into()),
            vhost_cert_file: Some("/certs/fullchain.pem".into()),
            tls: true,
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["vhost_config"], "/etc/bore/vhost.toml");
        assert_eq!(json["vhost_cert_file"], "/certs/fullchain.pem");
        // Verify no secrets leaked
        assert!(json.get("key").is_none());
        assert!(json.get("password").is_none());
    }

    #[test]
    fn t_metactive() {
        // Phase 0.4: MetricsView has active_connections field.
        let metrics = MetricsView {
            uptime_secs: 100,
            mem_rss_bytes: Some(100000),
            bandwidth_tx_bytes: 1000,
            bandwidth_rx_bytes: 2000,
            public_tunnels: 2,
            secret_tunnels: 1,
            vhost_domains: 1,
            #[cfg(feature = "vpn")]
            vpn_links: 0,
            active_connections: 42,
            auth_failures: 0,
            conn_rejections: 0,
            direct_fallbacks: 0,
        };
        let json = serde_json::to_value(&metrics).unwrap();
        assert_eq!(json["active_connections"], 42);
    }

    #[test]
    fn t_metcount() {
        // Phase 1.1: MetricsView has auth_failures, conn_rejections, direct_fallbacks.
        let metrics = MetricsView {
            uptime_secs: 100,
            mem_rss_bytes: Some(100000),
            bandwidth_tx_bytes: 1000,
            bandwidth_rx_bytes: 2000,
            public_tunnels: 2,
            secret_tunnels: 1,
            vhost_domains: 1,
            #[cfg(feature = "vpn")]
            vpn_links: 0,
            active_connections: 10,
            auth_failures: 5,
            conn_rejections: 3,
            direct_fallbacks: 2,
        };
        let json = serde_json::to_value(&metrics).unwrap();
        assert_eq!(json["auth_failures"], 5);
        assert_eq!(json["conn_rejections"], 3);
        assert_eq!(json["direct_fallbacks"], 2);
    }

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
            public_tunnels: 1,
            secret_tunnels: 2,
            vhost_domains: 2,
            #[cfg(feature = "vpn")]
            vpn_links: 0,
            vhost_http_port: None,
            vhost_https_port: None,
            vhost_quic_port: None,
            port_range: "5000-6000".into(),
            bind_tunnels: "0.0.0.0".into(),
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert!(json["version"].is_string());
        assert!(json["control_port"].is_number());
        assert!(json["tls"].is_boolean());
        assert!(json["uptime_secs"].is_number());
        assert!(json["public_tunnels"].is_number());
        assert!(json["secret_tunnels"].is_number());
        assert!(json["vhost_domains"].is_number());
        // Verify no legacy live_* fields
        assert!(json["live_tunnels"].is_null());
        assert!(json["live_vhost"].is_null());
        #[cfg(feature = "vpn")]
        assert!(json["live_vpn_links"].is_null());

        let tunnel = TunnelView {
            id: 1,
            peer: "10.0.0.1:54321".into(),
            public_port: Some(9000),
            notes: Some("test".into()),
            basic_auth: false,
            https: true,
            force_https: true,
            carriers: 4,
            auto_reconnect: true,
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
        // BUG-3: carriers + auto_reconnect + force_https must reach the JSON.
        assert_eq!(json["carriers"], 4);
        assert_eq!(json["auto_reconnect"], true);
        assert_eq!(json["force_https"], true);

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
            udp_stream_receive_window: "16MiB".into(),
            udp_connection_receive_window: "16MiB".into(),
            udp_send_window: "64MiB".into(),
            udp_max_streams: 4096,
            bind_domain: None,
            control_hsts: "max-age=31536000".into(),
            #[cfg(feature = "vpn")]
            vpn_enabled: false,
            #[cfg(feature = "vpn")]
            vpn_pool: None,
            #[cfg(feature = "vpn")]
            vpn_max_links: 100,
            #[cfg(feature = "vpn")]
            vpn_hub_prefix: 24,
            #[cfg(feature = "vpn")]
            vpn_punch_timeout: Some(10),
            vhost_enabled: false,
            vhost_base_domain: None,
            vhost_http_port: None,
            vhost_https_port: None,
            vhost_quic_port: None,
            vhost_mode: None,
            vhost_config: None,
            vhost_cert_file: None,
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
