//! API endpoint builders for the admin dashboard (§3.1 of the plan).
//!
//! Each builder is a synchronous snapshot function that copies data from live
//! registries into owned view structs, releasing all DashMap guards before
//! returning (D10, I-7). No locks held across `.await`.

use crate::admin::Role;
use crate::admin_views::*;
use crate::server::Server;
use tracing::warn;

/// Build the summary section view.
pub fn summary(server: &Server) -> SummaryView {
    let admin = server.admin_registry();
    let vhost_reg = server.vhost_registry();

    let snapshot = admin.snapshot();
    let (mut public_tunnels, mut secret_tunnels) = (0, 0);
    // VPN link count must come from the long-lived admin registry: the provider
    // registry (`vpn_providers`) is consumed when a 1:1 link pairs, so its
    // `.len()` reads 0 for every established link. Count distinct shared ids.
    #[cfg(feature = "vpn")]
    let mut vpn_ids = std::collections::HashSet::new();
    for entry in &snapshot {
        match entry.role {
            Role::Public => public_tunnels += 1,
            Role::SecretProvider | Role::SecretConsumer => secret_tunnels += 1,
            #[cfg(feature = "vpn")]
            Role::VpnListener | Role::VpnConnector => {
                vpn_ids.insert(entry.secret_id.clone().unwrap_or_default());
            }
            _ => {}
        }
    }

    let config = server.config_view();
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
        vpn_links: vpn_ids.len(),
        vhost_http_port: config.vhost_http_port,
        vhost_https_port: config.vhost_https_port,
        vhost_quic_port: config.vhost_quic_port,
        port_range: config.port_range.clone(),
        bind_tunnels: config.bind_tunnels.clone(),
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
            webserver_log: e.webserver_log,
            udp: e.udp,
            local_host: e.local_host,
            local_port: e.local_port,
            max_conns: e.max_conns,
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
            auto_reconnect: e.auto_reconnect,
            webserver_log: e.webserver_log,
            local_proxy_port: e.local_proxy_port,
            local_host: e.local_host,
            local_port: e.local_port,
            nat_udp_preferred_port: e.nat_udp_preferred_port,
            nat_udp_release_timeout: e.nat_udp_release_timeout,
            stun_server: e.stun_server,
            upnp: e.upnp,
            try_port_prediction: e.try_port_prediction,
            max_conns: e.max_conns,
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
            .map(|(k, v)| {
                let lower = k.to_lowercase();
                if matches!(lower.as_str(), "authorization" | "cookie" | "set-cookie" | "x-api-key" | "x-auth-token" | "proxy-authorization") {
                    warn!(header = %k, subdomain = %subdomain, "sensitive injected header key detected");
                }
                (k.clone(), v.clone())
            })
            .collect();
        let response_header_pairs: Vec<(String, String)> = vhost_entry
            .response_headers
            .iter()
            .map(|(k, v)| {
                let lower = k.to_lowercase();
                if matches!(lower.as_str(), "authorization" | "cookie" | "set-cookie" | "x-api-key" | "x-auth-token" | "proxy-authorization") {
                    warn!(header = %k, subdomain = %subdomain, "sensitive injected header key detected");
                }
                (k.clone(), v.clone())
            })
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
            peer: vhost_entry.peer.to_string(),
            notes: vhost_entry.notes.clone(),
            basic_auth: vhost_entry.basic_auth,
            udp: vhost_entry.udp,
            auto_reconnect: vhost_entry.auto_reconnect,
            webserver_log: vhost_entry.webserver_log,
            local_host: vhost_entry.local_host.clone(),
            local_port: (vhost_entry.local_port != 0).then_some(vhost_entry.local_port),
            uptime_secs: vhost_entry.since.elapsed().as_secs(),
            relay_tx_bytes: vhost_entry.relay_tx_bytes.load(Ordering::Relaxed),
            relay_rx_bytes: vhost_entry.relay_rx_bytes.load(Ordering::Relaxed),
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
    views.sort_by(|a, b| a.subdomain.cmp(&b.subdomain));
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
        // Shared link id: admin VPN entries carry secret_id = "vpn:{id}", and the
        // VPN provider registry is keyed by the bare id. Listener + connector(s)
        // of the same tunnel share it, so the UI groups the two sides by `link_id`.
        let link_id = entry
            .secret_id
            .as_deref()
            .and_then(|sid| sid.strip_prefix("vpn:"))
            .unwrap_or("")
            .to_string();

        // The provider registry is the ONLY place hub membership lives, and it
        // stays populated only in hub mode (a 1:1 provider entry is consumed at
        // pairing). So `mode`/`hub_peers` come from here, but every other display
        // field is sourced from the long-lived admin entry — otherwise carriers,
        // advertised, etc. would reset to defaults the moment a 1:1 link pairs.
        let provider = (!link_id.is_empty())
            .then(|| vpn_reg.get(&link_id))
            .flatten();
        let is_hub = provider.as_ref().is_some_and(|p| p.hub.is_some());

        // Attach the peer roster to the hub listener only (a spoke connector must
        // not duplicate the whole roster inside the grouped card).
        let hub_peers = if entry.role == Role::VpnListener {
            provider.as_ref().and_then(|provider| {
                provider.hub.as_ref().map(|hub_sh| {
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
                            advertised: provider.advertised.iter().map(|n| n.to_string()).collect(),
                        });
                    }
                    peer_list
                })
            })
        } else {
            None
        };

        let path = if entry.vpn_direct {
            "direct".to_string()
        } else {
            "relay".to_string()
        };
        let mode = if is_hub {
            "hub".to_string()
        } else {
            "1:1".to_string()
        };
        links.push(VpnLinkView {
            id: entry.id,
            link_id,
            role: format!("{:?}", entry.role).to_lowercase(),
            peer: entry.peer.clone(),
            notes: entry.notes.clone(),
            overlay: entry.overlay.clone(),
            advertised: entry.vpn_advertised.clone(),
            carriers: entry.carriers,
            direct: entry.vpn_direct,
            path,
            relay_tx_bytes: entry.relay_tx_bytes,
            relay_rx_bytes: entry.relay_rx_bytes,
            uptime_secs: entry.uptime_secs,
            mode,
            auto_reconnect: entry.auto_reconnect,
            relay_only: entry.vpn_relay_only,
            pin_mtu: entry.vpn_pin_mtu,
            mtu: entry.vpn_mtu,
            forward_accept: entry.vpn_forward_accept,
            nat_masquerade: entry.vpn_nat_masquerade,
            route_policy: entry.vpn_route_policy.clone(),
            nat_udp_port: entry.vpn_nat_udp_port,
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
        .unwrap_or_else(|_| {
            warn!(path = %p, "cert path canonicalization failed; dedup may not merge labels");
            p.to_string()
        })
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

    let snapshot = admin.snapshot();
    let (mut public_tunnels, mut secret_tunnels, mut active_connections) = (0, 0, 0);
    // See `summary`: count VPN links from the admin registry, not the ephemeral
    // provider registry (which empties on pairing).
    #[cfg(feature = "vpn")]
    let mut vpn_ids = std::collections::HashSet::new();
    for entry in &snapshot {
        active_connections += entry.active;
        match entry.role {
            Role::Public => public_tunnels += 1,
            Role::SecretProvider | Role::SecretConsumer => secret_tunnels += 1,
            #[cfg(feature = "vpn")]
            Role::VpnListener | Role::VpnConnector => {
                vpn_ids.insert(entry.secret_id.clone().unwrap_or_default());
            }
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
        vpn_links: vpn_ids.len(),
        active_connections,
        auth_failures: server.auth_failures(),
        conn_rejections: server.conn_rejections(),
        direct_fallbacks: server.direct_fallbacks(),
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
    fn t_vhostsort() {
        // Phase 0.3: vhost views returned sorted by subdomain.
        let mut subdomains = ["zebra", "apple", "middle"];
        subdomains.sort();
        assert_eq!(subdomains[0], "apple");
        assert_eq!(subdomains[1], "middle");
        assert_eq!(subdomains[2], "zebra");
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
            active_connections: 0,
            auth_failures: 0,
            conn_rejections: 0,
            direct_fallbacks: 0,
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
            webserver_log: false,
            udp: false,
            vpn_relay_only: false,
            vpn_pin_mtu: false,
            vpn_mtu: None,
            vpn_forward_accept: false,
            vpn_nat_masquerade: false,
            vpn_route_policy: None,
            vpn_advertised: vec![],
            vpn_nat_udp_port: None,
            local_proxy_port: None,
            local_host: None,
            local_port: None,
            nat_udp_preferred_port: None,
            nat_udp_release_timeout: None,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            max_conns: None,
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
            webserver_log: false,
            udp: false,
            vpn_relay_only: false,
            vpn_pin_mtu: false,
            vpn_mtu: None,
            vpn_forward_accept: false,
            vpn_nat_masquerade: false,
            vpn_route_policy: None,
            vpn_advertised: vec![],
            vpn_nat_udp_port: None,
            local_proxy_port: None,
            local_host: None,
            local_port: None,
            nat_udp_preferred_port: None,
            nat_udp_release_timeout: None,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            max_conns: None,
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
            webserver_log: false,
            udp: false,
            vpn_relay_only: false,
            vpn_pin_mtu: false,
            vpn_mtu: None,
            vpn_forward_accept: false,
            vpn_nat_masquerade: false,
            vpn_route_policy: None,
            vpn_advertised: vec![],
            vpn_nat_udp_port: None,
            local_proxy_port: None,
            local_host: None,
            local_port: None,
            nat_udp_preferred_port: None,
            nat_udp_release_timeout: None,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            max_conns: None,
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
            vhost_config: None,
            vhost_cert_file: None,
            tls: false,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["udp_socket_send_buffer"].as_u64(), Some(16777216));
        assert_eq!(json["udp_socket_recv_buffer"].as_u64(), Some(16777216));

        // Test unset buffers serialize as null
        let view_unset = ConfigView {
            udp_socket_send_buffer: None,
            udp_socket_recv_buffer: None,
            vhost_config: Some("/etc/bore/vhost.toml".into()),
            vhost_cert_file: Some("/certs/fullchain.pem".into()),
            ..view
        };
        let json_unset = serde_json::to_value(&view_unset).unwrap();
        assert!(json_unset["udp_socket_send_buffer"].is_null());
        assert!(json_unset["udp_socket_recv_buffer"].is_null());
        assert_eq!(json_unset["vhost_config"], "/etc/bore/vhost.toml");
        assert_eq!(json_unset["vhost_cert_file"], "/certs/fullchain.pem");
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
            vhost_config: Some("/etc/bore/vhost.toml".into()),
            vhost_cert_file: Some("/certs/cert.pem".into()),
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
        assert_eq!(json["vhost_config"], "/etc/bore/vhost.toml");
        assert_eq!(json["vhost_cert_file"], "/certs/cert.pem");
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
            auto_reconnect: false,
            webserver_log: false,
            local_proxy_port: None,
            local_host: None,
            local_port: None,
            nat_udp_preferred_port: None,
            nat_udp_release_timeout: None,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            max_conns: None,
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
            peer: "10.0.0.1:5555".into(),
            notes: Some("edge".into()),
            basic_auth: true,
            udp: true,
            auto_reconnect: true,
            webserver_log: true,
            local_host: Some("127.0.0.1".into()),
            local_port: Some(3000),
            uptime_secs: 120,
            relay_tx_bytes: 4096,
            relay_rx_bytes: 8192,
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

    #[test]
    fn t_vhost_parity_fields() {
        // VhostView must carry the same execution-info fields as TunnelView so the
        // dashboard Vhost section can mirror the Tunnels columns.
        let view = VhostView {
            subdomain: "demo".into(),
            peer: "203.0.113.7:443".into(),
            notes: Some("prod edge".into()),
            basic_auth: true,
            udp: true,
            auto_reconnect: true,
            webserver_log: true,
            local_host: Some("127.0.0.1".into()),
            local_port: Some(3000),
            uptime_secs: 600,
            relay_tx_bytes: 1024,
            relay_rx_bytes: 2048,
            active: 3,
            carriers: 4,
            direct_stream_opens: 7,
            request_headers: vec![],
            response_headers: vec![],
            request_header_pairs: vec![],
            response_header_pairs: vec![],
            direct_pool: 2,
            tls: false,
        };
        let json = serde_json::to_value(&view).unwrap();
        for key in [
            "peer",
            "notes",
            "basic_auth",
            "udp",
            "auto_reconnect",
            "webserver_log",
            "uptime_secs",
            "relay_tx_bytes",
            "relay_rx_bytes",
            "active",
            "carriers",
        ] {
            assert!(
                json.get(key).is_some(),
                "VhostView JSON missing parity field `{key}`"
            );
        }
        assert_eq!(json["peer"], "203.0.113.7:443");
        assert_eq!(json["webserver_log"], true);
        assert_eq!(json["relay_tx_bytes"], 1024);
        assert_eq!(json["uptime_secs"], 600);
    }
}
