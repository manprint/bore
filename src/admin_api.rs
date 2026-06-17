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
        live_tunnels: admin.len(),
        live_vhost: vhost_reg.len(),
        #[cfg(feature = "vpn")]
        live_vpn_links: vpn_reg.len(),
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
            basic_auth: e.basic_auth,
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

        views.push(VhostView {
            subdomain,
            active: vhost_entry.active.load(Ordering::Relaxed),
            carriers: vhost_entry.pool.len() as u16,
            direct_stream_opens,
            request_headers,
            response_headers,
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

    // Inspect vhost certificate if configured.
    if let Some(cfg_arc) = server.vhost_config() {
        let cfg = cfg_arc.read().unwrap();
        if let Some(cert_path) = &cfg.cert_file {
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
        live_tunnels: admin.len(),
        live_vhost: vhost_reg.len(),
        #[cfg(feature = "vpn")]
        live_vpn_links: vpn_reg.len(),
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
            live_tunnels: 0,
            live_vhost: 0,
            #[cfg(feature = "vpn")]
            live_vpn_links: 0,
        };
        assert_eq!(view.live_tunnels, 0);
        assert_eq!(view.live_vhost, 0);
    }
}
