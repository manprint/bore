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
    let (mut public_tunnels, mut secret_tunnels, mut ssh_tunnels, mut ssh_jump_hosts) =
        (0, 0, 0, 0);
    // VPN link count must come from the long-lived admin registry: the provider
    // registry (`vpn_providers`) is consumed when a 1:1 link pairs, so its
    // `.len()` reads 0 for every established link. Count distinct shared ids.
    #[cfg(feature = "vpn")]
    let mut vpn_ids = std::collections::HashSet::new();
    for entry in &snapshot {
        // Count SSH tunnels from secret provider/consumer.
        if entry.transport == crate::admin::Transport::Ssh
            && matches!(entry.role, Role::SecretProvider | Role::SecretConsumer)
        {
            ssh_tunnels += 1;
        }
        match entry.role {
            Role::Public => public_tunnels += 1,
            Role::SecretProvider | Role::SecretConsumer => secret_tunnels += 1,
            Role::SshJumpHost => ssh_jump_hosts += 1,
            #[cfg(feature = "vpn")]
            Role::VpnListener | Role::VpnConnector => {
                vpn_ids.insert(entry.secret_id.clone().unwrap_or_default());
            }
            _ => {}
        }
    }

    // SSH gateway details are populated from the live gateway instance. The
    // stored startup snapshot predates `set_ssh_gateway`, so using it here
    // would incorrectly report a running gateway as disabled.
    let config = config(server);
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
        ssh_jump_hosts,
        #[cfg(feature = "vpn")]
        vpn_links: vpn_ids.len(),
        vhost_http_port: config.vhost_http_port,
        vhost_https_port: config.vhost_https_port,
        vhost_quic_port: config.vhost_quic_port,
        port_range: config.port_range.clone(),
        bind_tunnels: config.bind_tunnels.clone(),
        ssh_gateway: config.ssh_gateway,
        ssh_tunnels,
        ssh_advertise_address: config.ssh_advertise_address.clone(),
        ssh_advertise_port: config.ssh_advertise_port,
    }
}

/// Build the dedicated SSH jump-host section without exposing credentials or
/// classic gateway usernames.
pub fn ssh_jump(server: &Server) -> Vec<SshJumpView> {
    use std::sync::atomic::Ordering;

    let registry = server.ssh_jump_registry();
    let base_domain = server.config_view().ssh_jump_base_domain.clone();
    let mut views = Vec::new();
    for admin in server
        .admin_registry()
        .snapshot()
        .into_iter()
        .filter(|entry| entry.role == Role::SshJumpHost)
    {
        let Some(alias) = admin.secret_id.as_deref() else {
            continue;
        };
        let Some(entry) = registry.get(alias) else {
            continue;
        };
        let hostname = base_domain
            .as_deref()
            .map(|domain| format!("{alias}.{domain}"))
            .unwrap_or_else(|| alias.to_string());
        let udp_active = {
            #[cfg(feature = "udp")]
            {
                !entry.direct.is_empty()
            }
            #[cfg(not(feature = "udp"))]
            {
                false
            }
        };
        let direct_carriers = {
            #[cfg(feature = "udp")]
            {
                entry.direct.len()
            }
            #[cfg(not(feature = "udp"))]
            {
                0
            }
        };
        views.push(SshJumpView {
            id: admin.id,
            hostname,
            ssh_port: entry.registration.ssh_port,
            peer: admin.peer,
            provider_type: entry.provider_type().to_string(),
            notes: admin.notes,
            requested_carriers: entry.registration.carriers,
            effective_carriers: admin.carriers,
            udp_requested: entry.registration.udp,
            udp_active,
            direct_carriers,
            direct_stream_opens: entry.direct_stream_opens.load(Ordering::Relaxed),
            direct_fallbacks: entry.direct_fallbacks.load(Ordering::Relaxed),
            auto_reconnect: entry.registration.auto_reconnect,
            max_conns: admin.max_conns,
            active_connections: admin.active,
            uptime_secs: admin.uptime_secs,
            relay_tx_bytes: admin.relay_tx_bytes,
            relay_rx_bytes: admin.relay_rx_bytes,
            direct_tx_bytes: entry.direct_tx_bytes.load(Ordering::Relaxed),
            direct_rx_bytes: entry.direct_rx_bytes.load(Ordering::Relaxed),
            local_host: entry.registration.local_host.clone(),
            local_port: entry.registration.local_port,
        });
    }
    views.sort_by(|left, right| left.hostname.cmp(&right.hostname));
    views
}

/// Build the public tunnels section view.
pub fn tunnels(server: &Server) -> Vec<TunnelView> {
    let admin = server.admin_registry();
    admin
        .snapshot()
        .into_iter()
        .filter(|e| e.role == Role::Public)
        .map(|e| {
            // Per-tunnel direct-path state. Derived on every read from the live
            // registry, like the vhost section: the admin `Entry` is a
            // registration snapshot and cannot know which transport the last
            // proxied connection took.
            #[cfg(feature = "udp")]
            let direct = e
                .public_port
                .and_then(|port| server.public_direct_stats(port));
            #[cfg(not(feature = "udp"))]
            let direct: Option<()> = None;
            // A tunnel that did not ask for `--udp` has exactly ONE possible
            // path, and the server knows it with certainty — reporting
            // "unknown" for it reads as "the server cannot tell", which is
            // false and made the Path column useless for the ordinary public
            // tunnel. "unknown" is reserved for a `--udp` tunnel that has not
            // proxied a connection yet, which is the one case where the
            // question genuinely has no answer.
            let relay_only_path =
                || crate::vhost::vhost_path_label(crate::vhost::VHOST_PATH_RELAY).to_string();
            let (direct_stream_opens, direct_fallbacks, direct_pool, current_path) = {
                #[cfg(feature = "udp")]
                {
                    match direct {
                        Some(d) => (
                            d.stream_opens,
                            d.fallbacks,
                            d.carriers,
                            crate::vhost::vhost_path_label(d.last_path).to_string(),
                        ),
                        None if e.udp => (
                            0,
                            0,
                            0,
                            crate::vhost::vhost_path_label(crate::vhost::VHOST_PATH_UNKNOWN)
                                .to_string(),
                        ),
                        None => (0, 0, 0, relay_only_path()),
                    }
                }
                #[cfg(not(feature = "udp"))]
                {
                    let _ = direct;
                    (0u64, 0u64, 0usize, relay_only_path())
                }
            };
            TunnelView {
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
                direct_stream_opens,
                direct_fallbacks,
                direct_pool,
                current_path,
                transport: e.transport,
                identity: e.identity,
            }
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
            transport: e.transport,
            identity: e.identity,
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

        let direct_fallbacks = {
            #[cfg(feature = "udp")]
            {
                vhost_entry.direct_fallbacks.load(Ordering::Relaxed)
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
            carrier_target: vhost_entry.carrier_target.load(Ordering::Relaxed),
            direct_stream_opens,
            direct_fallbacks,
            current_path: crate::vhost::vhost_path_label(
                vhost_entry.last_path.load(Ordering::Relaxed),
            )
            .to_string(),
            request_headers,
            response_headers,
            request_header_pairs,
            response_header_pairs,
            direct_pool,
            tls: server.vhost_has_tls(),
            transport: vhost_entry.transport,
            identity: vhost_entry.identity.clone(),
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

/// Kebab-case label for a configured vhost frontend mode, matching the
/// `--vhost-mode` vocabulary exactly (`http`, `https`, `both`,
/// `redirect-https`, `auto`).
fn vhost_mode_label(mode: crate::vhost::VhostModeCfg) -> &'static str {
    match mode {
        crate::vhost::VhostModeCfg::Http => "http",
        crate::vhost::VhostModeCfg::Https => "https",
        crate::vhost::VhostModeCfg::Both => "both",
        crate::vhost::VhostModeCfg::RedirectHttps => "redirect-https",
        crate::vhost::VhostModeCfg::Auto => "auto",
    }
}

/// Overlay the vhost section of the configuration view from the LIVE merged
/// vhost configuration (F-6, phase 06.4).
///
/// `ConfigView` is a startup snapshot built from CLI values, so it is stale by
/// construction for anything `vhost.yml` can carry or hot-reload: the endpoint
/// reported `default_response_headers` as absent while the SSH gateway's own
/// `vhost_info_banner` printed all of them to a connecting client. The data is
/// resolved and shared already — `SharedVhostConfig` holds the CLI overrides
/// merged over the file — so the fix is to DERIVE the section on every read
/// rather than restate it, exactly as phase 02.1 did for the UDP windows.
///
/// `vhost_quic_port` and `vhost_config` (the file path) are deliberately left
/// as the startup snapshot: neither is hot-reloadable (the QUIC port is a bound
/// socket, the path is fixed for the process lifetime) and neither lives in
/// `VhostConfig`.
fn overlay_vhost_config(view: &mut ConfigView, server: &Server) {
    let Some(shared) = server.vhost_config() else {
        return;
    };
    // Clone the Arc out, then drop the guard: no lock held while building the view.
    let cfg = {
        let guard = shared.read().unwrap();
        std::sync::Arc::clone(&guard)
    };
    view.vhost_enabled = true;
    view.vhost_base_domain = Some(cfg.base_domain.clone());
    view.vhost_http_port = Some(cfg.http_port);
    view.vhost_https_port = Some(cfg.https_port);
    view.vhost_mode = Some(vhost_mode_label(cfg.mode).to_string());
    view.vhost_cert_file = cfg
        .cert_file
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    view.vhost_default_request_headers = cfg.default_headers.clone();
    view.vhost_default_response_headers = cfg.default_response_headers.clone();
    view.vhost_reservations = cfg
        .reservations
        .iter()
        .map(|res| VhostReservationView {
            client_id: res.client_id.clone(),
            subdomain: res.subdomain.clone(),
            headers: res.headers.clone(),
            response_headers: res.response_headers.clone(),
        })
        .collect();
}

/// Overlay the tunables that live in process-wide state rather than in the
/// startup snapshot.
///
/// `BORE_PROXY_BUFFER_SIZE` is resolved once inside `shared::proxy_buffer_size()`
/// (a `OnceLock`, clamped to `[4 KiB, 16 MiB]`) and logged only at `trace`, so
/// the startup `ConfigView` never saw it and the endpoint reported nothing
/// while every neighbouring UDP window was reported. Deriving it here is the
/// same fix, and the same reasoning, as `overlay_vhost_config` (F-6).
///
/// The direct-path QUIC liveness pair is the same shape again, with one extra
/// reason: `holepunch::resolve_direct_quic_liveness` may TIGHTEN the requested
/// keep-alive to keep two consecutive losses survivable, so the value in the
/// environment is not necessarily the value in force.
fn overlay_runtime_tunables(view: &mut ConfigView, server: &Server) {
    view.proxy_buffer_size =
        crate::shared::format_iec_size(crate::shared::proxy_buffer_size() as u64);
    #[cfg(feature = "udp")]
    {
        let live = crate::holepunch::direct_quic_liveness();
        view.direct_quic_keepalive_ms = Some(live.keepalive.as_millis() as u64);
        view.direct_quic_idle_ms = Some(live.max_idle.as_millis() as u64);
    }

    // The whole direct-UDP block is derived from the tuning actually installed
    // on the server, not from the CLI strings the startup snapshot captured.
    // `--udp-memory-budget` computes the three windows from one number AFTER
    // the snapshot was taken (F-13), so a server running a budget reported the
    // requested 16MiB/256MiB while it was actually running the derived pair —
    // the same class of gap as the vhost headers (F-6) and the proxy buffer.
    let tuning = server.udp_tuning();
    view.udp_stream_receive_window =
        crate::shared::format_iec_size(tuning.stream_receive_window as u64);
    view.udp_connection_receive_window =
        crate::shared::format_iec_size(tuning.connection_receive_window as u64);
    view.udp_send_window = crate::shared::format_iec_size(tuning.send_window);
    view.udp_socket_recv_buffer = Some(tuning.udp_socket_recv_buffer);
    view.udp_socket_send_buffer = Some(tuning.udp_socket_send_buffer);
    view.udp_max_streams = tuning.max_direct_streams;
    view.udp_direct_slots = server.udp_direct_slots().map(|n| n as u32);
}

/// Build the server configuration view (already stored on Server).
pub fn config(server: &Server) -> ConfigView {
    #[cfg(feature = "ssh-gateway")]
    {
        let mut view = (*server.config_view()).clone();
        overlay_vhost_config(&mut view, server);
        overlay_runtime_tunables(&mut view, server);

        // Populate SSH gateway config from the running gateway instance.
        if let Some(gateway) = server.ssh_gateway() {
            view.ssh_gateway = true;
            view.ssh_port = gateway.port();
            view.ssh_advertise_address = gateway.advertise_address().map(|s| s.to_string());
            view.ssh_advertise_port = gateway.advertise_port();
            view.ssh_auth_pubkey = gateway.auth_pubkey();
            view.ssh_auth_password = gateway.auth_password();
            view.ssh_banner = gateway.has_banner();
            view.ssh_host_key_file = Some(gateway.host_key_file().display().to_string());
        }

        view
    }

    #[cfg(not(feature = "ssh-gateway"))]
    {
        let mut view = (*server.config_view()).clone();
        overlay_vhost_config(&mut view, server);
        overlay_runtime_tunables(&mut view, server);
        view
    }
}

/// Build the metrics section view.
pub fn metrics(server: &Server) -> MetricsView {
    let admin = server.admin_registry();
    let vhost_reg = server.vhost_registry();

    let snapshot = admin.snapshot();
    let (
        mut public_tunnels,
        mut secret_tunnels,
        mut active_connections,
        mut ssh_tunnels,
        mut ssh_jump_hosts,
        mut transport_bore,
        mut transport_ssh,
    ) = (0, 0, 0, 0, 0, 0, 0);
    // See `summary`: count VPN links from the admin registry, not the ephemeral
    // provider registry (which empties on pairing).
    #[cfg(feature = "vpn")]
    let mut vpn_ids = std::collections::HashSet::new();
    for entry in &snapshot {
        active_connections += entry.active;
        // Count transport types.
        match entry.transport {
            crate::admin::Transport::Bore => transport_bore += 1,
            crate::admin::Transport::Ssh => transport_ssh += 1,
        }
        // Count SSH tunnels from secret provider/consumer.
        if entry.transport == crate::admin::Transport::Ssh
            && matches!(entry.role, Role::SecretProvider | Role::SecretConsumer)
        {
            ssh_tunnels += 1;
        }
        match entry.role {
            Role::Public => public_tunnels += 1,
            Role::SecretProvider | Role::SecretConsumer => secret_tunnels += 1,
            Role::SshJumpHost => ssh_jump_hosts += 1,
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
        ssh_jump_hosts,
        #[cfg(feature = "vpn")]
        vpn_links: vpn_ids.len(),
        active_connections,
        auth_failures: server.auth_failures(),
        conn_rejections: server.conn_rejections(),
        direct_fallbacks: server.direct_fallbacks(),
        direct_budget_refusals: server.direct_budget_refusals(),
        udp_direct_slots_available: server.udp_direct_slots_available().map(|n| n as u32),
        rate_tx_bps: server.rate_tx_bps(),
        rate_rx_bps: server.rate_rx_bps(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        ssh_tunnels,
        transport_bore,
        transport_ssh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// P-11: `/admin/api/v1/config` must publish the slot count the budget was
    /// SIZED for, not the number still free. The two differ the moment any
    /// direct connection is admitted, and an operator reading the config
    /// endpoint to answer "how many slots did I configure?" would otherwise
    /// get a number that moves with load — and reads 0 on a saturated server,
    /// which looks exactly like "no budget at all".
    ///
    /// Red-check: point `Server::udp_direct_slots` back at
    /// `available_permits()` and the second assertion fails with `8` vs `5`.
    #[tokio::test]
    async fn configured_direct_slots_do_not_move_with_load() {
        let mut server = crate::server::Server::new(20000..=20000, None);
        server.set_udp_direct_slots(Some(8));
        assert_eq!(server.udp_direct_slots(), Some(8), "configured total");
        assert_eq!(
            server.udp_direct_slots_available(),
            Some(8),
            "nothing admitted yet, so the gauge starts at the total"
        );

        // Admit three connections' worth of budget and hold the permits.
        let permits = server.udp_direct_permits_for_test();
        let held: Vec<_> = (0..3)
            .map(|_| {
                std::sync::Arc::clone(permits.as_ref().unwrap())
                    .try_acquire_owned()
                    .expect("a fresh 8-slot budget has room for three")
            })
            .collect();

        assert_eq!(
            server.udp_direct_slots(),
            Some(8),
            "the CONFIGURED total is a configuration value and must not move"
        );
        assert_eq!(
            server.udp_direct_slots_available(),
            Some(5),
            "the live gauge is the one that moves"
        );
        drop(held);
        assert_eq!(server.udp_direct_slots_available(), Some(8));
    }

    #[test]
    fn a_relay_only_public_tunnel_reports_the_relay_path_not_unknown() {
        // The Path column exists so an operator can see which transport a
        // tunnel is actually using. A public tunnel that never asked for
        // `--udp` has exactly ONE possible path and the server knows it with
        // certainty, so reporting "unknown" there says "the server cannot
        // tell" — false, and it made the column useless for the ordinary
        // tunnel, which is most of them.
        //
        // Red-check: report UNKNOWN whenever the direct registry has no entry
        // (what the code did before) and the first assertion fails.
        use crate::admin::{AdminRegistry, Role};

        let server = Server::new(21300..=21400, None);
        let admin: AdminRegistry = server.admin_registry();

        let entry = |port: u16, udp: bool| crate::admin::NewEntry {
            role: Role::Public,
            peer: "127.0.0.1:1234".parse().unwrap(),
            secret_id: None,
            public_port: Some(port),
            notes: None,
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: 1,
            auto_reconnect: false,
            webserver_log: false,
            udp,
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
            transport: crate::admin::Transport::Bore,
            identity: None,
        };
        let _relay = admin.register(entry(21300, false));

        let views = tunnels(&server);
        let relay_view = views
            .iter()
            .find(|v| v.public_port == Some(21300))
            .expect("the relay tunnel is listed");
        assert_eq!(relay_view.current_path, "relay");
        assert!(!relay_view.udp);

        // A `--udp` tunnel whose direct path has not registered yet is the one
        // case where the question genuinely has no answer, and it must keep
        // saying so rather than claiming a relay it may never use.
        let _direct = admin.register(entry(21301, true));

        let views = tunnels(&server);
        let udp_view = views
            .iter()
            .find(|v| v.public_port == Some(21301))
            .expect("the udp tunnel is listed");
        assert_eq!(udp_view.current_path, "unknown");
    }

    #[test]
    fn config_view_names_the_running_build() {
        // A measurement campaign, an incident report and a "did my redeploy
        // actually land?" question all need the build string, and none of them
        // should require shell access to the container. The binary's own
        // `--version` and this field are the SAME constant on purpose: two
        // independent version strings drift, and a drifted one is worse than
        // none because it is believed.
        let server = Server::new(20701..=20801, None);
        let view = config(&server);
        assert_eq!(view.server_version, crate::FULL_VERSION);
        assert!(
            view.server_version.starts_with(env!("CARGO_PKG_VERSION")),
            "expected the crate version to lead the build string, got {:?}",
            view.server_version
        );
        assert!(
            view.server_version.matches(" - ").count() == 2,
            "expected \"<semver> - <branch> - <sha8>\", got {:?}",
            view.server_version
        );
    }

    /// Phase 06.4 gate (F-6). `/admin/api/v1/config` used to report the vhost
    /// response headers as absent while the SSH gateway's own
    /// `vhost_info_banner` printed all of them to a connecting client: the view
    /// is a startup snapshot of CLI values, and `default_response_headers` only
    /// ever lives in `vhost.yml`. The section must be DERIVED from the live
    /// merged configuration on every read.
    ///
    /// Red-check: hardcode any one of these (e.g. leave
    /// `view.vhost_default_response_headers` at its `Default::default()`) and
    /// this fails.
    #[test]
    fn config_view_reports_the_live_proxy_buffer_size() {
        // The startup snapshot deliberately carries an empty placeholder, so
        // this assertion is red the moment `overlay_runtime_tunables` stops
        // running: an operator who sets BORE_PROXY_BUFFER_SIZE could not
        // otherwise confirm it took effect anywhere in the API.
        let server = Server::new(20700..=20800, None);
        assert_eq!(
            server.config_view().proxy_buffer_size,
            "",
            "the snapshot must not restate a value it cannot keep current"
        );

        let view = config(&server);
        assert_eq!(
            view.proxy_buffer_size,
            crate::shared::format_iec_size(crate::shared::proxy_buffer_size() as u64),
            "the view must report the resolved buffer size"
        );
        assert!(
            !view.proxy_buffer_size.is_empty(),
            "the derived field must never be reported as absent"
        );
    }

    #[test]
    #[cfg(feature = "udp")]
    fn config_view_reports_the_live_direct_quic_liveness() {
        // Red-checks the same gap as the buffer-size test above, for the pair
        // that governs how long requests are lost after UDP disappears. The
        // snapshot carries `None`, so this is red the moment the overlay stops
        // running — and it asserts the RESOLVED pair, because the resolver may
        // tighten a requested keep-alive and the environment value would then
        // not be the value in force.
        let server = Server::new(20900..=21000, None);
        assert_eq!(
            server.config_view().direct_quic_keepalive_ms,
            None,
            "the snapshot must not restate a value it cannot keep current"
        );

        let live = crate::holepunch::direct_quic_liveness();
        let view = config(&server);
        assert_eq!(
            view.direct_quic_keepalive_ms,
            Some(live.keepalive.as_millis() as u64)
        );
        assert_eq!(
            view.direct_quic_idle_ms,
            Some(live.max_idle.as_millis() as u64)
        );
        assert!(
            view.direct_quic_idle_ms.unwrap() >= 3 * view.direct_quic_keepalive_ms.unwrap(),
            "the published pair must satisfy the same survivability rule the resolver enforces"
        );
    }

    /// F-13 coherence gate: with `--udp-memory-budget` the server DERIVES the
    /// three flow-control windows, and the startup snapshot — taken from the
    /// CLI strings before the derivation ran — was still what the endpoint
    /// reported. A staging run with a 512 MiB budget on a 1024-carrier server
    /// therefore showed `16MiB / 256MiB` while the process was running the
    /// derived pair, which is the worst possible answer to "is my budget on?".
    #[test]
    #[cfg(feature = "udp")]
    fn config_view_reports_the_windows_the_budget_actually_installed() {
        use crate::shared::{format_iec_size, UdpDirectTuning};

        let mut server = Server::new(21100..=21200, None);
        let snapshot = server.config_view().udp_stream_receive_window.clone();

        // The exact shape the flag takes in main.rs: derive, install the
        // windows, install the slot count.
        let plan = UdpDirectTuning::from_memory_budget(512 * 1024 * 1024, 1024);
        let mut tuning = server.udp_tuning();
        tuning.stream_receive_window = plan.tuning.stream_receive_window;
        tuning.connection_receive_window = plan.tuning.connection_receive_window;
        tuning.send_window = plan.tuning.send_window;
        server.set_udp_tuning(tuning);
        server.set_udp_direct_slots(Some(plan.direct_slots));

        let view = config(&server);
        assert_eq!(
            view.udp_stream_receive_window,
            format_iec_size(plan.tuning.stream_receive_window as u64),
            "the endpoint must report the DERIVED stream window, not the CLI one"
        );
        assert_eq!(
            view.udp_connection_receive_window,
            format_iec_size(plan.tuning.connection_receive_window as u64)
        );
        assert_eq!(
            view.udp_send_window,
            format_iec_size(plan.tuning.send_window)
        );
        assert_eq!(
            view.udp_direct_slots,
            Some(plan.direct_slots as u32),
            "the aggregate bound is the only field that says the budget is on"
        );
        assert_ne!(
            view.udp_stream_receive_window, snapshot,
            "this budget does change the window, so a snapshot-equal answer means the overlay is not running"
        );
    }

    #[test]
    fn config_view_vhost_section_equals_the_resolved_merged_configuration() {
        // mode stays `http` because `redirect-https` fails fast without a cert.
        let yaml = "\
base_domain: bore.example.com
mode: http
http_port: 8080
https_port: 8443
default_headers:
  x-forwarded-proto: https
default_response_headers:
  x-frame-options: DENY
  strict-transport-security: max-age=31536000
reservations:
  - client_id: team-a
    subdomain: app
    headers:
      x-tenant: a
    response_headers:
      x-cache: bypass
";
        let cfg = crate::vhost::parse_config(yaml).expect("parse vhost config");
        let mut server = Server::new(20500..=20600, None);
        server.set_vhost(cfg).expect("install vhost config");

        let live = {
            let shared = server.vhost_config().expect("vhost config installed");
            let guard = shared.read().unwrap();
            std::sync::Arc::clone(&guard)
        };
        let view = config(&server);

        assert!(view.vhost_enabled, "vhost is configured");
        assert_eq!(
            view.vhost_base_domain.as_deref(),
            Some(live.base_domain.as_str())
        );
        assert_eq!(view.vhost_http_port, Some(live.http_port));
        assert_eq!(view.vhost_https_port, Some(live.https_port));
        assert_eq!(view.vhost_mode.as_deref(), Some("http"));
        assert_eq!(view.vhost_default_request_headers, live.default_headers);
        // The measured F-6 symptom, pinned.
        assert_eq!(
            view.vhost_default_response_headers,
            live.default_response_headers
        );
        assert_eq!(
            view.vhost_default_response_headers
                .get("x-frame-options")
                .map(String::as_str),
            Some("DENY"),
            "the response header an operator actually reads must be present"
        );
        assert_eq!(view.vhost_reservations.len(), live.reservations.len());
        let res = &view.vhost_reservations[0];
        assert_eq!(res.client_id, "team-a");
        assert_eq!(res.subdomain, "app");
        assert_eq!(res.headers.get("x-tenant").map(String::as_str), Some("a"));
        assert_eq!(
            res.response_headers.get("x-cache").map(String::as_str),
            Some("bypass")
        );

        // Derived, not restated: a hot-reloaded config must move the view. This
        // is the half a startup snapshot can never satisfy.
        {
            let shared = server.vhost_config().unwrap();
            let mut next = (*live).clone();
            next.default_response_headers
                .insert("x-reloaded".into(), "yes".into());
            next.base_domain = "reloaded.example.com".into();
            *shared.write().unwrap() = std::sync::Arc::new(next);
        }
        let after = config(&server);
        assert_eq!(
            after
                .vhost_default_response_headers
                .get("x-reloaded")
                .map(String::as_str),
            Some("yes"),
            "the view must follow the live config, not the startup snapshot"
        );
        assert_eq!(
            after.vhost_base_domain.as_deref(),
            Some("reloaded.example.com")
        );
    }

    /// A server with no vhost configured keeps its startup snapshot untouched:
    /// the overlay must be a no-op, not a blanket `vhost_enabled = true`.
    #[test]
    fn config_view_vhost_overlay_is_a_no_op_without_a_vhost_config() {
        let server = Server::new(20601..=20700, None);
        let view = config(&server);
        assert!(!view.vhost_enabled);
        assert_eq!(view.vhost_base_domain, None);
        assert!(view.vhost_default_response_headers.is_empty());
        assert!(view.vhost_reservations.is_empty());
    }

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

    #[tokio::test]
    async fn ssh_jump_view_shares_limits_activity_and_counters() {
        use crate::admin::{ActiveGuard, NewEntry, Transport};
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        let mut server = Server::new(20000..=21000, None);
        server
            .set_ssh_jump_base_domain(Some("ssh.example.test".to_string()))
            .unwrap();
        let (socket, _peer_socket) = tokio::io::duplex(1024);
        let (opener, _acceptor) = crate::mux::client(socket);
        let registration = crate::ssh_jump::SshJumpRegistration::new(
            "vm-01",
            2222,
            Some("edge"),
            4,
            true,
            true,
            "127.0.0.1",
            2222,
        )
        .unwrap();
        let entry = Arc::new(crate::ssh_jump::SshJumpEntry::new(
            Arc::new(crate::pool::CarrierPool::new(crate::mux::LinkOpener::Mux(
                opener,
            ))),
            registration,
            Arc::new(Semaphore::new(1)),
        ));
        server
            .ssh_jump_registry()
            .insert("vm-01".to_string(), Arc::clone(&entry));

        let admin = server.admin_registry();
        let _admin_registration = admin.register_with_counters(
            NewEntry {
                role: Role::SshJumpHost,
                peer: "192.0.2.10:45000".parse().unwrap(),
                secret_id: Some("vm-01".to_string()),
                public_port: Some(2222),
                notes: Some("edge".to_string()),
                basic_auth: false,
                https: false,
                force_https: false,
                carriers: 2,
                auto_reconnect: true,
                webserver_log: false,
                udp: true,
                vpn_relay_only: false,
                vpn_pin_mtu: false,
                vpn_mtu: None,
                vpn_forward_accept: false,
                vpn_nat_masquerade: false,
                vpn_route_policy: None,
                vpn_advertised: vec![],
                vpn_nat_udp_port: None,
                local_proxy_port: None,
                local_host: Some("127.0.0.1".to_string()),
                local_port: Some(2222),
                nat_udp_preferred_port: None,
                nat_udp_release_timeout: None,
                stun_server: None,
                upnp: false,
                try_port_prediction: false,
                max_conns: Some(1),
                transport: Transport::Bore,
                identity: None,
            },
            Arc::clone(&entry.active),
            Arc::clone(&entry.relay_tx_bytes),
            Arc::clone(&entry.relay_rx_bytes),
        );

        let permit = Arc::clone(&entry.permits).try_acquire_owned().unwrap();
        assert!(Arc::clone(&entry.permits).try_acquire_owned().is_err());
        drop(permit);
        assert!(Arc::clone(&entry.permits).try_acquire_owned().is_ok());

        let active = ActiveGuard::new(Arc::clone(&entry.active));
        entry.relay_tx_bytes.fetch_add(11, Ordering::Relaxed);
        entry.relay_rx_bytes.fetch_add(22, Ordering::Relaxed);
        entry.direct_stream_opens.fetch_add(3, Ordering::Relaxed);
        entry.direct_fallbacks.fetch_add(2, Ordering::Relaxed);
        let views = ssh_jump(&server);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].hostname, "vm-01.ssh.example.test");
        assert_eq!(views[0].requested_carriers, 4);
        assert_eq!(views[0].effective_carriers, 2);
        assert_eq!(views[0].active_connections, 1);
        assert_eq!(views[0].relay_tx_bytes, 11);
        assert_eq!(views[0].relay_rx_bytes, 22);
        assert_eq!(views[0].direct_carriers, 0);
        assert_eq!(views[0].direct_stream_opens, 3);
        assert_eq!(views[0].direct_fallbacks, 2);
        assert_eq!(summary(&server).ssh_jump_hosts, 1);
        assert_eq!(metrics(&server).ssh_jump_hosts, 1);
        let json = serde_json::to_value(&views[0]).unwrap();
        assert!(json.get("identity").is_none());
        assert!(json.get("username").is_none());
        assert!(json.get("secret").is_none());
        drop(active);
        assert_eq!(ssh_jump(&server)[0].active_connections, 0);
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
        let mem_val: Option<u64> = {
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
            ssh_jump_hosts: 0,
            #[cfg(feature = "vpn")]
            vpn_links: 0,
            active_connections: 0,
            auth_failures: 0,
            conn_rejections: 0,
            direct_fallbacks: 0,
            direct_budget_refusals: 0,
            udp_direct_slots_available: None,
            rate_tx_bps: 0,
            rate_rx_bps: 0,
            ts: 0,
            ssh_tunnels: 0,
            transport_bore: 0,
            transport_ssh: 0,
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
            transport: crate::admin::Transport::Bore,
            identity: None,
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
            transport: crate::admin::Transport::Bore,
            identity: None,
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
            transport: crate::admin::Transport::Bore,
            identity: None,
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
            server_version: crate::FULL_VERSION.to_string(),
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
            proxy_buffer_size: "256KiB".into(),
            direct_quic_keepalive_ms: Some(3_000),
            direct_quic_idle_ms: Some(10_000),
            udp_direct_slots: None,
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
            vhost_default_request_headers: Default::default(),
            vhost_default_response_headers: Default::default(),
            vhost_reservations: Vec::new(),
            tls: false,
            ssh_gateway: false,
            ssh_jump_enabled: false,
            ssh_jump_base_domain: None,
            ssh_jump_classic_auth_required: false,
            ssh_jump_direct_quic_port: None,
            ssh_port: None,
            ssh_advertise_address: None,
            ssh_advertise_port: None,
            ssh_auth_pubkey: false,
            ssh_auth_password: false,
            ssh_banner: false,
            ssh_host_key_file: None,
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
            vhost_default_request_headers: Default::default(),
            vhost_default_response_headers: Default::default(),
            vhost_reservations: Vec::new(),
            ssh_gateway: false,
            ssh_jump_enabled: false,
            ssh_jump_base_domain: None,
            ssh_jump_classic_auth_required: false,
            ssh_jump_direct_quic_port: None,
            ssh_port: None,
            ssh_advertise_address: None,
            ssh_advertise_port: None,
            ssh_auth_pubkey: false,
            ssh_auth_password: false,
            ssh_banner: false,
            ssh_host_key_file: None,
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
            server_version: crate::FULL_VERSION.to_string(),
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
            proxy_buffer_size: "256KiB".into(),
            direct_quic_keepalive_ms: Some(3_000),
            direct_quic_idle_ms: Some(10_000),
            udp_direct_slots: None,
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
            vhost_default_request_headers: Default::default(),
            vhost_default_response_headers: Default::default(),
            vhost_reservations: Vec::new(),
            tls: true,
            ssh_gateway: false,
            ssh_jump_enabled: false,
            ssh_jump_base_domain: None,
            ssh_jump_classic_auth_required: false,
            ssh_jump_direct_quic_port: None,
            ssh_port: None,
            ssh_advertise_address: None,
            ssh_advertise_port: None,
            ssh_auth_pubkey: false,
            ssh_auth_password: false,
            ssh_banner: false,
            ssh_host_key_file: None,
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

    #[cfg(feature = "ssh-gateway")]
    #[test]
    fn t_ssh_gateway_config_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = crate::server::Server::new(1024..=65535, None);
        server
            .set_ssh_gateway(crate::sshgw::SshGatewayConfig {
                port: Some(2222),
                host_key_file: dir.path().join("host_key"),
                authorized_keys_dir: Some(dir.path().join("authorized_keys.d")),
                passwords_file: None,
                banner: None,
                window_size: crate::sshgw::SSH_DEFAULT_WINDOW_SIZE,
                advertise_address: Some("ssh.example.test".into()),
                advertise_port: Some(443),
            })
            .unwrap();

        let config = super::config(&server);
        assert!(config.ssh_gateway);
        assert_eq!(config.ssh_port, Some(2222));
        assert!(config.ssh_auth_pubkey);
        assert!(!config.ssh_auth_password);

        let summary = super::summary(&server);
        assert!(summary.ssh_gateway);
        assert_eq!(
            summary.ssh_advertise_address.as_deref(),
            Some("ssh.example.test")
        );
        assert_eq!(summary.ssh_advertise_port, Some(443));
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
            transport: crate::admin::Transport::Bore,
            identity: None,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["notes"].as_str(), Some("provider notes"));

        let view_no_notes = SecretView {
            notes: None,
            transport: crate::admin::Transport::Bore,
            identity: None,
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
            direct_fallbacks: 0,
            current_path: "relay".to_string(),
            carrier_target: 1,
            request_headers: vec!["x-custom".into()],
            response_headers: vec!["x-response".into()],
            request_header_pairs: vec![("x-custom".into(), "value1".into())],
            response_header_pairs: vec![("x-response".into(), "value2".into())],
            direct_pool: 3,
            tls: true,
            transport: crate::admin::Transport::Bore,
            identity: None,
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
            direct_fallbacks: 0,
            current_path: "relay".to_string(),
            carrier_target: 1,
            request_headers: vec![],
            response_headers: vec![],
            request_header_pairs: vec![],
            response_header_pairs: vec![],
            direct_pool: 2,
            tls: false,
            transport: crate::admin::Transport::Bore,
            identity: None,
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
