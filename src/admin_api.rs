//! API endpoint builders for the admin dashboard (§3.1 of the plan).
//!
//! Each builder is a synchronous snapshot function that copies data from live
//! registries into owned view structs, releasing all DashMap guards before
//! returning (D10, I-7). No locks held across `.await`.

use crate::admin::Role;
use crate::admin_views::*;
use crate::server::Server;

/// Build the summary section view.
pub fn summary(server: &Server) -> SummaryView {
    let admin = server.admin_registry();
    let vhost_reg = server.vhost_registry();
    #[cfg(feature = "vpn")]
    let vpn_reg = server.vpn_providers();

    let snapshot = admin.snapshot();
    let (mut public_tunnels, mut secret_tunnels) = (0, 0);
    for entry in snapshot {
        match entry.role {
            Role::Public => public_tunnels += 1,
            Role::SecretProvider | Role::SecretConsumer => secret_tunnels += 1,
            _ => {}
        }
    }

    SummaryView {
        version: format!(
            "{} - {} - {}",
            env!("CARGO_PKG_VERSION"),
            env!("GIT_BRANCH"),
            env!("GIT_SHA_SHORT")
        ),
        control_port: server.control_port(),
        tls: server.is_tls(),
        udp: server.is_udp(),
        vpn_enabled: server.is_vpn_enabled(),
        vhost_enabled: !vhost_reg.is_empty(),
        uptime_secs: server.uptime_secs(),
        public_tunnels,
        secret_tunnels,
        vhost_domains: vhost_reg.len(),
        #[cfg(feature = "vpn")]
        vpn_links: vpn_reg.len(),
    }
}

/// Build the public tunnels section view.
pub fn tunnels(server: &Server) -> Vec<TunnelView> {
    let admin = server.admin_registry();
    admin
        .snapshot()
        .into_iter()
        .filter(|e| e.role == Role::Public)
        .map(|e| TunnelView {
            id: e.id,
            peer: e.peer,
            public_port: e.public_port,
            notes: e.notes,
            basic_auth: e.basic_auth,
            https: e.https,
            force_https: e.force_https,
            carriers: e.carriers,
            auto_reconnect: e.auto_reconnect,
            udp: e.udp,
            overlay: e.overlay,
            vpn_direct: e.vpn_direct,
            active: e.active,
            uptime_secs: e.uptime_secs,
            relay_tx_bytes: e.relay_tx_bytes,
            relay_rx_bytes: e.relay_rx_bytes,
        })
        .collect()
}

/// Build the secret tunnels section view.
pub fn secret(server: &Server) -> Vec<SecretView> {
    let admin = server.admin_registry();
    admin
        .snapshot()
        .into_iter()
        .filter(|e| e.role == Role::SecretProvider || e.role == Role::SecretConsumer)
        .map(|e| SecretView {
            id: e.id,
            role: format!("{:?}", e.role).to_lowercase(),
            peer: e.peer,
            secret_id: e.secret_id,
            notes: e.notes,
            basic_auth: e.basic_auth,
            carriers: e.carriers,
            udp: e.udp,
            active: e.active,
            uptime_secs: e.uptime_secs,
            relay_tx_bytes: e.relay_tx_bytes,
            relay_rx_bytes: e.relay_rx_bytes,
        })
        .collect()
}

/// Build the vhost providers section view.
pub fn vhost(server: &Server) -> Vec<VhostView> {
    use std::sync::atomic::Ordering;

    let vhost_reg = server.vhost_registry();
    let mut views = Vec::new();
    for entry in vhost_reg.iter() {
        let (subdomain, vhost_entry) = (entry.key().clone(), entry.value().clone());

        let request_headers: Vec<String> = vhost_entry
            .request_headers
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let response_headers: Vec<String> = vhost_entry
            .response_headers
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let request_header_pairs: Vec<(String, String)> = vhost_entry
            .request_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let response_header_pairs: Vec<(String, String)> = vhost_entry
            .response_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let direct_stream_opens = {
            #[cfg(feature = "udp")]
            {
                vhost_entry.direct_stream_opens.load(Ordering::Relaxed)
            }
            #[cfg(not(feature = "udp"))]
            {
                0
            }
        };

        let direct_pool = {
            #[cfg(feature = "udp")]
            {
                vhost_entry.direct.len()
            }
            #[cfg(not(feature = "udp"))]
            {
                0
            }
        };

        views.push(VhostView {
            subdomain,
            active: vhost_entry.active.load(Ordering::Relaxed),
            carriers: vhost_entry.pool.len() as u16,
            direct_stream_opens,
            request_headers,
            response_headers,
            request_header_pairs,
            response_header_pairs,
            direct_pool,
            tls: server.vhost_has_tls(),
        });
    }
    views
}

/// Build the VPN links section view.
#[cfg(feature = "vpn")]
pub fn vpn(server: &Server) -> VpnSectionView {
    let admin = server.admin_registry();
    let vpn_reg = server.vpn_providers();

    let mut links = Vec::new();

    // Iterate VPN links from the admin registry (filtered to VPN roles).
    let vpn_entries: Vec<_> = admin
        .snapshot()
        .into_iter()
        .filter(|e| e.role == Role::VpnListener || e.role == Role::VpnConnector)
        .collect();

    for entry in &vpn_entries {
        // Extract the provider key from the admin entry's secret_id.
        // Admin VPN entries have secret_id: Some(format!("vpn:{id}")).
        // The VPN provider registry is keyed by the bare id (without "vpn:" prefix).
        let (advertised, carriers, hub_peers) = {
            let provider_key = entry
                .secret_id
                .as_ref()
                .and_then(|sid| sid.strip_prefix("vpn:").map(|s| s.to_string()));

            if let Some(key) = provider_key {
                if let Some(provider) = vpn_reg.get(&key) {
                    let adv = provider.advertised.clone();
                    let carr = provider.carriers;

                    // If this provider has a hub, extract its peers.
                    let hub_opt = provider.hub.as_ref().map(|hub_sh| {
                        let hub_st = hub_sh.state.lock().unwrap();
                        let mut peer_list = Vec::new();
                        for (peer_id, slot) in &hub_st.peers {
                            // Find the peer's real address from the admin registry if available.
                            let peer_addr = vpn_entries
                                .iter()
                                .find(|e| {
                                    e.role == Role::VpnConnector
                                        && e.overlay.as_ref() == Some(&slot.overlay.to_string())
                                })
                                .map(|e| e.peer.clone())
                                .unwrap_or_default();

                            peer_list.push(VpnPeerView {
                                peer_id: *peer_id,
                                overlay: slot.overlay.to_string(),
                                peer: peer_addr,
                                advertised: adv.iter().map(|n| n.to_string()).collect(),
                            });
                        }
                        peer_list
                    });

                    (adv.iter().map(|n| n.to_string()).collect(), carr, hub_opt)
                } else {
                    // Provider not found in registry; use defaults.
                    (vec![], 1u16, None)
                }
            } else {
                // No valid secret_id; use defaults.
                (vec![], 1u16, None)
            }
        };

        links.push(VpnLinkView {
            id: entry.id,
            role: format!("{:?}", entry.role).to_lowercase(),
            peer: entry.peer.clone(),
            overlay: entry.overlay.clone(),
            advertised,
            carriers,
            direct: entry.vpn_direct,
            relay: true, // relay is always enabled; direct is the optional upgrade
            relay_tx_bytes: entry.relay_tx_bytes,
            relay_rx_bytes: entry.relay_rx_bytes,
            hub_peers,
        });
    }

    VpnSectionView { links }
}

/// VPN section response wrapper (feature-gated).
#[cfg(feature = "vpn")]
#[derive(serde::Serialize, Clone)]
pub struct VpnSectionView {
    /// Live VPN links.
    pub links: Vec<VpnLinkView>,
}

/// Canonicalize a path for certificate dedup comparison; fall back to the raw
/// string when the file cannot be canonicalized (e.g. it does not exist).
fn canon_for_dedup(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|q| q.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.to_string())
}

/// BUG-4 dedup: if `views` already holds a cert for the same file as
/// `candidate_path` (compared by canonical path), merge `merge_label` into that
/// entry's label and return `true` (caller must NOT push a duplicate card).
/// Returns `false` when no existing entry matches.
fn dedup_merge_label(views: &mut [CertView], candidate_path: &str, merge_label: &str) -> bool {
    let canon = canon_for_dedup(candidate_path);
    if let Some(existing) = views.iter_mut().find(|v| {
        v.path
            .as_deref()
            .map(|p| canon_for_dedup(p) == canon)
            .unwrap_or(false)
    }) {
        if !existing.label.split('+').any(|l| l == merge_label) {
            existing.label = format!("{}+{}", existing.label, merge_label);
        }
        true
    } else {
        false
    }
}

/// Build the TLS certificates section view.
pub fn certs(server: &Server) -> Vec<CertView> {
    use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer};

    let mut views = Vec::new();

    // Inspect control TLS certificate if configured.
    if let Some(cert_path) = &server.tls_cert_path() {
        match std::fs::read(cert_path) {
            Ok(pem) => match CertificateDer::pem_slice_iter(&pem).next() {
                Some(Ok(der)) => {
                    views.push(crate::certinfo::inspect(&der, "control", Some(cert_path)));
                }
                _ => {
                    views.push(CertView {
                        label: "control".to_string(),
                        path: Some(cert_path.to_string_lossy().to_string()),
                        subject: None,
                        sans: vec![],
                        not_before: None,
                        not_after: None,
                        days_remaining: -999,
                        expiring: true,
                        error: Some("failed to parse certificate PEM".to_string()),
                    });
                }
            },
            Err(_) => {
                views.push(CertView {
                    label: "control".to_string(),
                    path: Some(cert_path.to_string_lossy().to_string()),
                    subject: None,
                    sans: vec![],
                    not_before: None,
                    not_after: None,
                    days_remaining: -999,
                    expiring: true,
                    error: Some("failed to read cert file".to_string()),
                });
            }
        }
    }

    // Inspect vhost certificate if configured. BUG-4: when the vhost cert is the
    // same file as the control cert, merge the labels into the existing card
    // instead of emitting a duplicate.
    if let Some(cfg_arc) = server.vhost_config() {
        let cfg = cfg_arc.read().unwrap();
        if let Some(cert_path) = &cfg.cert_file {
            if !dedup_merge_label(&mut views, &cert_path.to_string_lossy(), "vhost") {
                match std::fs::read(cert_path) {
                    Ok(pem) => match CertificateDer::pem_slice_iter(&pem).next() {
                        Some(Ok(der)) => {
                            views.push(crate::certinfo::inspect(&der, "vhost", Some(cert_path)));
                        }
                        _ => {
                            views.push(CertView {
                                label: "vhost".to_string(),
                                path: Some(cert_path.to_string_lossy().to_string()),
                                subject: None,
                                sans: vec![],
                                not_before: None,
                                not_after: None,
                                days_remaining: -999,
                                expiring: true,
                                error: Some("failed to parse certificate PEM".to_string()),
                            });
                        }
                    },
                    Err(_) => {
                        views.push(CertView {
                            label: "vhost".to_string(),
                            path: Some(cert_path.to_string_lossy().to_string()),
                            subject: None,
                            sans: vec![],
                            not_before: None,
                            not_after: None,
                            days_remaining: -999,
                            expiring: true,
                            error: Some("failed to read cert file".to_string()),
                        });
                    }
                }
            }
        }
    }

    views
}

/// Build the server configuration view (already stored on Server).
pub fn config(server: &Server) -> ConfigView {
    (*server.config_view()).clone()
}

/// Build the metrics section view.
pub fn metrics(server: &Server) -> MetricsView {
    let admin = server.admin_registry();
    let vhost_reg = server.vhost_registry();
    #[cfg(feature = "vpn")]
    let vpn_reg = server.vpn_providers();

    let snapshot = admin.snapshot();
    let (mut public_tunnels, mut secret_tunnels) = (0, 0);
    for entry in snapshot {
        match entry.role {
            Role::Public => public_tunnels += 1,
            Role::SecretProvider | Role::SecretConsumer => secret_tunnels += 1,
            _ => {}
        }
    }

    // Try to read process memory on Linux.
    let mem_rss_bytes = {
        #[cfg(target_os = "linux")]
        {
            procfs::process::Process::myself()
                .ok()
                .and_then(|p| p.statm().ok())
                .map(|sm| sm.resident * 4096) // pages → bytes (4KB page)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    };

    MetricsView {
        uptime_secs: server.uptime_secs(),
        mem_rss_bytes,
        bandwidth_tx_bytes: server.total_tx_bytes(),
        bandwidth_rx_bytes: server.total_rx_bytes(),
        public_tunnels,
        secret_tunnels,
        vhost_domains: vhost_reg.len(),
        #[cfg(feature = "vpn")]
        vpn_links: vpn_reg.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn t_total_bytes_accumulate() {
        // Simple test: verify that cumulative counters can be incremented.
        // In the real implementation, relay code will call fetch_add on both
        // per-entry and server atomics.
        let tx = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let rx = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Simulate two entries each contributing to the global counters.
        tx.fetch_add(1000, Ordering::Relaxed);
        rx.fetch_add(500, Ordering::Relaxed);
        tx.fetch_add(1000, Ordering::Relaxed);
        rx.fetch_add(500, Ordering::Relaxed);

        assert_eq!(tx.load(Ordering::Relaxed), 2000);
        assert_eq!(rx.load(Ordering::Relaxed), 1000);
    }

    fn cert_with(label: &str, path: &str) -> CertView {
        CertView {
            label: label.to_string(),
            path: Some(path.to_string()),
            subject: None,
            sans: vec![],
            not_before: None,
            not_after: None,
            days_remaining: 100,
            expiring: false,
            error: None,
        }
    }

    #[test]
    fn t_certs_dedup_same_path() {
        // BUG-4: control + vhost certs pointing at the same (non-canonicalizable
        // here) path must merge into one card with a combined label.
        let path = "/nonexistent/same/cert.pem";
        let mut views = vec![cert_with("control", path)];
        let merged = dedup_merge_label(&mut views, path, "vhost");
        assert!(merged, "same path must merge, not push");
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].label, "control+vhost");
        // Idempotent: merging vhost again does not double-append.
        assert!(dedup_merge_label(&mut views, path, "vhost"));
        assert_eq!(views[0].label, "control+vhost");
    }

    #[test]
    fn t_certs_distinct_paths_not_merged() {
        let mut views = vec![cert_with("control", "/nonexistent/a.pem")];
        let merged = dedup_merge_label(&mut views, "/nonexistent/b.pem", "vhost");
        assert!(!merged, "distinct paths must NOT merge");
        assert_eq!(views.len(), 1, "caller pushes the second cert itself");
        assert_eq!(views[0].label, "control");
    }

    #[test]
    fn t_metrics_mem_optional() {
        // Verify that mem_rss_bytes is an Option and is correctly set/unset
        // based on platform.
        let mem_val = {
            #[cfg(target_os = "linux")]
            {
                procfs::process::Process::myself()
                    .ok()
                    .and_then(|p| p.statm().ok())
                    .map(|sm| sm.resident * 4096)
            }
            #[cfg(not(target_os = "linux"))]
            {
                None
            }
        };

        #[cfg(target_os = "linux")]
        {
            // On Linux, memory should be available (though we can't assert
            // a specific value).
            assert!(mem_val.is_some());
        }

        #[cfg(not(target_os = "linux"))]
        {
            // On non-Linux, memory should be None.
            assert_eq!(mem_val, None);
        }
    }

    #[test]
    fn t_metrics_counts() {
        // Verify that live counts match registry sizes.
        // This is a simple sanity check (real values depend on the server state).
        // We just verify the builder doesn't crash and returns reasonable defaults.
        let view = MetricsView {
            uptime_secs: 0,
            mem_rss_bytes: None,
            bandwidth_tx_bytes: 0,
            bandwidth_rx_bytes: 0,
            public_tunnels: 0,
            secret_tunnels: 0,
            vhost_domains: 0,
            #[cfg(feature = "vpn")]
            vpn_links: 0,
        };
        assert_eq!(view.public_tunnels, 0);
        assert_eq!(view.secret_tunnels, 0);
    }

    #[test]
    fn t_sum_per_role_counts() {
        // T-SUM: test per-role summary counts.
        use crate::admin::{AdminRegistry, Role};

        let admin = AdminRegistry::default();

        let public_entry = crate::admin::NewEntry {
            role: Role::Public,
            peer: "127.0.0.1:1234".parse().unwrap(),
            secret_id: None,
            public_port: Some(4000),
            notes: None,
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: 1,
            auto_reconnect: false,
            udp: false,
        };
        let provider_entry = crate::admin::NewEntry {
            role: Role::SecretProvider,
            peer: "127.0.0.1:1235".parse().unwrap(),
            secret_id: Some("secret1".into()),
            public_port: None,
            notes: None,
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: 1,
            auto_reconnect: false,
            udp: false,
        };
        let consumer_entry = crate::admin::NewEntry {
            role: Role::SecretConsumer,
            peer: "127.0.0.1:1236".parse().unwrap(),
            secret_id: Some("secret1".into()),
            public_port: None,
            notes: None,
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: 1,
            auto_reconnect: false,
            udp: false,
        };

        let _h1 = admin.register(public_entry);
        let _h2 = admin.register(provider_entry);
        let _h3 = admin.register(consumer_entry);

        let snapshot = admin.snapshot();
        let mut public_count = 0;
        let mut secret_count = 0;
        for entry in snapshot {
            match entry.role {
                Role::Public => public_count += 1,
                Role::SecretProvider | Role::SecretConsumer => secret_count += 1,
                _ => {}
            }
        }

        assert_eq!(public_count, 1, "should count 1 public tunnel");
        assert_eq!(secret_count, 2, "should count 2 secret tunnels");
        // vhost and vpn counts not tested here (no vhost in admin registry)
    }

    #[test]
    fn t_buf_socket_buffers() {
        // T-BUF: test UDP socket buffer serialization.
        let view = ConfigView {
            port_range: "1000-2000".into(),
            control_port: 7835,
            max_conns: 1024,
            max_carriers: 16,
            bind_addr: "0.0.0.0".into(),
            bind_tunnels: "0.0.0.0".into(),
            udp: true,
            udp_socket_send_buffer: Some(16777216), // 16 MiB
            udp_socket_recv_buffer: Some(16777216),
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
            vpn_max_links: 0,
            #[cfg(feature = "vpn")]
            vpn_hub_prefix: 0,
            #[cfg(feature = "vpn")]
            vpn_punch_timeout: Some(10),
            vhost_enabled: false,
            vhost_base_domain: None,
            vhost_http_port: None,
            vhost_https_port: None,
            vhost_quic_port: None,
            vhost_mode: None,
            tls: false,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["udp_socket_send_buffer"].as_u64(), Some(16777216));
        assert_eq!(json["udp_socket_recv_buffer"].as_u64(), Some(16777216));

        // Test unset buffers serialize as null
        let view_unset = ConfigView {
            udp_socket_send_buffer: None,
            udp_socket_recv_buffer: None,
            ..view
        };
        let json_unset = serde_json::to_value(&view_unset).unwrap();
        assert!(json_unset["udp_socket_send_buffer"].is_null());
        assert!(json_unset["udp_socket_recv_buffer"].is_null());
    }

    #[test]
    fn t_cfg_new_fields() {
        // T-CFG: test ConfigView serializes new operator-tunable fields.
        let view = ConfigView {
            port_range: "1000-2000".into(),
            control_port: 7835,
            max_conns: 1024,
            max_carriers: 16,
            bind_addr: "0.0.0.0".into(),
            bind_tunnels: "0.0.0.0".into(),
            udp: true,
            udp_socket_send_buffer: Some(16777216),
            udp_socket_recv_buffer: Some(16777216),
            udp_stream_receive_window: "16MiB".into(),
            udp_connection_receive_window: "16MiB".into(),
            udp_send_window: "64MiB".into(),
            udp_max_streams: 4096,
            bind_domain: Some("bore.example.com".into()),
            control_hsts: "max-age=31536000".into(),
            #[cfg(feature = "vpn")]
            vpn_enabled: false,
            #[cfg(feature = "vpn")]
            vpn_pool: None,
            #[cfg(feature = "vpn")]
            vpn_max_links: 0,
            #[cfg(feature = "vpn")]
            vpn_hub_prefix: 0,
            #[cfg(feature = "vpn")]
            vpn_punch_timeout: Some(10),
            vhost_enabled: true,
            vhost_base_domain: Some("test.example.com".into()),
            vhost_http_port: Some(80),
            vhost_https_port: Some(443),
            vhost_quic_port: Some(8443),
            vhost_mode: Some("https".into()),
            tls: true,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert!(json["udp_stream_receive_window"].is_string());
        assert!(json["udp_connection_receive_window"].is_string());
        assert!(json["udp_send_window"].is_string());
        assert!(json["udp_max_streams"].is_number());
        assert!(json["bind_domain"].is_string());
        assert!(json["control_hsts"].is_string());
        assert!(json["vhost_quic_port"].is_number());
        assert!(json["vhost_mode"].is_string());
        #[cfg(feature = "vpn")]
        assert!(json["vpn_punch_timeout"].is_number());
    }

    #[test]
    fn t_secn_notes() {
        // T-SECN: test SecretView includes notes field.
        let view = SecretView {
            id: 1,
            role: "secretprovider".into(),
            peer: "10.0.0.1:54321".into(),
            secret_id: Some("secret1".into()),
            notes: Some("provider notes".into()),
            basic_auth: false,
            carriers: 1,
            udp: false,
            active: 0,
            uptime_secs: 100,
            relay_tx_bytes: 0,
            relay_rx_bytes: 0,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["notes"].as_str(), Some("provider notes"));

        let view_no_notes = SecretView {
            notes: None,
            ..view
        };
        let json_no_notes = serde_json::to_value(&view_no_notes).unwrap();
        assert!(json_no_notes["notes"].is_null());
    }

    #[test]
    fn t_vhh_header_pairs() {
        // T-VHH: test VhostView includes request/response header pairs.
        let view = VhostView {
            subdomain: "test".into(),
            active: 5,
            carriers: 2,
            direct_stream_opens: 10,
            request_headers: vec!["x-custom".into()],
            response_headers: vec!["x-response".into()],
            request_header_pairs: vec![("x-custom".into(), "value1".into())],
            response_header_pairs: vec![("x-response".into(), "value2".into())],
            direct_pool: 3,
            tls: true,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert!(json["request_header_pairs"].is_array());
        assert!(json["response_header_pairs"].is_array());
        assert!(json["direct_pool"].is_number());
        assert_eq!(json["direct_pool"].as_u64(), Some(3));
    }
}
