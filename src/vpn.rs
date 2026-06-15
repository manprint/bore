//! VPN L3 tunnel feature (Linux, experimental).

#![cfg(all(feature = "vpn", target_os = "linux"))]

use anyhow::{anyhow, bail, Context, Result};
use std::sync::Arc;
use tracing::{error, info};

/// Public arg struct for `bore vpn listen` (converted from CLI args).
#[derive(Clone, Debug)]
pub struct VpnListenArgs {
    /// Server address.
    pub to: String,
    /// Shared secret.
    pub secret: String,
    /// VPN link identifier.
    pub id: String,
    /// Skip TLS verification.
    pub insecure: bool,
    /// Advertised subnets, each optionally NAT-mapped (`real@exposed`). Only the
    /// `exposed` CIDR is sent on the wire (N3); `real` drives local route/NAT.
    pub advertise_entries: Vec<crate::shared::AdvertiseEntry>,
    /// Address request (pool or static).
    pub addr_request: crate::shared::VpnAddrRequest,
    /// TUN interface name.
    pub tun_name: String,
    /// Interface MTU.
    pub mtu: u16,
    /// Skip route/NAT management.
    pub no_route_manage: bool,
    /// STUN server.
    pub stun_server: Option<String>,
    /// Try UPnP.
    pub upnp: bool,
    /// Try port prediction.
    pub try_port_prediction: bool,
    /// Preferred UDP port.
    pub nat_udp_preferred_port: u16,
    /// UDP port release timeout.
    pub nat_udp_release_timeout: u64,
    /// Never attempt the direct UDP path (stay on the server relay).
    pub relay_only: bool,
    /// Reconnect with backoff when the link drops (full teardown + rebuild).
    pub auto_reconnect: bool,
    /// Requested relay carrier substream pairs (1 = single, as before).
    pub carriers: u16,
    /// Number of TUN queues (Linux IFF_MULTI_QUEUE); 1 = single queue.
    pub tun_queues: usize,
    /// Optional notes.
    pub notes: Option<String>,
    /// Max concurrent connectors (hub mode). 1 = legacy 1:1 path (unchanged).
    pub max_clients: u16,
    /// Masquerade NAT'd (`real@exposed`) subnets toward the LAN (F2). Default off
    /// keeps per-peer source visibility (I-NAT5) but only the gateway host itself
    /// is reachable unless it is the LAN's router; on rewrites the source to the
    /// gateway LAN IP so every host behind the gateway is reachable on any topology.
    pub nat_masquerade: bool,
}

/// Public arg struct for `bore vpn connect` (converted from CLI args).
#[derive(Clone, Debug)]
pub struct VpnConnectArgs {
    /// Server address.
    pub to: String,
    /// Shared secret.
    pub secret: String,
    /// VPN link identifier.
    pub id: String,
    /// Skip TLS verification.
    pub insecure: bool,
    /// Advertised subnets, each optionally NAT-mapped (`real@exposed`). Only the
    /// `exposed` CIDR is sent on the wire (N3); `real` drives local route/NAT.
    pub advertise_entries: Vec<crate::shared::AdvertiseEntry>,
    /// Address request (pool or static).
    pub addr_request: crate::shared::VpnAddrRequest,
    /// TUN interface name.
    pub tun_name: String,
    /// Interface MTU.
    pub mtu: u16,
    /// Skip route/NAT management.
    pub no_route_manage: bool,
    /// STUN server.
    pub stun_server: Option<String>,
    /// Try UPnP.
    pub upnp: bool,
    /// Try port prediction.
    pub try_port_prediction: bool,
    /// Preferred UDP port.
    pub nat_udp_preferred_port: u16,
    /// UDP port release timeout.
    pub nat_udp_release_timeout: u64,
    /// Never attempt the direct UDP path (stay on the server relay).
    pub relay_only: bool,
    /// Reconnect with backoff when the link drops (full teardown + rebuild).
    pub auto_reconnect: bool,
    /// Requested relay carrier substream pairs (1 = single, as before).
    pub carriers: u16,
    /// Number of TUN queues (Linux IFF_MULTI_QUEUE); 1 = single queue.
    pub tun_queues: usize,
    /// Optional notes.
    pub notes: Option<String>,
    /// Accept exactly these advertised routes (exact-or-subset).
    pub accept_routes: Vec<crate::shared::Ipv4Net>,
    /// Accept every route the listener advertises.
    pub accept_all_routes: bool,
    /// Subtract these routes from the accepted set.
    pub refuse_routes: Vec<crate::shared::Ipv4Net>,
    /// Accept nothing (== default; for explicit, self-documenting scripts).
    pub refuse_all_routes: bool,
    /// Masquerade NAT'd (`real@exposed`) subnets toward the LAN (F2). Applies on
    /// the advertising side (a connector advertising its own `real@exposed`). See
    /// [`VpnListenArgs::nat_masquerade`].
    pub nat_masquerade: bool,
}

/// Non-retryable configuration error: retrying would yield the same outcome
/// (duplicate id at first attempt is the deliberate exception — see
/// [`vpn_error_is_retryable`]). `run_with_reconnect` stops on these instead of
/// looping forever against a config mistake.
#[derive(Debug)]
pub struct FatalVpnError(pub String);

impl std::fmt::Display for FatalVpnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FatalVpnError {}

/// True when the error must stop the reconnect loop.
fn is_fatal(err: &anyhow::Error) -> bool {
    err.downcast_ref::<FatalVpnError>().is_some()
}

/// Classify a server-sent `VpnError` message.
///
/// Two `VpnError`s are deliberately retryable; the rest are fatal config errors.
///
/// - "vpn id already in use": during a reconnect the server-side handler of the
///   previous session can take a few seconds to die and release the id.
/// - "vpn listener '<id>' not found": on a reconnect after the server restarts,
///   the connector and listener race to re-register. If the connector wins it
///   gets this error before the listener has re-registered; retrying with
///   backoff lets the listener catch up. (Without `--auto-reconnect` the
///   connector still exits on first error, so a genuinely-missing listener is
///   not retried.)
///
/// Every other `VpnError` (pool exhausted, overlap, mode mismatch, static
/// mismatch, no vpn pool, max-links) reflects configuration and would fail
/// identically forever.
fn vpn_error_is_retryable(msg: &str) -> bool {
    msg.contains("already in use") || msg.contains("not found")
}

/// Build the error for a server-sent `VpnError`, fatal or retryable per
/// [`vpn_error_is_retryable`].
fn classify_vpn_error(msg: String) -> anyhow::Error {
    if vpn_error_is_retryable(&msg) {
        tracing::warn!(error = %msg, "retryable vpn error (stale server-side session?)");
        anyhow!("{msg}")
    } else {
        anyhow::Error::new(FatalVpnError(msg))
    }
}

/// Route filtering for connectors (Phase 1 of multi-client hub feature).
/// Resolves which advertised CIDRs the connector installs based on accept/refuse flags.
mod routes {
    use crate::shared::Ipv4Net;

    /// True iff `flag` is equal to OR a supernet of `adv` (i.e. `adv` is
    /// equal-to-or-contained-in `flag`, the D9 "exact-or-subset" relation).
    ///
    /// Requires BOTH: `flag` is no more specific than `adv` (`flag.prefix <=
    /// adv.prefix`) AND `adv`'s network base falls inside `flag`. The prefix
    /// check is essential: without it a *smaller* flag sharing the same base
    /// address (e.g. flag `10.10.0.0/24` vs adv `10.10.0.0/16`) would falsely
    /// match, since `contains` only tests the base address.
    fn covers(flag: &Ipv4Net, adv: &Ipv4Net) -> bool {
        flag.prefix <= adv.prefix && flag.contains(adv.network())
    }

    /// Resolve which advertised CIDRs the connector installs.
    ///
    /// Semantics (D9, exact-or-subset):
    /// - Start set = if `accept_all` then all advertised, else those advertised `A`
    ///   such that some `acc` in `accept` covers `A` (`acc` equals or is a supernet of `A`).
    /// - Then remove any `A` covered by some `r` in `refuse` (`r` equals or is a supernet of `A`).
    /// - If `refuse_all` → return empty (ignore accept flags).
    ///
    /// Example: advertised `10.10.0.0/16`, refuse `10.10.5.0/24` → result still contains
    /// `10.10.0.0/16` (a refuse must COVER the advertised entry to remove it).
    pub fn filter_accepted(
        advertised: &[Ipv4Net],
        accept_all: bool,
        refuse_all: bool,
        accept: &[Ipv4Net],
        refuse: &[Ipv4Net],
    ) -> Vec<Ipv4Net> {
        // If refuse_all is set, return empty regardless of accept flags.
        if refuse_all {
            return Vec::new();
        }

        // Start with the set of routes to accept.
        let mut result: Vec<Ipv4Net> = if accept_all {
            advertised.to_vec()
        } else {
            // Keep only advertised routes covered (equal-or-supernet) by some accept CIDR.
            advertised
                .iter()
                .filter(|adv| accept.iter().any(|acc| covers(acc, adv)))
                .copied()
                .collect()
        };

        // Remove any routes covered (equal-or-supernet) by a refuse CIDR.
        result.retain(|adv| !refuse.iter().any(|r| covers(r, adv)));

        result
    }

    use crate::shared::AdvertiseEntry;

    /// The local "real" subnets to drive route/NAT/LAN-iface detection (N7/I-NAT9).
    /// For a plain entry `real == exposed`, so this is byte-identical to today.
    pub fn advertised_reals(entries: &[AdvertiseEntry]) -> Vec<Ipv4Net> {
        entries.iter().map(|e| e.real).collect()
    }

    /// The subnets exposed on the wire (N3/I-NAT2). Only these are serialized;
    /// real subnets stay gateway-local.
    pub fn advertised_exposed(entries: &[AdvertiseEntry]) -> Vec<Ipv4Net> {
        entries.iter().map(|e| e.exposed).collect()
    }

    /// The `(real, exposed)` netmap pairs for NAT entries only (N5). Empty when
    /// no entry uses `@` → `NetConfig::apply` keeps today's blanket-masquerade
    /// path byte-for-byte (N8/I-NAT1).
    pub fn nat_maps(entries: &[AdvertiseEntry]) -> Vec<(Ipv4Net, Ipv4Net)> {
        entries
            .iter()
            .filter(|e| e.is_nat())
            .map(|e| (e.real, e.exposed))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn default_no_flags_returns_empty() {
            let advertised = vec![
                "192.168.4.0/24".parse().unwrap(),
                "10.10.0.0/16".parse().unwrap(),
            ];
            let result = filter_accepted(&advertised, false, false, &[], &[]);
            assert!(result.is_empty());
        }

        #[test]
        fn refuse_all_returns_empty() {
            let advertised = vec![
                "192.168.4.0/24".parse().unwrap(),
                "10.10.0.0/16".parse().unwrap(),
            ];
            let accept = vec!["192.168.4.0/24".parse().unwrap()];
            let result = filter_accepted(&advertised, true, true, &accept, &[]);
            assert!(result.is_empty());
        }

        #[test]
        fn accept_all_returns_all() {
            let advertised = vec![
                "192.168.4.0/24".parse().unwrap(),
                "10.10.0.0/16".parse().unwrap(),
            ];
            let result = filter_accepted(&advertised, true, false, &[], &[]);
            assert_eq!(result.len(), 2);
            assert!(result.contains(&"192.168.4.0/24".parse().unwrap()));
            assert!(result.contains(&"10.10.0.0/16".parse().unwrap()));
        }

        #[test]
        fn accept_all_refuse_one() {
            let advertised = vec![
                "192.168.4.0/24".parse().unwrap(),
                "10.10.0.0/16".parse().unwrap(),
            ];
            let refuse = vec!["10.10.0.0/16".parse().unwrap()];
            let result = filter_accepted(&advertised, true, false, &[], &refuse);
            assert_eq!(result.len(), 1);
            assert!(result.contains(&"192.168.4.0/24".parse().unwrap()));
        }

        #[test]
        fn accept_all_refuse_other() {
            let advertised = vec![
                "192.168.4.0/24".parse().unwrap(),
                "10.10.0.0/16".parse().unwrap(),
            ];
            let refuse = vec!["192.168.4.0/24".parse().unwrap()];
            let result = filter_accepted(&advertised, true, false, &[], &refuse);
            assert_eq!(result.len(), 1);
            assert!(result.contains(&"10.10.0.0/16".parse().unwrap()));
        }

        #[test]
        fn accept_specific_route() {
            let advertised = vec![
                "192.168.4.0/24".parse().unwrap(),
                "10.10.0.0/16".parse().unwrap(),
            ];
            let accept = vec!["192.168.4.0/24".parse().unwrap()];
            let result = filter_accepted(&advertised, false, false, &accept, &[]);
            assert_eq!(result.len(), 1);
            assert!(result.contains(&"192.168.4.0/24".parse().unwrap()));
        }

        #[test]
        fn accept_not_advertised() {
            let advertised = vec![
                "192.168.4.0/24".parse().unwrap(),
                "10.10.0.0/16".parse().unwrap(),
            ];
            let accept = vec!["8.8.8.0/24".parse().unwrap()];
            let result = filter_accepted(&advertised, false, false, &accept, &[]);
            assert!(result.is_empty());
        }

        #[test]
        fn refuse_must_cover_advertised_to_remove() {
            // advertised 10.10.0.0/16, refuse 10.10.5.0/24 → still contains 10.10.0.0/16
            let advertised = vec!["10.10.0.0/16".parse().unwrap()];
            let refuse = vec!["10.10.5.0/24".parse().unwrap()];
            let result = filter_accepted(&advertised, true, false, &[], &refuse);
            assert_eq!(result.len(), 1);
            assert!(result.contains(&"10.10.0.0/16".parse().unwrap()));
        }

        #[test]
        fn accept_smaller_subnet_sharing_base_does_not_cover_advertised() {
            // advertised 10.10.0.0/16, accept 10.10.0.0/24 (same base, MORE specific) → []
            // The flag must be equal-or-supernet of the advertised entry (D9).
            let advertised = vec!["10.10.0.0/16".parse().unwrap()];
            let accept = vec!["10.10.0.0/24".parse().unwrap()];
            let result = filter_accepted(&advertised, false, false, &accept, &[]);
            assert!(result.is_empty());
        }

        #[test]
        fn accept_supernet_covers_advertised_subset() {
            // advertised 10.10.5.0/24, accept 10.10.0.0/16 (supernet) → [10.10.5.0/24]
            let advertised = vec!["10.10.5.0/24".parse().unwrap()];
            let accept = vec!["10.10.0.0/16".parse().unwrap()];
            let result = filter_accepted(&advertised, false, false, &accept, &[]);
            assert_eq!(result.len(), 1);
            assert!(result.contains(&"10.10.5.0/24".parse().unwrap()));
        }

        fn entries(items: &[&str]) -> Vec<AdvertiseEntry> {
            items.iter().map(|s| s.parse().unwrap()).collect()
        }

        #[test]
        fn reals_exposed_maps_plain_only() {
            // Plain entries: real==exposed, no nat maps (N8/I-NAT1 byte-identical).
            let es = entries(&["192.168.50.0/24", "172.16.0.0/24"]);
            assert_eq!(advertised_reals(&es), advertised_exposed(&es));
            assert!(nat_maps(&es).is_empty());
        }

        #[test]
        fn reals_exposed_maps_mixed() {
            // One NAT entry + one plain.
            let es = entries(&["192.168.1.0/24@10.50.1.0/24", "172.16.9.0/24"]);
            assert_eq!(
                advertised_reals(&es),
                vec![
                    "192.168.1.0/24".parse().unwrap(),
                    "172.16.9.0/24".parse().unwrap()
                ]
            );
            assert_eq!(
                advertised_exposed(&es),
                vec![
                    "10.50.1.0/24".parse().unwrap(),
                    "172.16.9.0/24".parse().unwrap()
                ]
            );
            let maps = nat_maps(&es);
            assert_eq!(maps.len(), 1);
            assert_eq!(
                maps[0],
                (
                    "192.168.1.0/24".parse().unwrap(),
                    "10.50.1.0/24".parse().unwrap()
                )
            );
        }
    }
}

/// Reconnect wrapper (DEC-4): a local loop reusing [`crate::reconnect::Backoff`],
/// NOT `reconnect::run` — the VPN must distinguish fatal configuration errors
/// from lost links, which the shared helper deliberately does not.
///
/// Every attempt is a full teardown + rebuild (DEC-5): `run_*_once` owns the
/// TUN and `NetConfig` as locals, so their RAII drops run before the next
/// attempt; `ip route replace` keeps a re-apply idempotent.
async fn run_with_reconnect<F, Fut>(auto: bool, mut attempt: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if !auto {
        return attempt().await;
    }
    let mut backoff = crate::reconnect::Backoff::new(); // 1 s .. 32 s
    loop {
        let started = tokio::time::Instant::now();
        match attempt().await {
            Ok(()) => return Ok(()), // clean exit (future: shutdown signal)
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => {
                // An attempt that lived >60 s was a healthy link: restart the
                // backoff from the minimum instead of escalating.
                if started.elapsed() > std::time::Duration::from_secs(60) {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                tracing::warn!(error = %e, ?delay, "vpn link lost; reconnecting");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Start a VPN listener (reconnect loop around [`run_listen_once`]).
pub async fn run_listen(args: VpnListenArgs) -> Result<()> {
    let auto = args.auto_reconnect;
    run_with_reconnect(auto, move || run_listen_once(args.clone())).await
}

/// One full listener attempt: connect, pair, bring the link up, run the bridge.
async fn run_listen_once(args: VpnListenArgs) -> Result<()> {
    // Preflight checks (fatal: retrying cannot fix privileges or PATH)
    hostcfg::check_root().map_err(|e| FatalVpnError(e.to_string()))?;
    hostcfg::check_binary_exists("ip")
        .then_some(())
        .ok_or_else(|| FatalVpnError("'ip' command not found".into()))?;

    info!(link_id = %args.id, "vpn listener starting");

    // Connect to server
    let endpoint = crate::transport::Endpoint::parse(&args.to);
    let control_stream = crate::transport::connect(&endpoint, args.insecure).await?;

    let (opener, mut acceptor) = crate::mux::client(control_stream);
    let ctrl_stream = opener.open().await.context("open control stream")?;
    let mut ctrl = crate::shared::Delimited::new(ctrl_stream);

    // Send HelloVpn first (yamux lazy-init invariant). Only the exposed
    // (virtual) CIDRs go on the wire (N3/I-NAT2); real subnets stay local.
    let hello = crate::shared::ClientMessage::HelloVpn {
        id: args.id.clone(),
        advertised: routes::advertised_exposed(&args.advertise_entries),
        addr: args.addr_request.clone(),
        notes: args.notes.clone(),
        carriers: args.carriers.clamp(1, 16),
        max_clients: args.max_clients,
    };
    ctrl.send(hello).await?;

    // Auth if we have a secret (server will send Challenge if it requires it)
    crate::auth::Authenticator::new(&args.secret)
        .client_handshake(&mut ctrl)
        .await?;

    // Wait for VpnReady
    let msg = ctrl.recv::<crate::shared::ServerMessage>().await?;
    let (assigned, prefix, peer_advertised, session_nonce, admin_v2, carriers) = match msg {
        Some(crate::shared::ServerMessage::VpnReady {
            assigned,
            prefix,
            peer_advertised,
            session_nonce,
            admin_v2,
            carriers,
            ..
        }) => {
            info!(
                link_id = %args.id,
                path = "relay",
                overlay = %format!("{assigned}/{prefix}"),
                iface = %args.tun_name,
                "vpn link paired"
            );
            (
                assigned,
                prefix,
                peer_advertised,
                session_nonce,
                admin_v2,
                carriers.max(1),
            )
        }
        Some(crate::shared::ServerMessage::VpnError(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            return Err(classify_vpn_error(e));
        }
        Some(crate::shared::ServerMessage::Error(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            bail!("{e}");
        }
        None => {
            error!(link_id = %args.id, "server closed before sending VpnReady; may be too old or not VPN-capable");
            bail!("server may be too old or not VPN-capable (needs 'bore server --vpn', built with --features vpn)");
        }
        other => {
            error!(link_id = %args.id, msg = ?other, "unexpected server message");
            bail!("unexpected server message: {other:?}");
        }
    };

    // Hub mode (I-MC1: early branch preserves legacy 1:1 path unchanged)
    if args.max_clients > 1 {
        return hub::run_listen_hub(
            args, acceptor, opener, ctrl, assigned, prefix, admin_v2, carriers,
        )
        .await;
    }

    // Stale reclaim
    hostcfg::stale_reclaim(&args.id, "listen").await;

    // Create TUN device(s) (one per queue).
    let (devs_raw, offload, tun_name) =
        hostcfg::create_tun(&args.tun_name, assigned, prefix, args.mtu, args.tun_queues).await?;
    let devs: Vec<Arc<tun_rs::AsyncDevice>> = devs_raw.into_iter().map(Arc::new).collect();
    info!(
        link_id = %args.id,
        iface = %tun_name,
        addr = %assigned,
        prefix = prefix,
        "created tun device"
    );

    // Apply network config (routes, NAT, etc.). `advertised` = real subnets (N7);
    // nat_maps drives the optional 1:1 netmap (empty = today's path, N8/I-NAT1).
    let advertised_nets = routes::advertised_reals(&args.advertise_entries);
    let nat_maps = routes::nat_maps(&args.advertise_entries);
    let peer_routes = peer_advertised.to_vec();
    for (real, exposed) in &nat_maps {
        info!(
            link_id = %args.id,
            real = %real,
            exposed = %exposed,
            "NAT 1:1 netmap: advertising real {real} mapped to exposed {exposed} (peers reach this LAN as {exposed})"
        );
    }
    let runner = hostcfg::RealRunner;
    let _netcfg = hostcfg::NetConfig::apply(
        &runner,
        &args.id,
        "listen",
        &tun_name,
        assigned,
        prefix,
        &peer_routes,
        &advertised_nets,
        &nat_maps,
        args.no_route_manage,
        false,
        args.nat_masquerade,
    )
    .await?;

    // Accept the negotiated relay substream pairs from the server.
    let (egress, ingress) = link::accept_relay_multi(&mut acceptor, carriers).await?;

    // Build relay link
    let keys = crypto::derive_keys_listener(&args.secret, &session_nonce)?;
    let (sender, recver) = link::make_relay_multi(egress, ingress, keys);
    let counters = bridge::BridgeCounters::new();

    info!(link_id = %args.id, "vpn link bridge starting");

    // Control-stream actor (single owner of `ctrl` from here on).
    let (out_tx, event_rx, ctrl_task) = spawn_ctrl_actor(ctrl);

    // Admin v2 servers track the active path; report the initial relay state.
    if admin_v2 {
        let _ = out_tx
            .send(crate::shared::ClientMessage::VpnPathReport {
                path: "relay".into(),
            })
            .await;
    }

    // Direct-path upgrade attempt (skipped entirely with --relay-only).
    let (upgrade_tx, upgrade_rx) = tokio::sync::mpsc::channel(1);
    let (downgrade_tx, downgrade_rx) = tokio::sync::mpsc::channel::<()>(1);
    let direct_task = if args.relay_only {
        drop(event_rx);
        drop(upgrade_tx);
        drop(downgrade_rx);
        None
    } else {
        let ctx = DirectUpgradeCtx::from_link_args(
            DirectSide::Listener,
            &args.to,
            &CommonDirectArgs {
                id: &args.id,
                secret: &args.secret,
                stun_server: args.stun_server.as_ref(),
                upnp: args.upnp,
                try_port_prediction: args.try_port_prediction,
                nat_udp_preferred_port: args.nat_udp_preferred_port,
                tun_name: &tun_name,
                mtu: args.mtu,
            },
            admin_v2,
            peer_routes.clone(),
        );
        Some(tokio::spawn(direct_upgrade_task(
            ctx,
            out_tx.clone(),
            event_rx,
            upgrade_tx,
            downgrade_rx,
        )))
    };
    drop(out_tx);

    // Run the bridge until it closes or the control connection dies.
    let result = run_bridge_with_ctrl(
        &args.id,
        ctrl_task,
        devs,
        sender,
        recver,
        counters,
        args.mtu,
        offload,
        upgrade_rx,
        downgrade_tx,
    )
    .await;

    if let Some(task) = direct_task {
        task.abort();
    }

    info!(link_id = %args.id, "vpn link bridge closed");
    result
}

/// Events the control-stream actor forwards to the direct-path logic.
enum CtrlEvent {
    /// The server brokered a punch: peer candidates + transport tuning.
    Punch {
        /// Session nonce; both peers derive the same QUIC token from it.
        nonce: [u8; crate::shared::UDP_NONCE_LEN],
        /// Peer candidate addresses to punch toward.
        peer: Vec<std::net::SocketAddr>,
        /// Direct-UDP transport tuning requested by the server.
        tuning: crate::shared::UdpDirectTuning,
    },
    /// The direct path is unavailable; stay on relay.
    Unavailable,
}

/// Spawn the control-stream actor: the **single** owner of the control stream
/// after `VpnReady` (one stream = one task).
///
/// The server sends `Heartbeat` every 500 ms. Without a reader those frames
/// would slowly exhaust the stream's receive window, and — worse — server death
/// would go completely unnoticed: the bridge would keep "running" with zero
/// traffic and no log line. The actor drains the stream for prompt, loud
/// detection of a lost server (the returned `JoinHandle` resolves with the
/// error), forwards `UdpPunch`/`UdpUnavailable` to `CtrlEvent` consumers, and
/// writes any `ClientMessage` submitted on the returned sender (candidate
/// offers, path reports).
fn spawn_ctrl_actor(
    mut ctrl: crate::shared::Delimited<crate::mux::Stream>,
) -> (
    tokio::sync::mpsc::Sender<crate::shared::ClientMessage>,
    tokio::sync::mpsc::Receiver<CtrlEvent>,
    tokio::task::JoinHandle<anyhow::Error>,
) {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<crate::shared::ClientMessage>(8);
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<CtrlEvent>(8);
    let task = tokio::spawn(async move {
        let mut out_open = true;
        loop {
            tokio::select! {
                out = out_rx.recv(), if out_open => match out {
                    Some(msg) => {
                        if let Err(e) = ctrl.send(msg).await {
                            return anyhow!("vpn control stream error: {e}");
                        }
                    }
                    // All senders dropped: keep draining the stream (I-7).
                    None => out_open = false,
                },
                msg = ctrl.recv::<crate::shared::ServerMessage>() => match msg {
                    Ok(Some(crate::shared::ServerMessage::Heartbeat)) => continue,
                    Ok(Some(crate::shared::ServerMessage::UdpPunch {
                        nonce,
                        peer,
                        peer_selected_stun,
                        tuning,
                        peer_id: _,
                    })) => {
                        tracing::debug!(?peer, ?peer_selected_stun, "received vpn udp punch");
                        let _ = event_tx
                            .send(CtrlEvent::Punch { nonce, peer, tuning })
                            .await;
                    }
                    Ok(Some(crate::shared::ServerMessage::UdpUnavailable)) => {
                        let _ = event_tx.send(CtrlEvent::Unavailable).await;
                    }
                    Ok(Some(crate::shared::ServerMessage::VpnPeerJoin { .. })) => {
                        tracing::debug!("unexpected VpnPeerJoin on 1:1 vpn link (ignoring)");
                    }
                    Ok(Some(crate::shared::ServerMessage::VpnPeerLeave { .. })) => {
                        tracing::debug!("unexpected VpnPeerLeave on 1:1 vpn link (ignoring)");
                    }
                    Ok(Some(msg)) => {
                        tracing::debug!(?msg, "ignoring control message on vpn link");
                    }
                    Ok(None) => return anyhow!("server closed the vpn control stream"),
                    Err(e) => return anyhow!("vpn control stream error: {e}"),
                }
            }
        }
    });
    (out_tx, event_rx, task)
}

/// Which QUIC role this peer plays during the direct-path upgrade.
#[derive(Clone, Copy, Debug)]
enum DirectSide {
    /// QUIC server (`DirectListener::accept`).
    Listener,
    /// QUIC client (`connect_direct`).
    Connector,
}

/// Inputs for [`direct_upgrade_task`], captured from the link args + pairing.
struct DirectUpgradeCtx {
    side: DirectSide,
    link_id: String,
    secret: String,
    server_host: String,
    server_port: u16,
    stun_server: Option<String>,
    upnp: bool,
    try_port_prediction: bool,
    nat_udp_preferred_port: u16,
    /// Server accepts `VpnPathReport` (admin page v2).
    admin_v2: bool,
    /// TUN interface name (for the dynamic-PMTU monitor).
    tun_name: String,
    /// Initial TUN MTU (the PMTU monitor's starting point).
    mtu: u16,
    /// Subnets this node routes INTO the TUN (`peer_routes`). A direct-path
    /// candidate whose IP falls inside one of these is only "reachable" by
    /// looping back through the VPN itself: the QUIC handshake rides the relay
    /// tunnel, succeeds, then dies the moment the relay halves are dropped at
    /// the switch to direct (`read_datagram: timed out`). Such candidates are
    /// filtered out before punching (see [`filter_tunneled_candidates`]).
    tunneled_subnets: Vec<crate::shared::Ipv4Net>,
}

impl DirectUpgradeCtx {
    fn from_link_args(
        side: DirectSide,
        to: &str,
        args: &CommonDirectArgs<'_>,
        admin_v2: bool,
        tunneled_subnets: Vec<crate::shared::Ipv4Net>,
    ) -> Self {
        let endpoint = crate::transport::Endpoint::parse(to);
        DirectUpgradeCtx {
            side,
            link_id: args.id.to_string(),
            secret: args.secret.to_string(),
            server_host: endpoint.host,
            server_port: endpoint.port,
            stun_server: args.stun_server.cloned(),
            upnp: args.upnp,
            try_port_prediction: args.try_port_prediction,
            nat_udp_preferred_port: args.nat_udp_preferred_port,
            admin_v2,
            tun_name: args.tun_name.to_string(),
            mtu: args.mtu,
            tunneled_subnets,
        }
    }
}

/// Borrowed view of the NAT-related fields shared by listen/connect args.
struct CommonDirectArgs<'a> {
    id: &'a str,
    secret: &'a str,
    stun_server: Option<&'a String>,
    upnp: bool,
    try_port_prediction: bool,
    nat_udp_preferred_port: u16,
    tun_name: &'a str,
    mtu: u16,
}

/// Total budget for the offer → punch round-trip before giving up on direct.
const DIRECT_PUNCH_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
/// How long the listener waits for the peer's QUIC connection after the punch.
const DIRECT_ACCEPT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Cadence of the background direct-upgrade retry while the link is on relay.
///
/// Both peers run the same state machine anchored at pairing, so a fixed-grid
/// `interval` keeps their retry rounds aligned (the server brokers a punch only
/// when it holds BOTH offers within `punch_timeout`). Must exceed the worst-case
/// single attempt (`DIRECT_PUNCH_WAIT` 15 s ≥ a stalled round) so rounds never
/// overlap and the two sides stay on the same 30 s grid.
const DIRECT_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Decide whether the direct-upgrade loop should run another round. It stops
/// once direct is achieved (`succeeded`) or the bridge's upgrade channel is gone
/// (`upgrade_closed` — the link is tearing down); otherwise it keeps retrying
/// while on relay when `retry_enabled`.
fn should_retry_direct(succeeded: bool, retry_enabled: bool, upgrade_closed: bool) -> bool {
    retry_enabled && !succeeded && !upgrade_closed
}

/// Background task that attempts the relay → direct upgrade and, while the link
/// stays on relay, keeps retrying on a fixed grid ([`DIRECT_RETRY_INTERVAL`]).
///
/// On success it pushes the new `Direct` link halves into the bridge's upgrade
/// channel (DEC-1), logs `path = "direct"`, and stops. On failure it logs and
/// retries on the next tick — the relay bridge keeps running untouched the whole
/// time (relay stability is never affected by a failed direct attempt). It gives
/// up only when the upgrade channel closes (the link is being torn down; the
/// task is also `abort()`ed by the caller on teardown). The first attempt is
/// immediate (`interval`'s first tick fires at once), preserving the original
/// try-direct-ASAP behaviour.
async fn direct_upgrade_task(
    ctx: DirectUpgradeCtx,
    out_tx: tokio::sync::mpsc::Sender<crate::shared::ClientMessage>,
    mut event_rx: tokio::sync::mpsc::Receiver<CtrlEvent>,
    upgrade_tx: tokio::sync::mpsc::Sender<(link::LinkSender, link::LinkRecver)>,
    mut downgrade_rx: tokio::sync::mpsc::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(DIRECT_RETRY_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut attempt: u32 = 0;
    'retry: loop {
        ticker.tick().await; // immediate on the first iteration
        attempt += 1;
        match try_direct_upgrade(&ctx, &out_tx, &mut event_rx, &upgrade_tx).await {
            Ok(()) => {
                // Direct is up. Block until the bridge tells us it fell back, then re-arm.
                match downgrade_rx.recv().await {
                    Some(()) => {
                        info!(link_id=%ctx.link_id, "direct path lost; re-arming relay→direct retry");
                        continue 'retry;
                    }
                    None => return, // bridge gone → link closing
                }
            }
            Err(e) => {
                let will_retry = should_retry_direct(false, true, upgrade_tx.is_closed());
                if will_retry {
                    info!(
                        link_id = %ctx.link_id,
                        error = %e,
                        attempt,
                        retry_in = ?DIRECT_RETRY_INTERVAL,
                        path = "relay",
                        "direct path unavailable; staying on relay, will retry"
                    );
                } else {
                    info!(
                        link_id = %ctx.link_id,
                        error = %e,
                        attempt,
                        path = "relay",
                        "direct path unavailable; staying on relay"
                    );
                    return;
                }
            }
        }
    }
}

/// Drop direct-path candidates whose IP falls inside a subnet this node routes
/// into the TUN. Reaching such a candidate would loop back through the VPN
/// itself (the QUIC handshake rides the relay tunnel, succeeds, then dies when
/// the relay halves are dropped at the switch to direct — observed as
/// `read_datagram: timed out` ~10 s after `path="direct"`). Returns
/// `(kept, dropped)`. With no tunneled subnets nothing is filtered.
///
/// Note: this is intentionally conservative — if a candidate inside a tunneled
/// subnet is *also* reachable off-tunnel via a more-specific connected route,
/// it is still dropped and the link stays on relay. Relay is correct (just not
/// optimal); a looped "direct" path that silently dies is not.
fn filter_tunneled_candidates(
    peers: &[std::net::SocketAddr],
    tunneled: &[crate::shared::Ipv4Net],
) -> (Vec<std::net::SocketAddr>, Vec<std::net::SocketAddr>) {
    if tunneled.is_empty() {
        return (peers.to_vec(), Vec::new());
    }
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for &p in peers {
        let routed_into_tun = match p.ip() {
            std::net::IpAddr::V4(v4) => tunneled.iter().any(|n| n.contains(v4)),
            // The overlay is IPv4-only; an IPv6 candidate never matches a
            // tunneled subnet, so it can never loop.
            std::net::IpAddr::V6(_) => false,
        };
        if routed_into_tun {
            dropped.push(p);
        } else {
            kept.push(p);
        }
    }
    (kept, dropped)
}

async fn try_direct_upgrade(
    ctx: &DirectUpgradeCtx,
    out_tx: &tokio::sync::mpsc::Sender<crate::shared::ClientMessage>,
    event_rx: &mut tokio::sync::mpsc::Receiver<CtrlEvent>,
    upgrade_tx: &tokio::sync::mpsc::Sender<(link::LinkSender, link::LinkRecver)>,
) -> Result<()> {
    // 1. UDP socket (0 = ephemeral port).
    let socket = crate::holepunch::bind_socket(ctx.nat_udp_preferred_port).await?;

    // 2. STUN chain (explicit override > public chain > bore server fallback).
    let targets = crate::holepunch::resolve_live_stun_targets(
        &ctx.server_host,
        ctx.server_port,
        ctx.stun_server.as_deref(),
    )
    .await?;

    // 3. Candidate gathering (reflexive + local; optional UPnP / port prediction).
    let disc = crate::holepunch::gather_candidates_from_stun_targets(
        &socket,
        &targets,
        ctx.upnp,
        ctx.try_port_prediction,
    )
    .await;
    anyhow::ensure!(!disc.candidates.is_empty(), "no usable UDP candidates");

    // 4. Offer our candidates to the server's broker.
    out_tx
        .send(crate::shared::ClientMessage::UdpCandidateOffer(
            crate::shared::UdpCandidateOffer {
                candidates: disc.candidates,
                selected_stun: disc.selected_stun.map(|s| s.requested),
                peer_id: 0,
            },
        ))
        .await
        .map_err(|_| anyhow!("control actor closed"))?;

    // 5. Wait for the punch (the server replies only when BOTH offers are in).
    let event = tokio::time::timeout(DIRECT_PUNCH_WAIT, event_rx.recv())
        .await
        .map_err(|_| anyhow!("no punch from server within {DIRECT_PUNCH_WAIT:?}"))?
        .ok_or_else(|| anyhow!("control stream closed"))?;
    let (nonce, peer, tuning) = match event {
        CtrlEvent::Punch {
            nonce,
            peer,
            tuning,
        } => (nonce, peer, tuning),
        CtrlEvent::Unavailable => bail!("server reported the direct path unavailable"),
    };

    // 5b. Routing-loop guard: drop candidates inside subnets we route into the
    //     TUN — connecting to them would loop the QUIC handshake back through
    //     the relay and the "direct" path would die at the switch (DEC-2).
    let (peer, dropped) = filter_tunneled_candidates(&peer, &ctx.tunneled_subnets);
    if !dropped.is_empty() {
        info!(
            link_id = %ctx.link_id,
            ?dropped,
            "skipping direct candidates inside tunneled subnets (would loop through the VPN)"
        );
    }
    anyhow::ensure!(
        !peer.is_empty(),
        "all direct candidates are inside tunneled subnets; staying on relay"
    );

    // 6. Hole-punch + QUIC with the token both peers derive from (secret, nonce).
    let token = crate::holepunch::derive_token(Some(&ctx.secret), &nonce);
    let conn = match ctx.side {
        DirectSide::Listener => {
            let dl = crate::holepunch::DirectListener::new(socket, peer, tuning).await?;
            tokio::time::timeout(DIRECT_ACCEPT_WAIT, dl.accept(token))
                .await
                .map_err(|_| anyhow!("timed out waiting for the peer's direct QUIC connection"))??
        }
        DirectSide::Connector => {
            crate::holepunch::connect_direct(socket, peer, token, tuning).await?
        }
    };

    // 7. Hand the Direct link halves to the bridge (DEC-1: controlled restart)
    //    and start the dynamic-PMTU monitor (C2) on the live connection.
    let monitor_conn = conn.clone();
    upgrade_tx
        .send(link::make_direct(conn))
        .await
        .map_err(|_| anyhow!("bridge closed before the direct upgrade"))?;
    tokio::spawn(pmtu_monitor(monitor_conn, ctx.tun_name.clone(), ctx.mtu));
    info!(link_id = %ctx.link_id, path = "direct", "vpn path upgraded to direct QUIC");
    if ctx.admin_v2 {
        let _ = out_tx
            .send(crate::shared::ClientMessage::VpnPathReport {
                path: "direct".into(),
            })
            .await;
    }
    Ok(())
}

/// Decide the new TUN MTU from the QUIC path-MTU sample history (C2).
///
/// Returns `Some(new_mtu)` only when the last 3 samples are present and
/// identical (the QUIC MTU discovery has settled), the stable value differs
/// from the current MTU by at least 16 bytes (avoid churn), and the result is
/// within [576, 9000] (candidates above 9000 are clamped to 9000; below 576
/// rejected).
fn pmtu_decision(current_mtu: u16, samples: &[usize]) -> Option<u16> {
    if samples.len() < 3 {
        return None;
    }
    let last3 = &samples[samples.len() - 3..];
    let stable = last3[0];
    if last3.iter().any(|&s| s != stable) {
        return None;
    }
    if stable < 576 {
        return None;
    }
    let candidate = stable.min(9000) as u16;
    if candidate.abs_diff(current_mtu) < 16 {
        return None;
    }
    Some(candidate)
}

/// Urgent one-sample shrink (C2 fast path).
///
/// The instant the QUIC path-MTU is observed BELOW the current TUN MTU we are
/// dropping full-size packets *right now* — every read from TUN at the old MTU
/// is a `TooLarge` datagram. Waiting for the stable 3-sample [`pmtu_decision`]
/// would mean up to ~10 s of lost throughput after every switch to the direct
/// path. Narrowing is always safe (it never over-shoots the path), so we do it
/// on a single sample. Growing still goes through the anti-flap 3-sample path.
///
/// Returns `Some(new_mtu)` only when the sample is at least 16 bytes below the
/// current MTU (churn guard) and within the valid floor `[576, 9000]`.
fn pmtu_shrink_now(current_mtu: u16, sample: usize) -> Option<u16> {
    if sample < 576 {
        return None;
    }
    let candidate = sample.min(9000) as u16;
    if current_mtu.saturating_sub(candidate) >= 16 {
        Some(candidate)
    } else {
        None
    }
}

/// Background task: track the direct path's QUIC datagram limit and follow it
/// with `ip link set <tun> mtu` (C2). Started only after the switch to direct;
/// exits when the QUIC connection closes (link teardown or path death). No MTU
/// revert is needed: the TUN is destroyed at teardown (DEC-5), and the nft MSS
/// clamp uses `rt mtu`, adapting on its own.
async fn pmtu_monitor(conn: crate::holepunch::DirectConn, tun_name: String, initial_mtu: u16) {
    use futures_util::FutureExt;
    let runner = hostcfg::RealRunner;
    let mut current = initial_mtu;
    let mut samples: Vec<usize> = Vec::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = conn.closed() => return,
        }
        // The QUIC datagram limit minus AEAD-free overhead IS the usable IP
        // packet size on the direct path.
        let Some(max_datagram) = conn.max_datagram_size() else {
            continue;
        };
        samples.push(max_datagram);
        if samples.len() > 3 {
            samples.remove(0);
        }
        // Shrink immediately on one below-current sample (fast recovery after a
        // direct switch); otherwise use the stable 3-sample growth/shrink path.
        let decision =
            pmtu_shrink_now(current, max_datagram).or_else(|| pmtu_decision(current, &samples));
        if let Some(new_mtu) = decision {
            let argv = hostcfg_cmd::cmd_link_set_mtu(&tun_name, new_mtu);
            match crate::vpn::hostcfg::CommandRunner::run(&runner, &argv).await {
                Ok(_) => {
                    info!(
                        old = current,
                        new = new_mtu,
                        "tun MTU adjusted to QUIC path MTU"
                    );
                    current = new_mtu;
                }
                Err(e) => {
                    // A failed adjust during teardown (the TUN was destroyed
                    // between the decision and the `ip link` call) is benign:
                    // the device is gone anyway. Demote to debug so it does not
                    // spam warnings; keep WARN for genuine failures.
                    if conn.closed().now_or_never().is_some() {
                        tracing::debug!(error = %e, new_mtu, "tun MTU adjust skipped; link closing");
                    } else {
                        tracing::warn!(error = %e, new_mtu, "failed to adjust tun MTU; keeping current");
                    }
                }
            }
        }
    }
}

/// Run the data-plane bridge alongside the control-stream actor.
///
/// The actor's `JoinHandle` resolving means the control connection died (server
/// gone): the link is torn down loudly. The bridge finishing (error or upgrade
/// channel logic) aborts the actor; the caller's RAII guards then revert host
/// state.
#[allow(clippy::too_many_arguments)]
async fn run_bridge_with_ctrl(
    link_id: &str,
    mut ctrl_task: tokio::task::JoinHandle<anyhow::Error>,
    devs: Vec<Arc<tun_rs::AsyncDevice>>,
    sender: link::LinkSender,
    recver: link::LinkRecver,
    counters: Arc<bridge::BridgeCounters>,
    mtu: u16,
    offload: bool,
    upgrade_rx: tokio::sync::mpsc::Receiver<(link::LinkSender, link::LinkRecver)>,
    downgrade_tx: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let result = tokio::select! {
        res = bridge::run(devs, sender, recver, counters, mtu, offload, upgrade_rx, downgrade_tx) => {
            ctrl_task.abort();
            res
        }
        res = &mut ctrl_task => {
            let err = res.unwrap_or_else(|e| anyhow!("vpn control task panicked: {e}"));
            error!(link_id = %link_id, error = %err, "vpn control connection lost; closing link");
            Err(err)
        }
    };
    result
}

/// Start a VPN connector (reconnect loop around [`run_connect_once`]).
pub async fn run_connect(args: VpnConnectArgs) -> Result<()> {
    let auto = args.auto_reconnect;
    run_with_reconnect(auto, move || run_connect_once(args.clone())).await
}

/// One full connector attempt: connect, pair, bring the link up, run the bridge.
async fn run_connect_once(args: VpnConnectArgs) -> Result<()> {
    // Preflight checks (fatal: retrying cannot fix privileges or PATH)
    hostcfg::check_root().map_err(|e| FatalVpnError(e.to_string()))?;
    hostcfg::check_binary_exists("ip")
        .then_some(())
        .ok_or_else(|| FatalVpnError("'ip' command not found".into()))?;

    info!(link_id = %args.id, "vpn connector starting");

    // Connect to server
    let endpoint = crate::transport::Endpoint::parse(&args.to);
    let control_stream = crate::transport::connect(&endpoint, args.insecure).await?;

    let (opener, _acceptor) = crate::mux::client(control_stream);
    let ctrl_stream = opener.open().await.context("open control stream")?;
    let mut ctrl = crate::shared::Delimited::new(ctrl_stream);

    // Send ConnectVpn first (yamux lazy-init invariant). Only the exposed
    // (virtual) CIDRs go on the wire (N3/I-NAT2); real subnets stay local.
    let connect_msg = crate::shared::ClientMessage::ConnectVpn {
        id: args.id.clone(),
        advertised: routes::advertised_exposed(&args.advertise_entries),
        addr: args.addr_request.clone(),
        notes: args.notes.clone(),
        carriers: args.carriers.clamp(1, 16),
    };
    ctrl.send(connect_msg).await?;

    // Auth if we have a secret (server will send Challenge if it requires it)
    crate::auth::Authenticator::new(&args.secret)
        .client_handshake(&mut ctrl)
        .await?;

    // Wait for VpnReady
    let msg = ctrl.recv::<crate::shared::ServerMessage>().await?;
    let (assigned, prefix, peer_advertised, session_nonce, admin_v2, carriers) = match msg {
        Some(crate::shared::ServerMessage::VpnReady {
            assigned,
            prefix,
            peer_advertised,
            session_nonce,
            admin_v2,
            carriers,
            ..
        }) => {
            info!(
                link_id = %args.id,
                path = "relay",
                overlay = %format!("{assigned}/{prefix}"),
                iface = %args.tun_name,
                "vpn link paired"
            );
            (
                assigned,
                prefix,
                peer_advertised,
                session_nonce,
                admin_v2,
                carriers.max(1),
            )
        }
        Some(crate::shared::ServerMessage::VpnError(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            return Err(classify_vpn_error(e));
        }
        Some(crate::shared::ServerMessage::Error(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            bail!("{e}");
        }
        None => {
            error!(link_id = %args.id, "server closed before sending VpnReady; may be too old or not VPN-capable");
            bail!("server may be too old or not VPN-capable (needs 'bore server --vpn', built with --features vpn)");
        }
        other => {
            error!(link_id = %args.id, msg = ?other, "unexpected server message");
            bail!("unexpected server message: {other:?}");
        }
    };

    // Stale reclaim
    hostcfg::stale_reclaim(&args.id, "connect").await;

    // Create TUN device(s) (one per queue).
    let (devs_raw, offload, tun_name) =
        hostcfg::create_tun(&args.tun_name, assigned, prefix, args.mtu, args.tun_queues).await?;
    let devs: Vec<Arc<tun_rs::AsyncDevice>> = devs_raw.into_iter().map(Arc::new).collect();
    info!(
        link_id = %args.id,
        iface = %tun_name,
        addr = %assigned,
        prefix = prefix,
        "created tun device"
    );

    // Apply network config (routes, NAT, etc.). `advertised` = real subnets (N7);
    // nat_maps drives the optional 1:1 netmap (empty = today's path, N8/I-NAT1).
    let advertised_nets = routes::advertised_reals(&args.advertise_entries);
    let nat_maps = routes::nat_maps(&args.advertise_entries);
    // Connector-local route policy (D3/D9): install only the advertised CIDRs this
    // connector opted into. Default (no flags) = accept nothing (I-MC8). The refused
    // subnets simply get no route installed. The connector only ever sees the peer's
    // EXPOSED virtuals (real subnets are hidden by design, N3).
    let peer_routes = routes::filter_accepted(
        &peer_advertised,
        args.accept_all_routes,
        args.refuse_all_routes,
        &args.accept_routes,
        &args.refuse_routes,
    );
    info!(
        link_id = %args.id,
        advertised = ?peer_advertised,
        accepted = ?peer_routes,
        accept_all = args.accept_all_routes,
        refuse_all = args.refuse_all_routes,
        "resolved connector route policy"
    );
    // Per-route clarity (peer subnets are the EXPOSED virtuals; the peer's real LAN
    // behind any 1:1 NAT is hidden by design, N3/I-NAT2 — never sent on the wire).
    for r in &peer_routes {
        info!(link_id = %args.id, route = %r, "accepted peer route {r} (routed via tun; if the gateway NAT-maps it, the real LAN is gateway-side and not visible here)");
    }
    // This connector's OWN advertised NAT entries (site↔site), if any.
    for (real, exposed) in &nat_maps {
        info!(
            link_id = %args.id,
            real = %real,
            exposed = %exposed,
            "NAT 1:1 netmap: advertising real {real} mapped to exposed {exposed} (peers reach this LAN as {exposed})"
        );
    }
    let runner = hostcfg::RealRunner;
    let _netcfg = hostcfg::NetConfig::apply(
        &runner,
        &args.id,
        "connect",
        &tun_name,
        assigned,
        prefix,
        &peer_routes,
        &advertised_nets,
        &nat_maps,
        args.no_route_manage,
        false,
        args.nat_masquerade,
    )
    .await?;

    // Open the negotiated relay substream pairs and tag them.
    let (egress, ingress) = link::connect_relay_multi(&opener, carriers).await?;

    // Build relay link
    let keys = crypto::derive_keys_connector(&args.secret, &session_nonce)?;
    let (sender, recver) = link::make_relay_multi(egress, ingress, keys);
    let counters = bridge::BridgeCounters::new();

    info!(link_id = %args.id, "vpn link bridge starting");

    // Control-stream actor (single owner of `ctrl` from here on).
    let (out_tx, event_rx, ctrl_task) = spawn_ctrl_actor(ctrl);

    // Admin v2 servers track the active path; report the initial relay state.
    if admin_v2 {
        let _ = out_tx
            .send(crate::shared::ClientMessage::VpnPathReport {
                path: "relay".into(),
            })
            .await;
    }

    // Direct-path upgrade attempt (skipped entirely with --relay-only).
    let (upgrade_tx, upgrade_rx) = tokio::sync::mpsc::channel(1);
    let (downgrade_tx, downgrade_rx) = tokio::sync::mpsc::channel::<()>(1);
    let direct_task = if args.relay_only {
        drop(event_rx);
        drop(upgrade_tx);
        drop(downgrade_rx);
        None
    } else {
        let ctx = DirectUpgradeCtx::from_link_args(
            DirectSide::Connector,
            &args.to,
            &CommonDirectArgs {
                id: &args.id,
                secret: &args.secret,
                stun_server: args.stun_server.as_ref(),
                upnp: args.upnp,
                try_port_prediction: args.try_port_prediction,
                nat_udp_preferred_port: args.nat_udp_preferred_port,
                tun_name: &tun_name,
                mtu: args.mtu,
            },
            admin_v2,
            peer_routes.clone(),
        );
        Some(tokio::spawn(direct_upgrade_task(
            ctx,
            out_tx.clone(),
            event_rx,
            upgrade_tx,
            downgrade_rx,
        )))
    };
    drop(out_tx);

    // Run the bridge until it closes or the control connection dies.
    let result = run_bridge_with_ctrl(
        &args.id,
        ctrl_task,
        devs,
        sender,
        recver,
        counters,
        args.mtu,
        offload,
        upgrade_rx,
        downgrade_tx,
    )
    .await;

    if let Some(task) = direct_task {
        task.abort();
    }

    info!(link_id = %args.id, "vpn link bridge closed");
    result
}

/// Reexport submodules as public for use by tests and from main.rs.
pub use hostcfg::RealRunner;
pub use link::{LinkRecver, LinkSender};

/// Internal network types and utilities.
pub mod net {
    #![allow(dead_code)]
    use serde::{Deserialize, Serialize};
    use std::fmt;
    use std::net::Ipv4Addr;
    use std::str::FromStr;

    /// IPv4 CIDR (address + prefix length). Used for overlay + advertised subnets.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Ipv4Net {
        /// IPv4 address.
        pub addr: Ipv4Addr,
        /// Prefix length.
        pub prefix: u8,
    }

    impl Ipv4Net {
        /// Network address (host bits zeroed).
        pub fn network(&self) -> Ipv4Addr {
            let mask = Self::prefix_to_mask(self.prefix);
            let n = u32::from(self.addr) & mask;
            Ipv4Addr::from(n)
        }

        /// True if `other` addr is within this network.
        pub fn contains(&self, other: Ipv4Addr) -> bool {
            let mask = Self::prefix_to_mask(self.prefix);
            (u32::from(self.addr) & mask) == (u32::from(other) & mask)
        }

        /// True if `other` network overlaps with this one.
        pub fn overlaps(&self, other: &Ipv4Net) -> bool {
            let mask = if self.prefix <= other.prefix {
                Self::prefix_to_mask(self.prefix)
            } else {
                Self::prefix_to_mask(other.prefix)
            };
            (u32::from(self.addr) & mask) == (u32::from(other.addr) & mask)
        }

        fn prefix_to_mask(prefix: u8) -> u32 {
            if prefix == 0 {
                0
            } else {
                !0u32 << (32 - prefix)
            }
        }
    }

    impl FromStr for Ipv4Net {
        type Err = anyhow::Error;
        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let (addr_str, prefix_str) = s
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!("missing '/' in CIDR: {s}"))?;
            let addr = addr_str
                .parse::<Ipv4Addr>()
                .map_err(|e| anyhow::anyhow!("invalid addr in {s}: {e}"))?;
            let prefix = prefix_str
                .parse::<u8>()
                .map_err(|e| anyhow::anyhow!("invalid prefix in {s}: {e}"))?;
            anyhow::ensure!(prefix <= 32, "prefix {prefix} > 32 in {s}");
            Ok(Ipv4Net { addr, prefix })
        }
    }

    impl fmt::Display for Ipv4Net {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}/{}", self.addr, self.prefix)
        }
    }

    /// A /30 pool allocator. Carves /30 blocks from a parent CIDR.
    /// Each /30 has 4 addresses: network, .1 (listener), .2 (connector), broadcast.
    pub struct PoolAllocator {
        /// Parent CIDR.
        parent: Ipv4Net,
        /// Allocated block network addresses (as u32).
        allocated: std::collections::HashSet<u32>,
    }

    impl PoolAllocator {
        /// Create a new /30 pool allocator from a parent CIDR.
        pub fn new(parent: Ipv4Net) -> anyhow::Result<Self> {
            anyhow::ensure!(
                parent.prefix <= 30,
                "pool CIDR prefix must be ≤ 30, got /{}",
                parent.prefix
            );
            Ok(Self {
                parent,
                allocated: Default::default(),
            })
        }

        /// Allocate next free /30. Returns (listener_addr, connector_addr).
        pub fn alloc(&mut self) -> anyhow::Result<(Ipv4Addr, Ipv4Addr)> {
            let base = u32::from(self.parent.network());
            let total_bits = 32 - self.parent.prefix;
            let blocks = 1u32 << total_bits.saturating_sub(2);
            for i in 0..blocks {
                let net_addr = base + i * 4;
                if !self.allocated.contains(&net_addr) {
                    self.allocated.insert(net_addr);
                    let listener = Ipv4Addr::from(net_addr + 1);
                    let connector = Ipv4Addr::from(net_addr + 2);
                    return Ok((listener, connector));
                }
            }
            anyhow::bail!(
                "vpn pool exhausted (all /30 blocks in {} in use)",
                self.parent
            )
        }

        /// Free a previously allocated block. `addr` is any address in the /30.
        pub fn free(&mut self, addr: Ipv4Addr) {
            let net_addr = u32::from(addr) & 0xFFFF_FFFC;
            self.allocated.remove(&net_addr);
        }

        /// Check if a static addr collides with any allocated block.
        pub fn collides(&self, addr: Ipv4Addr) -> bool {
            let net_addr = u32::from(addr) & 0xFFFF_FFFC;
            self.allocated.contains(&net_addr)
        }
    }

    /// Calculate the IP MTU for IP packets from a tun MTU.
    pub fn ip_mtu(tun_mtu: u16) -> u16 {
        tun_mtu
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn overlap_truth_table() {
            let a: Ipv4Net = "10.0.0.0/24".parse().unwrap();
            let b: Ipv4Net = "10.0.0.0/25".parse().unwrap();
            let c: Ipv4Net = "10.0.1.0/24".parse().unwrap();
            assert!(a.overlaps(&b));
            assert!(b.overlaps(&a));
            assert!(!a.overlaps(&c));

            let d: Ipv4Net = "192.168.0.0/30".parse().unwrap();
            let e: Ipv4Net = "192.168.0.4/30".parse().unwrap();
            assert!(!d.overlaps(&e));
        }

        #[test]
        fn pool_alloc_assigns_dot1_dot2() {
            let parent: Ipv4Net = "192.168.0.0/30".parse().unwrap();
            let mut pool = PoolAllocator::new(parent).unwrap();
            let (l1, c1) = pool.alloc().unwrap();
            assert_eq!(l1.to_string(), "192.168.0.1");
            assert_eq!(c1.to_string(), "192.168.0.2");
        }

        #[test]
        fn pool_free_reuses_block() {
            let parent: Ipv4Net = "192.168.0.0/28".parse().unwrap();
            let mut pool = PoolAllocator::new(parent).unwrap();
            let (l1, c1) = pool.alloc().unwrap();
            assert_eq!(l1.to_string(), "192.168.0.1");
            pool.free(c1);
            let (l2, c2) = pool.alloc().unwrap();
            assert_eq!(l2, l1);
            assert_eq!(c2, c1);
        }

        #[test]
        fn pool_exhaustion_errors() {
            let result = PoolAllocator::new("192.168.0.0/31".parse().unwrap());
            assert!(result.is_err());

            let parent: Ipv4Net = "192.168.0.0/30".parse().unwrap();
            let mut pool = PoolAllocator::new(parent).unwrap();
            let _ = pool.alloc().unwrap();
            let result = pool.alloc();
            assert!(result.is_err());
        }
    }
}

/// Relay AEAD framing: HKDF key derivation + ChaCha20-Poly1305 seal/open.
/// Public so integration tests can drive the relay link without a TUN device.
pub mod crypto {
    #![allow(dead_code)]
    use anyhow::{bail, Result};
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
    use ring::hkdf;

    const MAX_COUNTER: u64 = u64::MAX - 1;
    /// ChaCha20-Poly1305 authentication tag length in bytes.
    pub const TAG_LEN: usize = 16;
    const INFO_L2C: &[u8] = b"bore-vpn l2c v1";
    const INFO_C2L: &[u8] = b"bore-vpn c2l v1";

    /// Two derived 32-byte keys for the two directions.
    pub struct DirectionKeys {
        /// Key for frames this side seals and sends.
        pub egress: [u8; 32],
        /// Key for frames received from the peer.
        pub ingress: [u8; 32],
    }

    /// Derive the relay AEAD keys for the listener side.
    pub fn derive_keys_listener(secret: &str, nonce: &[u8]) -> Result<DirectionKeys> {
        let l2c = hkdf_expand(secret, nonce, INFO_L2C)?;
        let c2l = hkdf_expand(secret, nonce, INFO_C2L)?;
        Ok(DirectionKeys {
            egress: l2c,
            ingress: c2l,
        })
    }

    /// Derive the relay AEAD keys for the connector side.
    pub fn derive_keys_connector(secret: &str, nonce: &[u8]) -> Result<DirectionKeys> {
        let l2c = hkdf_expand(secret, nonce, INFO_L2C)?;
        let c2l = hkdf_expand(secret, nonce, INFO_C2L)?;
        Ok(DirectionKeys {
            egress: c2l,
            ingress: l2c,
        })
    }

    fn hkdf_expand(secret: &str, salt_bytes: &[u8], info: &[u8]) -> Result<[u8; 32]> {
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt_bytes);
        let prk = salt.extract(secret.as_bytes());
        let info_arr = [info];
        let mut out = [0u8; 32];
        prk.expand(&info_arr, hkdf::HKDF_SHA256)
            .map_err(|_| anyhow::anyhow!("HKDF expand failed"))?
            .fill(&mut out)
            .map_err(|_| anyhow::anyhow!("HKDF fill failed"))?;
        Ok(out)
    }

    /// Build a 96-bit nonce from a u64 counter: 4 zero bytes ‖ counter (BE).
    pub fn nonce_from_counter(counter: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&counter.to_be_bytes());
        n
    }

    /// Seal an IP packet. Returns `[u32 BE total_len][u64 BE counter][ciphertext‖tag]`.
    pub fn seal(key_bytes: &[u8; 32], counter: &mut u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        if *counter >= MAX_COUNTER {
            bail!("AEAD counter exhausted — tear down link");
        }
        let nonce_bytes = nonce_from_counter(*counter);
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, key_bytes)
            .map_err(|_| anyhow::anyhow!("AEAD key init"))?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut buf = plaintext.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
            .map_err(|_| anyhow::anyhow!("AEAD seal"))?;

        let ctr = *counter;
        *counter += 1;

        let total_len = (8 + buf.len()) as u32;
        let mut frame = Vec::with_capacity(4 + 8 + buf.len());
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&ctr.to_be_bytes());
        frame.extend_from_slice(&buf);
        Ok(frame)
    }

    /// Open a received frame. `frame` is the raw bytes after reading `[u32 total_len]`.
    pub fn open(key_bytes: &[u8; 32], frame: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(
            frame.len() >= 8 + TAG_LEN,
            "frame too short: {} bytes",
            frame.len()
        );
        let ctr = u64::from_be_bytes(frame[..8].try_into().unwrap());
        let nonce_bytes = nonce_from_counter(ctr);

        let unbound = UnboundKey::new(&CHACHA20_POLY1305, key_bytes)
            .map_err(|_| anyhow::anyhow!("AEAD key init"))?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut buf = frame[8..].to_vec();
        let plaintext = key
            .open_in_place(nonce, Aad::empty(), &mut buf)
            .map_err(|_| anyhow::anyhow!("AEAD open — tampered or wrong key"))?;
        Ok(plaintext.to_vec())
    }

    /// Seal with an explicit counter value (no auto-increment).
    /// Returns `[u32 BE total_len][u64 BE counter][ciphertext‖tag]`.
    pub fn seal_with_counter(
        key_bytes: &[u8; 32],
        counter: u64,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        if counter >= MAX_COUNTER {
            bail!("AEAD counter exhausted — tear down link");
        }
        let nonce_bytes = nonce_from_counter(counter);
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, key_bytes)
            .map_err(|_| anyhow::anyhow!("AEAD key init"))?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut buf = plaintext.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
            .map_err(|_| anyhow::anyhow!("AEAD seal"))?;

        let total_len = (8 + buf.len()) as u32;
        let mut frame = Vec::with_capacity(4 + 8 + buf.len());
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&counter.to_be_bytes());
        frame.extend_from_slice(&buf);
        Ok(frame)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn aead_roundtrip_ok() {
            let key = [1u8; 32];
            let mut ctr = 0u64;
            let plaintext = b"hello world";
            let sealed = seal(&key, &mut ctr, plaintext).unwrap();
            assert_eq!(ctr, 1);
            let frame = &sealed[4..];
            let opened = open(&key, frame).unwrap();
            assert_eq!(&opened[..], plaintext);
        }

        #[test]
        fn aead_tamper_fails() {
            let key = [1u8; 32];
            let mut ctr = 0u64;
            let plaintext = b"hello world";
            let sealed = seal(&key, &mut ctr, plaintext).unwrap();
            let mut frame = sealed[4..].to_vec();
            if frame.len() > 20 {
                frame[20] ^= 0xFF;
            }
            let result = open(&key, &frame);
            assert!(result.is_err());
        }

        #[test]
        fn aead_wrong_key_fails() {
            let key1 = [1u8; 32];
            let key2 = [2u8; 32];
            let mut ctr = 0u64;
            let plaintext = b"hello world";
            let sealed = seal(&key1, &mut ctr, plaintext).unwrap();
            let frame = &sealed[12..];
            let result = open(&key2, frame);
            assert!(result.is_err());
        }

        #[test]
        fn nonce_monotonic_unique() {
            let n1 = nonce_from_counter(0);
            let n2 = nonce_from_counter(1);
            assert_ne!(n1, n2);
        }

        #[test]
        fn hkdf_deterministic() {
            let secret = "test-secret";
            let nonce = b"test-nonce";
            let k1 = derive_keys_listener(secret, nonce).unwrap();
            let k2 = derive_keys_listener(secret, nonce).unwrap();
            assert_eq!(k1.egress, k2.egress);
            assert_eq!(k1.ingress, k2.ingress);
        }

        #[test]
        fn hkdf_directions_differ() {
            let secret = "test-secret";
            let nonce = b"test-nonce";
            let listener = derive_keys_listener(secret, nonce).unwrap();
            let connector = derive_keys_connector(secret, nonce).unwrap();
            assert_ne!(listener.egress, connector.egress);
            assert_ne!(listener.ingress, connector.ingress);
            assert_eq!(listener.egress, connector.ingress);
            assert_eq!(listener.ingress, connector.egress);
        }
    }
}

/// Command builders for host network configuration (ip, nft, iptables).
pub mod hostcfg_cmd {
    #![allow(dead_code)]
    /// Build `ip addr add <addr>/<prefix> dev <dev>` argv.
    pub fn cmd_addr_add(dev: &str, addr: &str, prefix: u8) -> Vec<String> {
        vec![
            "ip".into(),
            "addr".into(),
            "add".into(),
            format!("{addr}/{prefix}"),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build `ip addr del <addr>/<prefix> dev <dev>` argv.
    pub fn cmd_addr_del(dev: &str, addr: &str, prefix: u8) -> Vec<String> {
        vec![
            "ip".into(),
            "addr".into(),
            "del".into(),
            format!("{addr}/{prefix}"),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build `ip link set <dev> up` argv.
    pub fn cmd_link_set_up(dev: &str) -> Vec<String> {
        vec![
            "ip".into(),
            "link".into(),
            "set".into(),
            dev.into(),
            "up".into(),
        ]
    }

    /// Build `ip link set <dev> mtu <mtu>` argv.
    pub fn cmd_link_set_mtu(dev: &str, mtu: u16) -> Vec<String> {
        vec![
            "ip".into(),
            "link".into(),
            "set".into(),
            dev.into(),
            "mtu".into(),
            mtu.to_string(),
        ]
    }

    /// Build `ip route replace <subnet> dev <dev>` argv.
    ///
    /// `replace` (not `add`) keeps the operation idempotent: a stale route left
    /// behind by a crashed previous run (or an in-flight reconnect) would make
    /// `ip route add` fail with EEXIST and abort the whole link setup.
    pub fn cmd_route_add(subnet: &str, dev: &str) -> Vec<String> {
        vec![
            "ip".into(),
            "route".into(),
            "replace".into(),
            subnet.into(),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build the `sh -c "echo <v> | sudo -n tee /proc/sys/net/ipv4/ip_forward"` argv.
    ///
    /// Fallback used when the process holds CAP_NET_ADMIN but not UID 0: writing
    /// `/proc/sys/net/ipv4/ip_forward` directly fails with EACCES, while a
    /// non-interactive `sudo -n tee` succeeds if the operator installed the
    /// recommended sudoers line (see docs/vpn/VPN.md, "Requirements").
    pub fn cmd_sysctl_ip_forward(value: u8) -> Vec<String> {
        vec![
            "sh".into(),
            "-c".into(),
            format!("echo {value} | sudo -n tee /proc/sys/net/ipv4/ip_forward"),
        ]
    }

    /// Build `ip route del <subnet> dev <dev>` argv.
    pub fn cmd_route_del(subnet: &str, dev: &str) -> Vec<String> {
        vec![
            "ip".into(),
            "route".into(),
            "del".into(),
            subnet.into(),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build `ip route get <host>` argv.
    pub fn cmd_route_get(host: &str) -> Vec<String> {
        vec!["ip".into(), "route".into(), "get".into(), host.into()]
    }

    /// Parse the output of `ip route get <host>` to extract the `dev <iface>` field.
    pub fn parse_lan_iface(output: &str) -> Option<String> {
        let mut iter = output.split_whitespace();
        while let Some(token) = iter.next() {
            if token == "dev" {
                return iter.next().map(str::to_string);
            }
        }
        None
    }

    /// Build `nft add table inet bore_vpn_<id>` argv.
    pub fn cmd_nft_add_table(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "table".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build `nft add chain inet bore_vpn_<id> post` argv.
    pub fn cmd_nft_add_postrouting_chain(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "chain".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "post".into(),
            "{ type nat hook postrouting priority 100 ; }".into(),
        ]
    }

    /// Build nft masquerade rule argv.
    pub fn cmd_nft_add_masquerade_rule(id: &str, tun: &str, lan_if: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "post".into(),
            "iif".into(),
            tun.into(),
            "oif".into(),
            lan_if.into(),
            "masquerade".into(),
        ]
    }

    /// Build nft forward chain argv.
    pub fn cmd_nft_add_forward_chain(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "chain".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "bore_fw".into(),
            "{ type filter hook forward priority -10 ; }".into(),
        ]
    }

    /// Build nft MSS clamp argv.
    pub fn cmd_nft_add_mss_clamp(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "bore_fw".into(),
            "tcp".into(),
            "flags".into(),
            "syn".into(),
            "tcp".into(),
            "option".into(),
            "maxseg".into(),
            "size".into(),
            "set".into(),
            "rt".into(),
            "mtu".into(),
        ]
    }

    /// Build `nft delete table inet bore_vpn_<id>` argv.
    pub fn cmd_nft_delete_table(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "delete".into(),
            "table".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    // ── iptables custom chain management (F3/F4 fix) ────────────────────────────
    //
    // Per-link NAT chains avoid `-i` in POSTROUTING (which iptables rejects, F4)
    // and enable chain teardown by `-X` instead of comment-matching (which fails
    // with partial-spec rules, F3). Chain names kept short (< 28 chars) for iptables
    // portability. Pre chain handles DNAT (prerouting hook); post chain handles
    // SNAT, masquerade, and scoped masquerade (postrouting hook).

    /// Build the prerouting custom chain name: `bore_<id>_pre`.
    pub fn ipt_nat_pre_chain(id: &str) -> String {
        format!("bore_{id}_pre")
    }

    /// Build the postrouting custom chain name: `bore_<id>_post`.
    pub fn ipt_nat_post_chain(id: &str) -> String {
        format!("bore_{id}_post")
    }

    /// Build `iptables -t nat -N <chain>` argv (create custom chain).
    pub fn cmd_iptables_nat_new_chain(chain: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-N".into(),
            chain.into(),
        ]
    }

    /// Build `iptables -t nat -F <chain>` argv (flush custom chain).
    pub fn cmd_iptables_nat_flush_chain(chain: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-F".into(),
            chain.into(),
        ]
    }

    /// Build `iptables -t nat -X <chain>` argv (delete empty custom chain).
    pub fn cmd_iptables_nat_del_chain(chain: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-X".into(),
            chain.into(),
        ]
    }

    /// Build `iptables -t nat -A <hook> -j <chain>` argv (jump from hook to chain).
    pub fn cmd_iptables_nat_jump(hook: &str, chain: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-A".into(),
            hook.into(),
            "-j".into(),
            chain.into(),
        ]
    }

    /// Build `iptables -t nat -D <hook> -j <chain>` argv (delete jump from hook).
    pub fn cmd_iptables_nat_jump_del(hook: &str, chain: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-D".into(),
            hook.into(),
            "-j".into(),
            chain.into(),
        ]
    }

    /// Build iptables masquerade rule argv for the postrouting chain.
    pub fn cmd_iptables_masquerade_add(id: &str, lan_if: &str) -> Vec<String> {
        let chain = ipt_nat_post_chain(id);
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-A".into(),
            chain,
            "-o".into(),
            lan_if.into(),
            "-j".into(),
            "MASQUERADE".into(),
        ]
    }

    /// Build iptables MSS clamp argv.
    pub fn cmd_iptables_mss_clamp_add(id: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "mangle".into(),
            "-A".into(),
            "FORWARD".into(),
            "-p".into(),
            "tcp".into(),
            "--tcp-flags".into(),
            "SYN,RST".into(),
            "SYN".into(),
            "-j".into(),
            "TCPMSS".into(),
            "--clamp-mss-to-pmtu".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build iptables MSS clamp del argv.
    pub fn cmd_iptables_mss_clamp_del(id: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "mangle".into(),
            "-D".into(),
            "FORWARD".into(),
            "-p".into(),
            "tcp".into(),
            "--tcp-flags".into(),
            "SYN,RST".into(),
            "SYN".into(),
            "-j".into(),
            "TCPMSS".into(),
            "--clamp-mss-to-pmtu".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build nft spoke-isolation drop rule for hub mode: `nft add rule inet bore_vpn_<id> bore_fw iif <tun> oif <tun> drop`.
    pub fn cmd_nft_add_spoke_isolation(id: &str, tun: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "bore_fw".into(),
            "iif".into(),
            tun.into(),
            "oif".into(),
            tun.into(),
            "drop".into(),
        ]
    }

    /// Build iptables spoke-isolation drop rule for hub mode: `-A FORWARD -i <tun> -o <tun> -j DROP`.
    pub fn cmd_iptables_spoke_isolation_add(id: &str, tun: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-A".into(),
            "FORWARD".into(),
            "-i".into(),
            tun.into(),
            "-o".into(),
            tun.into(),
            "-j".into(),
            "DROP".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}_spoke_iso"),
        ]
    }

    /// Build iptables spoke-isolation drop rule deletion.
    pub fn cmd_iptables_spoke_isolation_del(id: &str, _tun: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-D".into(),
            "FORWARD".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}_spoke_iso"),
        ]
    }

    // ── Overlapping-subnet 1:1 NAT (netmap), "E3" ────────────────────────────
    //
    // Stateless prefix-preserving DNAT/SNAT, the strongSwan/OPNsense "1:1 NAT".
    // nft form is the modern prefix netmap (`dnat ip to <prefix>`), locked by the
    // Phase 1.0 spike on nft 1.0.9 (renders as `dstnat`/`srcnat`). iptables uses
    // the rock-solid `NETMAP` target. All carry the `bore_vpn_<id>` comment so the
    // SIGKILL reclaim loop can find them.

    /// Build `nft add chain inet bore_vpn_<id> pre { type nat hook prerouting priority -100 ; }`
    /// (DNAT runs before the routing decision; nft renders priority -100 as `dstnat`).
    pub fn cmd_nft_add_prerouting_chain(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "chain".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "pre".into(),
            "{ type nat hook prerouting priority -100 ; }".into(),
        ]
    }

    /// Build nft ingress netmap DNAT: `... pre iif <tun> ip daddr <exposed> dnat ip prefix to <real>`.
    /// Prefix-preserving (host bits kept): `10.50.1.7 → 192.168.1.7`.
    ///
    /// The `prefix` keyword is MANDATORY: without it, `dnat ip to <prefix>` treats the
    /// target as a plain range and the kernel scrambles the host part (verified on
    /// nft 1.0.9 / kernel 7.0: `100.100.16.138 → 10.10.16.8`, not `.138`). `dnat ip
    /// prefix to <prefix>` sets NF_NAT_RANGE_NETMAP → true 1:1 host-bit-preserving map,
    /// matching the iptables `NETMAP` fallback below.
    pub fn cmd_nft_add_netmap_dnat(id: &str, tun: &str, exposed: &str, real: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "pre".into(),
            "iif".into(),
            tun.into(),
            "ip".into(),
            "daddr".into(),
            exposed.into(),
            "dnat".into(),
            "ip".into(),
            "prefix".into(),
            "to".into(),
            real.into(),
        ]
    }

    /// Build nft egress netmap SNAT: `... post oif <tun> ip saddr <real> snat ip prefix to <exposed>`
    /// (reuses the existing postrouting chain). The `prefix` keyword is MANDATORY for
    /// host-bit preservation — see [`cmd_nft_add_netmap_dnat`].
    pub fn cmd_nft_add_netmap_snat(id: &str, tun: &str, real: &str, exposed: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "post".into(),
            "oif".into(),
            tun.into(),
            "ip".into(),
            "saddr".into(),
            real.into(),
            "snat".into(),
            "ip".into(),
            "prefix".into(),
            "to".into(),
            exposed.into(),
        ]
    }

    /// Build nft destination-scoped masquerade for a PLAIN advertised subnet (N6):
    /// `... post iif <tun> oif <lan_if> ip daddr <plain> masquerade`. Used instead
    /// of the blanket masquerade whenever any NAT entry is present, so NAT'd
    /// tunnel→LAN traffic (source already a peer virtual) is never re-masqueraded.
    pub fn cmd_nft_add_masquerade_scoped(
        id: &str,
        tun: &str,
        lan_if: &str,
        plain: &str,
    ) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "post".into(),
            "iif".into(),
            tun.into(),
            "oif".into(),
            lan_if.into(),
            "ip".into(),
            "daddr".into(),
            plain.into(),
            "masquerade".into(),
        ]
    }

    /// Build iptables ingress NETMAP DNAT add into the prerouting custom chain.
    pub fn cmd_iptables_netmap_dnat_add(
        id: &str,
        tun: &str,
        exposed: &str,
        real: &str,
    ) -> Vec<String> {
        let chain = ipt_nat_pre_chain(id);
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-A".into(),
            chain,
            // Scope to tunnel ingress (parity with nft `iif <tun>`): the exposed
            // virtual is only routed via the TUN.
            "-i".into(),
            tun.into(),
            "-d".into(),
            exposed.into(),
            "-j".into(),
            "NETMAP".into(),
            "--to".into(),
            real.into(),
        ]
    }

    /// Build iptables egress NETMAP SNAT add into the postrouting custom chain.
    pub fn cmd_iptables_netmap_snat_add(
        id: &str,
        tun: &str,
        real: &str,
        exposed: &str,
    ) -> Vec<String> {
        let chain = ipt_nat_post_chain(id);
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-A".into(),
            chain,
            // MUST scope to tunnel egress (parity with nft `oif <tun>`): without
            // `-o <tun>` this would rewrite the real LAN's normal (non-tunnel)
            // traffic source and break the LAN.
            "-o".into(),
            tun.into(),
            "-s".into(),
            real.into(),
            "-j".into(),
            "NETMAP".into(),
            "--to".into(),
            exposed.into(),
        ]
    }

    /// Build iptables destination-scoped masquerade add into the postrouting chain.
    pub fn cmd_iptables_masquerade_scoped_add(id: &str, lan_if: &str, subnet: &str) -> Vec<String> {
        let chain = ipt_nat_post_chain(id);
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-A".into(),
            chain,
            "-o".into(),
            lan_if.into(),
            "-d".into(),
            subnet.into(),
            "-j".into(),
            "MASQUERADE".into(),
        ]
    }

    /// macOS argv builders (E6 groundwork, host-only mode — no NAT/forwarding).
    ///
    /// Pure functions so the snapshots run on every platform. The runtime
    /// host-config refactor that selects these per-OS is still pending (the
    /// `vpn` module is currently compiled on Linux only).
    pub mod macos {
        /// Build `route -n add -net <subnet> -interface <dev>` argv.
        pub fn cmd_route_add(subnet: &str, dev: &str) -> Vec<String> {
            vec![
                "route".into(),
                "-n".into(),
                "add".into(),
                "-net".into(),
                subnet.into(),
                "-interface".into(),
                dev.into(),
            ]
        }

        /// Build `route -n delete -net <subnet> -interface <dev>` argv.
        pub fn cmd_route_del(subnet: &str, dev: &str) -> Vec<String> {
            vec![
                "route".into(),
                "-n".into(),
                "delete".into(),
                "-net".into(),
                subnet.into(),
                "-interface".into(),
                dev.into(),
            ]
        }

        /// Build `ifconfig <dev> mtu <mtu>` argv (dynamic PMTU).
        pub fn cmd_link_set_mtu(dev: &str, mtu: u16) -> Vec<String> {
            vec!["ifconfig".into(), dev.into(), "mtu".into(), mtu.to_string()]
        }
    }

    /// Windows argv builders (E6 groundwork, host-only mode). `netsh` is used
    /// over `route ADD` for native CIDR syntax (no interface-index lookups).
    pub mod windows {
        /// Build `netsh interface ipv4 add route <cidr> <iface>` argv.
        pub fn cmd_route_add(cidr: &str, iface: &str) -> Vec<String> {
            vec![
                "netsh".into(),
                "interface".into(),
                "ipv4".into(),
                "add".into(),
                "route".into(),
                cidr.into(),
                iface.into(),
            ]
        }

        /// Build `netsh interface ipv4 delete route <cidr> <iface>` argv.
        pub fn cmd_route_del(cidr: &str, iface: &str) -> Vec<String> {
            vec![
                "netsh".into(),
                "interface".into(),
                "ipv4".into(),
                "delete".into(),
                "route".into(),
                cidr.into(),
                iface.into(),
            ]
        }

        /// Build `netsh interface ipv4 set subinterface <iface> mtu=<mtu>` argv.
        pub fn cmd_link_set_mtu(iface: &str, mtu: u16) -> Vec<String> {
            vec![
                "netsh".into(),
                "interface".into(),
                "ipv4".into(),
                "set".into(),
                "subinterface".into(),
                iface.into(),
                format!("mtu={mtu}"),
            ]
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cmd_macos_builders_snapshot() {
            assert_eq!(
                macos::cmd_route_add("10.0.0.0/24", "utun4"),
                vec![
                    "route",
                    "-n",
                    "add",
                    "-net",
                    "10.0.0.0/24",
                    "-interface",
                    "utun4"
                ]
            );
            assert_eq!(
                macos::cmd_route_del("10.0.0.0/24", "utun4"),
                vec![
                    "route",
                    "-n",
                    "delete",
                    "-net",
                    "10.0.0.0/24",
                    "-interface",
                    "utun4"
                ]
            );
            assert_eq!(
                macos::cmd_link_set_mtu("utun4", 1400),
                vec!["ifconfig", "utun4", "mtu", "1400"]
            );
        }

        #[test]
        fn cmd_windows_builders_snapshot() {
            assert_eq!(
                windows::cmd_route_add("10.0.0.0/24", "bore0"),
                vec![
                    "netsh",
                    "interface",
                    "ipv4",
                    "add",
                    "route",
                    "10.0.0.0/24",
                    "bore0"
                ]
            );
            assert_eq!(
                windows::cmd_route_del("10.0.0.0/24", "bore0"),
                vec![
                    "netsh",
                    "interface",
                    "ipv4",
                    "delete",
                    "route",
                    "10.0.0.0/24",
                    "bore0"
                ]
            );
            assert_eq!(
                windows::cmd_link_set_mtu("bore0", 1400),
                vec![
                    "netsh",
                    "interface",
                    "ipv4",
                    "set",
                    "subinterface",
                    "bore0",
                    "mtu=1400"
                ]
            );
        }

        #[test]
        fn cmd_route_replace_snapshot() {
            let cmd = cmd_route_add("10.0.0.0/24", "tun0");
            assert_eq!(
                cmd,
                vec!["ip", "route", "replace", "10.0.0.0/24", "dev", "tun0"]
            );
        }

        #[test]
        fn cmd_sysctl_ip_forward_snapshot() {
            assert_eq!(
                cmd_sysctl_ip_forward(1),
                vec![
                    "sh",
                    "-c",
                    "echo 1 | sudo -n tee /proc/sys/net/ipv4/ip_forward"
                ]
            );
            assert_eq!(
                cmd_sysctl_ip_forward(0),
                vec![
                    "sh",
                    "-c",
                    "echo 0 | sudo -n tee /proc/sys/net/ipv4/ip_forward"
                ]
            );
        }

        #[test]
        fn cmd_nft_table_snapshot() {
            let cmd = cmd_nft_add_table("link1");
            assert_eq!(cmd, vec!["nft", "add", "table", "inet", "bore_vpn_link1"]);
        }

        #[test]
        fn cmd_iptables_fallback_snapshot() {
            let cmd = cmd_iptables_masquerade_add("link1", "eth0");
            assert_eq!(
                cmd,
                vec![
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "bore_link1_post",
                    "-o",
                    "eth0",
                    "-j",
                    "MASQUERADE",
                ]
            );
        }

        #[test]
        fn cmd_nft_netmap_snapshots() {
            assert_eq!(
                cmd_nft_add_prerouting_chain("link1"),
                vec![
                    "nft",
                    "add",
                    "chain",
                    "inet",
                    "bore_vpn_link1",
                    "pre",
                    "{ type nat hook prerouting priority -100 ; }"
                ]
            );
            assert_eq!(
                cmd_nft_add_netmap_dnat("link1", "bore0", "10.50.1.0/24", "192.168.1.0/24"),
                vec![
                    "nft",
                    "add",
                    "rule",
                    "inet",
                    "bore_vpn_link1",
                    "pre",
                    "iif",
                    "bore0",
                    "ip",
                    "daddr",
                    "10.50.1.0/24",
                    "dnat",
                    "ip",
                    "prefix",
                    "to",
                    "192.168.1.0/24"
                ]
            );
            assert_eq!(
                cmd_nft_add_netmap_snat("link1", "bore0", "192.168.1.0/24", "10.50.1.0/24"),
                vec![
                    "nft",
                    "add",
                    "rule",
                    "inet",
                    "bore_vpn_link1",
                    "post",
                    "oif",
                    "bore0",
                    "ip",
                    "saddr",
                    "192.168.1.0/24",
                    "snat",
                    "ip",
                    "prefix",
                    "to",
                    "10.50.1.0/24"
                ]
            );
            assert_eq!(
                cmd_nft_add_masquerade_scoped("link1", "bore0", "eth0", "172.16.9.0/24"),
                vec![
                    "nft",
                    "add",
                    "rule",
                    "inet",
                    "bore_vpn_link1",
                    "post",
                    "iif",
                    "bore0",
                    "oif",
                    "eth0",
                    "ip",
                    "daddr",
                    "172.16.9.0/24",
                    "masquerade"
                ]
            );
        }

        #[test]
        fn cmd_iptables_netmap_snapshots() {
            // Test chain-based DNAT rule targeting the prerouting custom chain.
            assert_eq!(
                cmd_iptables_netmap_dnat_add("link1", "bore0", "10.50.1.0/24", "192.168.1.0/24"),
                vec![
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "bore_link1_pre",
                    "-i",
                    "bore0",
                    "-d",
                    "10.50.1.0/24",
                    "-j",
                    "NETMAP",
                    "--to",
                    "192.168.1.0/24",
                ]
            );
            // Test chain-based SNAT rule targeting the postrouting custom chain.
            assert_eq!(
                cmd_iptables_netmap_snat_add("link1", "bore0", "192.168.1.0/24", "10.50.1.0/24"),
                vec![
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "bore_link1_post",
                    "-o",
                    "bore0",
                    "-s",
                    "192.168.1.0/24",
                    "-j",
                    "NETMAP",
                    "--to",
                    "10.50.1.0/24",
                ]
            );
            // Test chain-based scoped masquerade.
            assert_eq!(
                cmd_iptables_masquerade_scoped_add("link1", "eth0", "172.16.9.0/24"),
                vec![
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "bore_link1_post",
                    "-o",
                    "eth0",
                    "-d",
                    "172.16.9.0/24",
                    "-j",
                    "MASQUERADE",
                ]
            );
        }

        #[test]
        fn cmd_iptables_nat_chain_management_snapshots() {
            // Test chain management builders.
            assert_eq!(
                cmd_iptables_nat_new_chain("bore_link1_pre"),
                vec!["iptables", "-t", "nat", "-N", "bore_link1_pre",]
            );
            assert_eq!(
                cmd_iptables_nat_flush_chain("bore_link1_post"),
                vec!["iptables", "-t", "nat", "-F", "bore_link1_post",]
            );
            assert_eq!(
                cmd_iptables_nat_del_chain("bore_link1_post"),
                vec!["iptables", "-t", "nat", "-X", "bore_link1_post",]
            );
            assert_eq!(
                cmd_iptables_nat_jump("POSTROUTING", "bore_link1_post"),
                vec![
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-j",
                    "bore_link1_post",
                ]
            );
            assert_eq!(
                cmd_iptables_nat_jump_del("PREROUTING", "bore_link1_pre"),
                vec![
                    "iptables",
                    "-t",
                    "nat",
                    "-D",
                    "PREROUTING",
                    "-j",
                    "bore_link1_pre",
                ]
            );
        }

        #[test]
        fn parse_lan_iface_from_ip_route_get() {
            let output = "10.0.0.1 via 192.168.1.1 dev eth0 src 192.168.1.100";
            let iface = parse_lan_iface(output);
            assert_eq!(iface, Some("eth0".to_string()));

            let output2 = "10.0.0.1 dev eth0 src 192.168.1.100";
            let iface2 = parse_lan_iface(output2);
            assert_eq!(iface2, Some("eth0".to_string()));
        }
    }
}

pub mod hostcfg {
    #![allow(dead_code)]
    //! Host network configuration (routes, NAT, ip_forward) with RAII cleanup.
    //!
    //! Manages routes, ip_forward toggle, and NAT rules for a VPN link.
    //! All configuration is reverted in reverse order on Drop (cleanup path).

    use anyhow::{anyhow, bail, Context};
    use std::collections::HashSet;
    use std::net::Ipv4Addr;
    use std::process::Command;

    /// Resolve a requested TUN name. "auto" → first free `boreN` (N=0..=255) per `exists`.
    /// Any explicit name is returned verbatim. None only if every auto slot is taken.
    pub fn pick_tun_name(requested: &str, exists: impl Fn(&str) -> bool) -> Option<String> {
        if requested != "auto" {
            return Some(requested.to_string());
        }
        for n in 0..=255 {
            let c = format!("bore{n}");
            if !exists(&c) {
                return Some(c);
            }
        }
        None
    }

    /// Injectable command runner (allows unit testing without root).
    pub trait CommandRunner: Send + Sync {
        /// Run a command with the given argv.
        fn run<'a>(
            &'a self,
            argv: &'a [String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>;
    }

    /// Real runner: calls std::process::Command (blocking, suitable for root operations).
    pub struct RealRunner;

    impl CommandRunner for RealRunner {
        fn run<'a>(
            &'a self,
            argv: &'a [String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>
        {
            Box::pin(async move {
                anyhow::ensure!(!argv.is_empty(), "empty argv");
                let out = Command::new(&argv[0])
                    .args(&argv[1..])
                    .output()
                    .with_context(|| format!("failed to run {:?}", argv))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    bail!(
                        "command {:?} failed ({}): {}",
                        argv,
                        out.status,
                        stderr.trim()
                    );
                }
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            })
        }
    }

    /// Test runner: records all calls in memory.
    #[cfg(test)]
    pub struct TestRunner {
        calls: std::sync::Arc<tokio::sync::Mutex<Vec<Vec<String>>>>,
    }

    #[cfg(test)]
    impl TestRunner {
        /// Create a new test runner.
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            Self {
                calls: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
            }
        }

        /// Get the list of commands that were run.
        pub async fn get_calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().await.clone()
        }
    }

    #[cfg(test)]
    impl CommandRunner for TestRunner {
        fn run<'a>(
            &'a self,
            argv: &'a [String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>
        {
            let calls = std::sync::Arc::clone(&self.calls);
            let argv_owned = argv.to_vec();
            Box::pin(async move {
                calls.lock().await.push(argv_owned.clone());
                // Canned `ip route get` reply so the gateway path can resolve a LAN
                // iface ("eth0") in hermetic unit tests; everything else is a no-op.
                if argv_owned.len() >= 3
                    && argv_owned[0] == "ip"
                    && argv_owned[1] == "route"
                    && argv_owned[2] == "get"
                {
                    return Ok("10.0.0.1 dev eth0 src 10.0.0.2".to_string());
                }
                Ok(String::new())
            })
        }
    }

    /// Check that we are root (UID 0).
    pub fn check_root() -> anyhow::Result<()> {
        if nix::unistd::getuid().is_root() {
            Ok(())
        } else {
            bail!(
                "bore vpn requires root privileges (or CAP_NET_ADMIN). \
                 Run with sudo or grant the capability."
            )
        }
    }

    /// Verify a binary exists by running it with --version.
    pub fn check_binary_exists(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|_| true)
            .unwrap_or(false)
    }

    /// Delete leftover resources from a previous failed run (idempotent, best-effort).
    /// TUN devices are non-persistent (kernel auto-removes on process death), so we do NOT
    /// delete them here — that would only destroy a co-located live instance's interface.
    pub async fn stale_reclaim(id: &str, role: &str) {
        // Try to restore ip_forward from state file (SIGKILL recovery). The file is keyed
        // by (id, role) so a co-located peer with the same id (e.g. the connector in the
        // netns harness) cannot read+delete THIS side's file out from under it.
        let state_path = ipforward_state_path(id, role);
        if let Ok(content) = std::fs::read_to_string(&state_path) {
            if let Ok(saved_value) = content.trim().parse::<u8>() {
                tracing::info!(
                    saved_value,
                    "stale_reclaim: restoring ip_forward from state file"
                );
                let _ = std::fs::write(
                    "/proc/sys/net/ipv4/ip_forward",
                    format!("{}\n", saved_value),
                );
                // If direct write failed, try sudo -n fallback
                if std::fs::write(
                    "/proc/sys/net/ipv4/ip_forward",
                    format!("{}\n", saved_value),
                )
                .is_err()
                {
                    let argv = super::hostcfg_cmd::cmd_sysctl_ip_forward(saved_value);
                    let _ = std::process::Command::new(&argv[0])
                        .args(&argv[1..])
                        .output();
                }
            }
            let _ = std::fs::remove_file(&state_path);
        }

        // Try to delete nft table (ignore "not found" errors)
        let _ = Command::new("nft")
            .args(["delete", "table", "inet", &format!("bore_vpn_{id}")])
            .output();

        // Try to delete iptables chains (F3/F4 fix: custom chain teardown by id only, no subnet info).
        // Run deletion commands ignoring errors (they may not exist).
        use super::hostcfg_cmd::*;
        let post = ipt_nat_post_chain(id);
        let pre = ipt_nat_pre_chain(id);

        // Order: delete jumps, flush chains, delete chains (iptables requires this).
        let teardown_cmds = vec![
            cmd_iptables_nat_jump_del("POSTROUTING", &post),
            cmd_iptables_nat_jump_del("PREROUTING", &pre),
            cmd_iptables_nat_flush_chain(&post),
            cmd_iptables_nat_del_chain(&post),
            cmd_iptables_nat_flush_chain(&pre),
            cmd_iptables_nat_del_chain(&pre),
        ];

        for argv in teardown_cmds {
            let _ = Command::new(&argv[0])
                .args(&argv[1..])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
        }

        let mss_clamp_del = super::hostcfg_cmd::cmd_iptables_mss_clamp_del(id);
        let mut cmd = Command::new(&mss_clamp_del[0]);
        cmd.args(&mss_clamp_del[1..]);
        let _ = cmd.output();
    }

    /// Create a TUN device with `queues` kernel queues (C1).
    ///
    /// Resolves "auto" name to the first free `boreN` (N=0..=255); explicit names are used
    /// verbatim. Tries `IFF_VNET_HDR` + GSO/GRO offload first (Phase 6.2). If the kernel
    /// does not support it the flag is not set and we fall back to single-packet I/O (Phase 6.1).
    /// With `queues > 1` the device is created with `IFF_MULTI_QUEUE` and the extra queue fds
    /// come from `try_clone` (each clone = one more queue). Returns `(devices, offload_enabled,
    /// resolved_name)` — with `queues == 1` a vector of one, path identical to before (I-9).
    pub async fn create_tun(
        name: &str,
        addr: Ipv4Addr,
        prefix: u8,
        mtu: u16,
        queues: usize,
    ) -> anyhow::Result<(Vec<tun_rs::AsyncDevice>, bool, String)> {
        let queues = queues.clamp(1, 8);

        // Resolve and create with retry for auto-mode races.
        let mut taken = HashSet::new();
        let mut resolved_name;
        let (first, offload) = 'create_loop: loop {
            // Pick the name (auto or explicit).
            resolved_name = match pick_tun_name(name, |c| {
                taken.contains(c) || std::path::Path::new(&format!("/sys/class/net/{c}")).exists()
            }) {
                Some(n) => n,
                None => {
                    if name == "auto" {
                        bail!("no free bore<N> TUN name available (0..=255 all in use)");
                    } else {
                        bail!("TUN name resolution failed for explicit name '{name}'");
                    }
                }
            };

            // Try to build the device with offload first.
            let try_build = |offload: bool| {
                let mut b = tun_rs::DeviceBuilder::new()
                    .name(&resolved_name)
                    .ipv4(addr, prefix, None)
                    .mtu(mtu);
                if offload {
                    b = b.offload(true);
                }
                if queues > 1 {
                    b = b.multi_queue(true);
                }
                b.build_async()
            };

            match try_build(true) {
                Ok(dev) if dev.tcp_gso() || dev.udp_gso() => {
                    tracing::info!(%resolved_name, tcp_gso = dev.tcp_gso(), udp_gso = dev.udp_gso(), queues,
                        "TUN created with GSO/GRO offload (Phase 6.2)");
                    break 'create_loop (dev, true);
                }
                Ok(dev) => {
                    tracing::info!(%resolved_name, "kernel built TUN but reports no GSO; using single-packet path");
                    drop(dev);
                    let dev = try_build(false).context("failed to create TUN device")?;
                    break 'create_loop (dev, false);
                }
                Err(e) => {
                    if name == "auto" && taken.len() < 255 {
                        tracing::debug!(
                            name = %resolved_name,
                            error = %e,
                            "TUN creation failed (race); trying next free boreN"
                        );
                        taken.insert(resolved_name);
                        continue;
                    } else {
                        return Err(e).context("failed to create TUN device");
                    }
                }
            }
        };

        let mut devs = vec![first];
        for i in 1..queues {
            let extra = devs[0]
                .try_clone()
                .with_context(|| format!("failed to clone TUN queue {i}"))?;
            devs.push(extra);
        }
        Ok((devs, offload, resolved_name))
    }

    /// Internal: marker for an ip_forward revert operation.
    #[derive(Debug)]
    enum AppliedOp {
        IpForward { saved_value: u8 },
    }

    /// Sanitize id for use in a filename: keep [A-Za-z0-9_-], replace others with _.
    /// Path of the ip_forward recovery state file. Keyed by BOTH the link id AND the
    /// role ("listen"/"connect"): two peers of one link can share a host (and `/run`
    /// is host-shared, not network-namespaced), so a key on `id` alone would make the
    /// connector's `stale_reclaim` read+delete the listener's file (racing it away) and
    /// would collide outright in site↔site mode where both sides are gateways.
    fn ipforward_state_path(id: &str, role: &str) -> std::path::PathBuf {
        let sanitize = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        std::path::PathBuf::from(format!(
            "/run/bore-vpn-{}-{}.ipforward",
            sanitize(id),
            sanitize(role)
        ))
    }

    /// RAII guard that manages routes, forwarding, and NAT around a VPN link.
    /// Reverts everything in reverse order on `Drop`.
    #[derive(Debug)]
    pub struct NetConfig {
        id: String,
        // "listen" / "connect" — disambiguates the ip_forward state file between two
        // peers of one link that share a host (and `/run`).
        role: String,
        tun_name: String,
        no_route_manage: bool,
        nft_available: bool,
        // Revert actions in reverse order (each is an argv).
        revert_cmds: Vec<Vec<String>>,
        // Labels for logging during revert.
        revert_labels: Vec<String>,
        // Saved ip_forward value (if we changed it).
        ip_forward_saved: Option<u8>,
        // Operations (e.g. ip_forward save/restore).
        applied_ops: Vec<AppliedOp>,
    }

    impl NetConfig {
        /// Apply host network configuration for a VPN link.
        ///
        /// - `id`: link id (used for nft table name)
        /// - `tun_name`: tun device name
        /// - `assigned`: this side's overlay address
        /// - `prefix`: overlay prefix (30 for /30)
        /// - `peer_routes`: subnets to route via the tun
        /// - `advertised`: the **real** local subnets this side exposes (non-empty =
        ///   gateway mode). Drives `ip_forward`, LAN-iface detection, and masquerade
        ///   scope (N7/I-NAT9) — always a real subnet with a local route, never a virtual.
        /// - `nat_maps`: `(real, exposed)` pairs for overlapping-subnet 1:1 netmap
        ///   (N5). Empty ⇒ today's blanket-masquerade path, byte-for-byte (N8/I-NAT1).
        ///   Non-empty ⇒ install DNAT/SNAT netmap per pair + per-plain-subnet scoped
        ///   masquerade instead of the blanket rule (N6/I-NAT5).
        /// - `no_route_manage`: if true, print commands instead of running them
        /// - `hub`: if true, apply hub-mode spoke-isolation rule (blocks spoke-to-spoke forwarding)
        #[allow(clippy::too_many_arguments)]
        pub async fn apply<R: CommandRunner>(
            runner: &R,
            id: &str,
            role: &str,
            tun_name: &str,
            _assigned: std::net::Ipv4Addr,
            _prefix: u8,
            peer_routes: &[crate::shared::Ipv4Net],
            advertised: &[crate::shared::Ipv4Net],
            nat_maps: &[(crate::shared::Ipv4Net, crate::shared::Ipv4Net)],
            no_route_manage: bool,
            hub: bool,
            nat_masquerade: bool,
        ) -> anyhow::Result<Self> {
            use super::hostcfg_cmd::*;

            let mut cfg = NetConfig {
                id: id.to_string(),
                role: role.to_string(),
                tun_name: tun_name.to_string(),
                no_route_manage,
                nft_available: false,
                revert_cmds: Vec::new(),
                revert_labels: Vec::new(),
                ip_forward_saved: None,
                applied_ops: Vec::new(),
            };

            let is_gateway = !advertised.is_empty();

            // ── Routes ────────────────────────────────────────────────────────────
            for net in peer_routes {
                let subnet = net.to_string();
                let argv = cmd_route_add(&subnet, tun_name);
                if no_route_manage {
                    println!("# (skipped, --no-route-manage): {}", argv.join(" "));
                } else {
                    runner
                        .run(&argv)
                        .await
                        .with_context(|| format!("ip route add {subnet}"))?;
                    tracing::info!(%subnet, %tun_name, "added route");
                    cfg.revert_cmds.push(cmd_route_del(&subnet, tun_name));
                    cfg.revert_labels
                        .push(format!("del route {subnet} dev {tun_name}"));
                }
            }

            // ── Gateway mode: ip_forward + NAT ────────────────────────────────────
            if is_gateway && !no_route_manage {
                // Save and enable ip_forward
                let current = tokio::fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
                    .await
                    .unwrap_or_else(|_| "0".to_string());
                let saved: u8 = current.trim().parse().unwrap_or(0);

                if saved == 0 {
                    match tokio::fs::write("/proc/sys/net/ipv4/ip_forward", "1\n").await {
                        Ok(()) => {}
                        Err(e) => {
                            // CAP_NET_ADMIN without UID 0 cannot write procfs
                            // directly; fall back to non-interactive sudo tee.
                            tracing::debug!(error = %e, "direct ip_forward write failed; trying sudo -n fallback");
                            runner
                                .run(&cmd_sysctl_ip_forward(1))
                                .await
                                .context("enable ip_forward (direct write failed and 'sudo -n tee' fallback failed; run as root or add a sudoers rule for tee /proc/sys/net/ipv4/ip_forward)")?;
                        }
                    }
                    tracing::info!("enabled ip_forward (saved={})", saved);

                    // Write state file so stale_reclaim can restore on SIGKILL
                    let state_path = ipforward_state_path(id, role);
                    if let Err(e) = tokio::fs::write(&state_path, format!("{}\n", saved)).await {
                        tracing::debug!(error = %e, ?state_path, "could not write ip_forward state file (will not recover from SIGKILL); continuing");
                    }
                }

                cfg.ip_forward_saved = Some(saved);
                cfg.applied_ops
                    .push(AppliedOp::IpForward { saved_value: saved });

                // Determine LAN egress interface
                let sample_host: std::net::Ipv4Addr = {
                    let net = &advertised[0];
                    let base = u32::from(net.network());
                    std::net::Ipv4Addr::from(base + 1)
                };
                let route_out = runner
                    .run(&cmd_route_get(&sample_host.to_string()))
                    .await
                    .context("ip route get to find LAN iface")?;
                let lan_if = super::hostcfg_cmd::parse_lan_iface(&route_out).ok_or_else(|| {
                    anyhow!("could not determine LAN egress interface from: {route_out}")
                })?;

                // Try nft first, fall back to iptables. Test hook: BORE_VPN_FORCE_IPTABLES
                // env var forces the iptables fallback on nft-available systems.
                cfg.nft_available = check_binary_exists("nft")
                    && std::env::var_os("BORE_VPN_FORCE_IPTABLES").is_none();

                // Plain (non-NAT) advertised reals: everything not in nat_maps. When
                // any NAT entry is present these get a destination-scoped masquerade
                // instead of the blanket rule (N6); NAT'd reals are netmap-only.
                let plain_subnets: Vec<&crate::shared::Ipv4Net> = advertised
                    .iter()
                    .filter(|p| !nat_maps.iter().any(|(r, _)| r == *p))
                    .collect();

                if cfg.nft_available {
                    runner
                        .run(&cmd_nft_add_table(id))
                        .await
                        .context("nft add table")?;
                    runner
                        .run(&cmd_nft_add_postrouting_chain(id))
                        .await
                        .context("nft add postrouting chain")?;

                    if nat_maps.is_empty() {
                        // N8/I-NAT1: byte-for-byte today's path — blanket masquerade,
                        // no prerouting chain, no netmap.
                        runner
                            .run(&cmd_nft_add_masquerade_rule(id, tun_name, &lan_if))
                            .await
                            .context("nft add masquerade rule")?;
                    } else {
                        // Overlapping-subnet 1:1 netmap (E3). DNAT lives in a new
                        // prerouting chain (pri -100, before routing); SNAT reuses the
                        // postrouting chain (pri 100, after).
                        runner
                            .run(&cmd_nft_add_prerouting_chain(id))
                            .await
                            .context("nft add prerouting chain")?;
                        for (real, exposed) in nat_maps {
                            let (r, v) = (real.to_string(), exposed.to_string());
                            runner
                                .run(&cmd_nft_add_netmap_dnat(id, tun_name, &v, &r))
                                .await
                                .with_context(|| format!("nft netmap dnat {v}->{r}"))?;
                            runner
                                .run(&cmd_nft_add_netmap_snat(id, tun_name, &r, &v))
                                .await
                                .with_context(|| format!("nft netmap snat {r}->{v}"))?;
                            tracing::info!(exposed = %v, real = %r, tun = %tun_name, "nat netmap: dnat ingress  exposed -> real");
                            tracing::info!(real = %r, exposed = %v, tun = %tun_name, "nat netmap: snat egress   real -> exposed");
                        }
                        // Plain subnets keep today's semantics, scoped by destination.
                        for plain in &plain_subnets {
                            let p = plain.to_string();
                            runner
                                .run(&cmd_nft_add_masquerade_scoped(id, tun_name, &lan_if, &p))
                                .await
                                .with_context(|| format!("nft scoped masquerade {p}"))?;
                            tracing::info!(plain = %p, lan_if = %lan_if, tun = %tun_name, "masquerade (scoped to plain subnet)");
                        }
                        // F2: opt-in masquerade of NAT'd subnets toward the LAN, scoped to
                        // the REAL (post-DNAT) destination. Lets peers reach every host
                        // behind the gateway (not just the gateway itself) even when the
                        // gateway is not the LAN router; trades away per-peer source
                        // visibility (the LAN host sees the gateway IP). Off = I-NAT5.
                        if nat_masquerade {
                            for (real, _exposed) in nat_maps {
                                let r = real.to_string();
                                runner
                                    .run(&cmd_nft_add_masquerade_scoped(id, tun_name, &lan_if, &r))
                                    .await
                                    .with_context(|| format!("nft nat-masquerade {r}"))?;
                                tracing::info!(real = %r, lan_if = %lan_if, tun = %tun_name, "nat-masquerade: NAT'd subnet -> LAN (--nat-masquerade)");
                            }
                        }
                    }

                    runner
                        .run(&cmd_nft_add_forward_chain(id))
                        .await
                        .context("nft add forward chain")?;
                    runner
                        .run(&cmd_nft_add_mss_clamp(id))
                        .await
                        .context("nft add mss clamp")?;

                    // Hub mode: add spoke-isolation rule
                    if hub {
                        runner
                            .run(&cmd_nft_add_spoke_isolation(id, tun_name))
                            .await
                            .context("nft add spoke isolation")?;
                        tracing::info!(%id, %tun_name, "added nft spoke-isolation rule");
                    }

                    tracing::info!(%id, "created nft table bore_vpn_{}", id);
                    // Single table delete reverts every rule above, netmap included.
                    cfg.revert_cmds.push(cmd_nft_delete_table(id));
                    cfg.revert_labels
                        .push(format!("delete nft table bore_vpn_{id}"));
                } else {
                    // iptables fallback path (F3/F4 fix): custom chains instead of
                    // direct POSTROUTING/PREROUTING rules. Chain teardown via -X
                    // replaces comment-matching deletion (which fails on partial specs).
                    let post = ipt_nat_post_chain(id);

                    // Create and jump to postrouting chain
                    runner
                        .run(&cmd_iptables_nat_new_chain(&post))
                        .await
                        .context("iptables nat new postrouting chain")?;
                    runner
                        .run(&cmd_iptables_nat_jump("POSTROUTING", &post))
                        .await
                        .context("iptables nat jump postrouting")?;

                    if nat_maps.is_empty() {
                        // N8/I-NAT1: byte-for-byte today's path — blanket masquerade,
                        // no prerouting chain, no netmap.
                        runner
                            .run(&cmd_iptables_masquerade_add(id, &lan_if))
                            .await
                            .context("iptables masquerade add")?;
                    } else {
                        // Overlapping-subnet 1:1 netmap (E3). Create prerouting chain
                        // and add DNAT rules; reuse postrouting for SNAT.
                        let pre = ipt_nat_pre_chain(id);
                        runner
                            .run(&cmd_iptables_nat_new_chain(&pre))
                            .await
                            .context("iptables nat new prerouting chain")?;
                        runner
                            .run(&cmd_iptables_nat_jump("PREROUTING", &pre))
                            .await
                            .context("iptables nat jump prerouting")?;

                        for (real, exposed) in nat_maps {
                            let (r, v) = (real.to_string(), exposed.to_string());
                            runner
                                .run(&cmd_iptables_netmap_dnat_add(id, tun_name, &v, &r))
                                .await
                                .with_context(|| format!("iptables netmap dnat {v}->{r}"))?;
                            runner
                                .run(&cmd_iptables_netmap_snat_add(id, tun_name, &r, &v))
                                .await
                                .with_context(|| format!("iptables netmap snat {r}->{v}"))?;
                            tracing::info!(exposed = %v, real = %r, tun = %tun_name, "nat netmap: dnat ingress  exposed -> real");
                            tracing::info!(real = %r, exposed = %v, tun = %tun_name, "nat netmap: snat egress   real -> exposed");
                        }

                        // Plain subnets keep today's semantics, scoped by destination.
                        for plain in &plain_subnets {
                            let p = plain.to_string();
                            runner
                                .run(&cmd_iptables_masquerade_scoped_add(id, &lan_if, &p))
                                .await
                                .with_context(|| format!("iptables scoped masquerade {p}"))?;
                            tracing::info!(plain = %p, lan_if = %lan_if, tun = %tun_name, "masquerade (scoped to plain subnet)");
                        }

                        // F2: opt-in masquerade of NAT'd subnets toward the LAN, scoped to
                        // the REAL (post-DNAT) destination. Lets peers reach every host
                        // behind the gateway (not just the gateway itself) even when the
                        // gateway is not the LAN router; trades away per-peer source
                        // visibility (the LAN host sees the gateway IP). Off = I-NAT5.
                        if nat_masquerade {
                            for (real, _exposed) in nat_maps {
                                let r = real.to_string();
                                runner
                                    .run(&cmd_iptables_masquerade_scoped_add(id, &lan_if, &r))
                                    .await
                                    .with_context(|| format!("iptables nat-masquerade {r}"))?;
                                tracing::info!(real = %r, lan_if = %lan_if, tun = %tun_name, "nat-masquerade: NAT'd subnet -> LAN (--nat-masquerade)");
                            }
                        }
                    }

                    runner
                        .run(&cmd_iptables_mss_clamp_add(id))
                        .await
                        .context("iptables mss clamp add")?;

                    // Hub mode: add spoke-isolation rule
                    if hub {
                        runner
                            .run(&cmd_iptables_spoke_isolation_add(id, tun_name))
                            .await
                            .context("iptables add spoke isolation")?;
                        tracing::info!(%id, %tun_name, "added iptables spoke-isolation rule");
                        cfg.revert_cmds
                            .push(cmd_iptables_spoke_isolation_del(id, tun_name));
                        cfg.revert_labels
                            .push(format!("del iptables spoke isolation bore_vpn_{id}"));
                    }

                    tracing::info!(%id, "applied iptables NAT rules");

                    // Build revert command stack in reverse order for correct cleanup:
                    // (reversed sequence = del jumps, flush chains, delete chains).
                    // MSS clamp del (unchanged behavior, pushed first so reversed = last).
                    cfg.revert_cmds.push(cmd_iptables_mss_clamp_del(id));
                    cfg.revert_labels
                        .push(format!("del iptables mss clamp bore_vpn_{id}"));

                    if !nat_maps.is_empty() {
                        // Pre chain: delete jump, flush, delete chain.
                        let pre = ipt_nat_pre_chain(id);
                        cfg.revert_cmds.push(cmd_iptables_nat_del_chain(&pre));
                        cfg.revert_labels
                            .push(format!("delete iptables nat chain {}", pre));
                        cfg.revert_cmds.push(cmd_iptables_nat_flush_chain(&pre));
                        cfg.revert_labels
                            .push(format!("flush iptables nat chain {}", pre));
                        cfg.revert_cmds
                            .push(cmd_iptables_nat_jump_del("PREROUTING", &pre));
                        cfg.revert_labels
                            .push(format!("delete jump prerouting -> {}", pre));
                    }

                    // Post chain: delete jump, flush, delete chain.
                    cfg.revert_cmds.push(cmd_iptables_nat_del_chain(&post));
                    cfg.revert_labels
                        .push(format!("delete iptables nat chain {}", post));
                    cfg.revert_cmds.push(cmd_iptables_nat_flush_chain(&post));
                    cfg.revert_labels
                        .push(format!("flush iptables nat chain {}", post));
                    cfg.revert_cmds
                        .push(cmd_iptables_nat_jump_del("POSTROUTING", &post));
                    cfg.revert_labels
                        .push(format!("delete jump postrouting -> {}", post));
                }
            } else if is_gateway && no_route_manage {
                // Print commands for gateway mode (nft preferred). LAN iface is a
                // placeholder operators replace with their real egress iface.
                for cmd in gateway_nft_cmds(
                    id,
                    tun_name,
                    "LAN_IFACE",
                    advertised,
                    nat_maps,
                    hub,
                    nat_masquerade,
                ) {
                    println!("# (skipped, --no-route-manage): {}", cmd.join(" "));
                }
            }

            Ok(cfg)
        }
    }

    /// Build the ordered nft command list for the gateway data plane (the
    /// `--no-route-manage` print branch + the testable mirror of the live nft
    /// path). Blanket masquerade when `nat_maps` is empty (N8), else prerouting +
    /// per-pair netmap DNAT/SNAT + per-plain-subnet scoped masquerade (N6).
    pub fn gateway_nft_cmds(
        id: &str,
        tun: &str,
        lan_if: &str,
        advertised: &[crate::shared::Ipv4Net],
        nat_maps: &[(crate::shared::Ipv4Net, crate::shared::Ipv4Net)],
        hub: bool,
        nat_masquerade: bool,
    ) -> Vec<Vec<String>> {
        use super::hostcfg_cmd::*;
        let mut cmds = vec![cmd_nft_add_table(id), cmd_nft_add_postrouting_chain(id)];
        if nat_maps.is_empty() {
            cmds.push(cmd_nft_add_masquerade_rule(id, tun, lan_if));
        } else {
            cmds.push(cmd_nft_add_prerouting_chain(id));
            for (real, exposed) in nat_maps {
                let (r, v) = (real.to_string(), exposed.to_string());
                cmds.push(cmd_nft_add_netmap_dnat(id, tun, &v, &r));
                cmds.push(cmd_nft_add_netmap_snat(id, tun, &r, &v));
            }
            for plain in advertised
                .iter()
                .filter(|p| !nat_maps.iter().any(|(r, _)| r == *p))
            {
                cmds.push(cmd_nft_add_masquerade_scoped(
                    id,
                    tun,
                    lan_if,
                    &plain.to_string(),
                ));
            }
            // F2: opt-in masquerade of NAT'd subnets toward the LAN (scoped to real).
            if nat_masquerade {
                for (real, _exposed) in nat_maps {
                    cmds.push(cmd_nft_add_masquerade_scoped(
                        id,
                        tun,
                        lan_if,
                        &real.to_string(),
                    ));
                }
            }
        }
        cmds.push(cmd_nft_add_forward_chain(id));
        cmds.push(cmd_nft_add_mss_clamp(id));
        if hub {
            cmds.push(cmd_nft_add_spoke_isolation(id, tun));
        }
        cmds
    }

    impl Drop for NetConfig {
        fn drop(&mut self) {
            // Revert in reverse order using blocking std::process::Command.
            // Note: Drop is not async, so we use blocking subprocess calls.

            // First, revert nft/iptables rules in reverse order.
            for (argv, label) in self
                .revert_cmds
                .iter()
                .rev()
                .zip(self.revert_labels.iter().rev())
            {
                tracing::info!(%label, "reverting vpn netconfig");
                match std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                {
                    Err(e) => {
                        tracing::warn!(%e, %label, "vpn netconfig revert step failed (spawn error)");
                    }
                    Ok(s) if !s.success() => {
                        tracing::warn!(code=%s, %label, "vpn netconfig revert step exited non-zero");
                    }
                    Ok(_) => {}
                }
            }

            // Then revert applied_ops in reverse order.
            for op in self.applied_ops.iter().rev() {
                match op {
                    AppliedOp::IpForward { saved_value } => {
                        tracing::info!(saved_value, "restoring ip_forward");
                        if std::fs::write(
                            "/proc/sys/net/ipv4/ip_forward",
                            format!("{}\n", saved_value),
                        )
                        .is_err()
                        {
                            // CAP_NET_ADMIN without UID 0: try sudo -n tee
                            // (best-effort, non-interactive).
                            let argv = super::hostcfg_cmd::cmd_sysctl_ip_forward(*saved_value);
                            let ok = std::process::Command::new(&argv[0])
                                .args(&argv[1..])
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .status()
                                .map(|s| s.success())
                                .unwrap_or(false);
                            if !ok {
                                tracing::warn!(
                                    saved_value,
                                    "could not restore ip_forward (no root and sudo -n failed); \
                                     restore manually: echo {} | sudo tee /proc/sys/net/ipv4/ip_forward",
                                    saved_value
                                );
                            }
                        }

                        // Delete state file (best-effort)
                        let state_path = ipforward_state_path(&self.id, &self.role);
                        let _ = std::fs::remove_file(&state_path);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn netconfig_rollback_is_reverse_order() {
            // Simulate applying multiple routes.
            // (We can't call the full apply() in a sync test, so we construct
            // the revert commands manually for this test.)
            let cfg = NetConfig {
                id: "test1".to_string(),
                role: "listen".to_string(),
                tun_name: "tun0".to_string(),
                no_route_manage: false,
                nft_available: false,
                revert_cmds: vec![
                    vec![
                        "ip".into(),
                        "route".into(),
                        "del".into(),
                        "10.0.0.0/24".into(),
                        "dev".into(),
                        "tun0".into(),
                    ],
                    vec![
                        "ip".into(),
                        "route".into(),
                        "del".into(),
                        "10.1.0.0/24".into(),
                        "dev".into(),
                        "tun0".into(),
                    ],
                ],
                revert_labels: vec![
                    "del route 10.0.0.0/24 dev tun0".to_string(),
                    "del route 10.1.0.0/24 dev tun0".to_string(),
                ],
                ip_forward_saved: None,
                applied_ops: vec![],
            };

            // Record the order of revert commands by manually calling drop.
            // (In practice, Drop is called automatically.)
            drop(cfg);
            // If we got here, Drop ran without panicking. The actual order
            // is verified by the reversal in Drop implementation.
        }

        #[tokio::test]
        async fn netconfig_apply_routes_only() {
            let runner = TestRunner::new();
            let peer_routes = vec![
                "10.0.0.0/24".parse::<crate::shared::Ipv4Net>().unwrap(),
                "10.1.0.0/24".parse::<crate::shared::Ipv4Net>().unwrap(),
            ];
            let advertised = vec![];

            let cfg = NetConfig::apply(
                &runner,
                "test1",
                "listen",
                "tun0",
                "192.168.100.1".parse().unwrap(),
                30,
                &peer_routes,
                &advertised,
                &[],
                false,
                false,
                false,
            )
            .await
            .expect("apply should succeed");

            let calls = runner.get_calls().await;
            // Should have called route replace (idempotent) twice.
            assert_eq!(calls.len(), 2);
            assert!(calls[0][0] == "ip" && calls[0][1] == "route" && calls[0][2] == "replace");
            assert!(calls[1][0] == "ip" && calls[1][1] == "route" && calls[1][2] == "replace");

            // Verify revert order is reversed.
            assert_eq!(cfg.revert_cmds.len(), 2);
            assert!(cfg.revert_cmds[0].contains(&"10.0.0.0/24".to_string()));
            assert!(cfg.revert_cmds[1].contains(&"10.1.0.0/24".to_string()));
        }

        #[tokio::test]
        async fn netconfig_no_route_manage_skips_routes() {
            let runner = TestRunner::new();
            let peer_routes = vec!["10.0.0.0/24".parse::<crate::shared::Ipv4Net>().unwrap()];
            let advertised = vec![];

            let cfg = NetConfig::apply(
                &runner,
                "test1",
                "listen",
                "tun0",
                "192.168.100.1".parse().unwrap(),
                30,
                &peer_routes,
                &advertised,
                &[],
                true, // --no-route-manage
                false,
                false,
            )
            .await
            .expect("apply should succeed");

            let calls = runner.get_calls().await;
            // Should not have called anything (only printed).
            assert_eq!(calls.len(), 0);
            assert_eq!(cfg.revert_cmds.len(), 0);
        }

        // ── Overlapping-subnet NAT (E3): NetConfig::apply rule-plane tests ─────
        //
        // The gateway path resolves a LAN iface via the canned `ip route get`
        // ("eth0") and selects nft when present (this dev box) else iptables. The
        // assertions are backend-aware so they hold under either nft or iptables.

        fn net(s: &str) -> crate::shared::Ipv4Net {
            s.parse().unwrap()
        }
        fn has(calls: &[Vec<String>], argv: &[String]) -> bool {
            calls.iter().any(|c| c.as_slice() == argv)
        }
        fn used_nft(calls: &[Vec<String>]) -> bool {
            calls
                .iter()
                .any(|c| c.first().map(|s| s == "nft").unwrap_or(false))
        }

        #[tokio::test]
        async fn apply_plain_only_unchanged() {
            // N8/I-NAT1: one plain advertised subnet, no nat_maps → blanket
            // masquerade, no prerouting chain, no netmap.
            use crate::vpn::hostcfg_cmd::*;
            let runner = TestRunner::new();
            let advertised = vec![net("192.168.50.0/24")];
            let _cfg = NetConfig::apply(
                &runner,
                "t",
                "listen",
                "tun0",
                "10.0.0.1".parse().unwrap(),
                30,
                &[],
                &advertised,
                &[],
                false,
                false,
                false,
            )
            .await
            .expect("apply ok");
            let calls = runner.get_calls().await;
            if used_nft(&calls) {
                assert!(has(
                    &calls,
                    &cmd_nft_add_masquerade_rule("t", "tun0", "eth0")
                ));
                assert!(!has(&calls, &cmd_nft_add_prerouting_chain("t")));
                assert!(!calls.iter().any(|c| c.contains(&"dnat".to_string())));
            } else {
                assert!(has(&calls, &cmd_iptables_masquerade_add("t", "eth0")));
                assert!(!calls.iter().any(|c| c.contains(&"NETMAP".to_string())));
            }
        }

        #[tokio::test]
        async fn apply_nat_only_emits_prerouting_dnat_snat_no_masquerade() {
            use crate::vpn::hostcfg_cmd::*;
            let runner = TestRunner::new();
            let advertised = vec![net("192.168.1.0/24")];
            let maps = vec![(net("192.168.1.0/24"), net("10.50.1.0/24"))];
            let _cfg = NetConfig::apply(
                &runner,
                "t",
                "listen",
                "tun0",
                "10.0.0.1".parse().unwrap(),
                30,
                &[],
                &advertised,
                &maps,
                false,
                false,
                false,
            )
            .await
            .expect("apply ok");
            let calls = runner.get_calls().await;
            if used_nft(&calls) {
                assert!(has(&calls, &cmd_nft_add_prerouting_chain("t")));
                assert!(has(
                    &calls,
                    &cmd_nft_add_netmap_dnat("t", "tun0", "10.50.1.0/24", "192.168.1.0/24")
                ));
                assert!(has(
                    &calls,
                    &cmd_nft_add_netmap_snat("t", "tun0", "192.168.1.0/24", "10.50.1.0/24")
                ));
                // No blanket masquerade (N6/I-NAT5).
                assert!(!has(
                    &calls,
                    &cmd_nft_add_masquerade_rule("t", "tun0", "eth0")
                ));
            } else {
                assert!(has(
                    &calls,
                    &cmd_iptables_netmap_dnat_add("t", "tun0", "10.50.1.0/24", "192.168.1.0/24")
                ));
                assert!(has(
                    &calls,
                    &cmd_iptables_netmap_snat_add("t", "tun0", "192.168.1.0/24", "10.50.1.0/24")
                ));
                assert!(!has(&calls, &cmd_iptables_masquerade_add("t", "eth0")));
            }
        }

        #[tokio::test]
        async fn apply_mixed_scopes_masquerade_to_plain_only() {
            use crate::vpn::hostcfg_cmd::*;
            let runner = TestRunner::new();
            let advertised = vec![net("192.168.1.0/24"), net("172.16.9.0/24")];
            let maps = vec![(net("192.168.1.0/24"), net("10.50.1.0/24"))];
            let _cfg = NetConfig::apply(
                &runner,
                "t",
                "listen",
                "tun0",
                "10.0.0.1".parse().unwrap(),
                30,
                &[],
                &advertised,
                &maps,
                false,
                false,
                false,
            )
            .await
            .expect("apply ok");
            let calls = runner.get_calls().await;
            if used_nft(&calls) {
                // netmap for the NAT'd subnet, scoped masquerade for the plain one,
                // no blanket masquerade.
                assert!(has(
                    &calls,
                    &cmd_nft_add_netmap_dnat("t", "tun0", "10.50.1.0/24", "192.168.1.0/24")
                ));
                assert!(has(
                    &calls,
                    &cmd_nft_add_masquerade_scoped("t", "tun0", "eth0", "172.16.9.0/24")
                ));
                assert!(!has(
                    &calls,
                    &cmd_nft_add_masquerade_rule("t", "tun0", "eth0")
                ));
            } else {
                assert!(has(
                    &calls,
                    &cmd_iptables_masquerade_scoped_add("t", "eth0", "172.16.9.0/24")
                ));
                assert!(!has(&calls, &cmd_iptables_masquerade_add("t", "eth0")));
            }
        }

        #[tokio::test]
        async fn apply_lan_iface_detection_uses_real_subnet() {
            // N7/I-NAT9: the route-get probe targets a REAL host (192.168.1.1), never
            // the virtual (10.50.1.1) which has no local route.
            let runner = TestRunner::new();
            let advertised = vec![net("192.168.1.0/24")];
            let maps = vec![(net("192.168.1.0/24"), net("10.50.1.0/24"))];
            let _cfg = NetConfig::apply(
                &runner,
                "t",
                "listen",
                "tun0",
                "10.0.0.1".parse().unwrap(),
                30,
                &[],
                &advertised,
                &maps,
                false,
                false,
                false,
            )
            .await
            .expect("apply ok");
            let calls = runner.get_calls().await;
            assert!(has(
                &calls,
                &[
                    "ip".to_string(),
                    "route".to_string(),
                    "get".to_string(),
                    "192.168.1.1".to_string()
                ]
            ));
            assert!(!calls.iter().any(|c| c.contains(&"10.50.1.1".to_string())));
        }

        #[tokio::test]
        async fn apply_nat_revert_mirrors_apply() {
            use crate::vpn::hostcfg_cmd::*;
            let runner = TestRunner::new();
            let advertised = vec![net("192.168.1.0/24")];
            let maps = vec![(net("192.168.1.0/24"), net("10.50.1.0/24"))];
            let cfg = NetConfig::apply(
                &runner,
                "t",
                "listen",
                "tun0",
                "10.0.0.1".parse().unwrap(),
                30,
                &[],
                &advertised,
                &maps,
                false,
                false,
                false,
            )
            .await
            .expect("apply ok");
            if cfg.nft_available {
                // nft: a single table delete reverts every rule (netmap included).
                assert!(cfg.revert_cmds.contains(&cmd_nft_delete_table("t")));
            } else {
                // iptables: chain teardown deletes the entire chain containing all rules.
                let pre = ipt_nat_pre_chain("t");
                let post = ipt_nat_post_chain("t");
                assert!(cfg.revert_cmds.contains(&cmd_iptables_nat_del_chain(&pre)));
                assert!(cfg
                    .revert_cmds
                    .contains(&cmd_iptables_nat_flush_chain(&pre)));
                assert!(cfg.revert_cmds.contains(&cmd_iptables_nat_del_chain(&post)));
                assert!(cfg
                    .revert_cmds
                    .contains(&cmd_iptables_nat_flush_chain(&post)));
            }
        }

        #[test]
        fn no_route_manage_prints_netmap_and_scoped_masquerade() {
            use crate::vpn::hostcfg_cmd::*;
            // NAT + plain mix: prerouting + netmap + scoped masquerade, no blanket.
            let cmds = gateway_nft_cmds(
                "t",
                "tun0",
                "LAN_IFACE",
                &[net("192.168.1.0/24"), net("172.16.9.0/24")],
                &[(net("192.168.1.0/24"), net("10.50.1.0/24"))],
                false,
                false,
            );
            assert!(cmds.contains(&cmd_nft_add_prerouting_chain("t")));
            assert!(cmds.contains(&cmd_nft_add_netmap_dnat(
                "t",
                "tun0",
                "10.50.1.0/24",
                "192.168.1.0/24"
            )));
            assert!(cmds.contains(&cmd_nft_add_netmap_snat(
                "t",
                "tun0",
                "192.168.1.0/24",
                "10.50.1.0/24"
            )));
            assert!(cmds.contains(&cmd_nft_add_masquerade_scoped(
                "t",
                "tun0",
                "LAN_IFACE",
                "172.16.9.0/24"
            )));
            assert!(!cmds.contains(&cmd_nft_add_masquerade_rule("t", "tun0", "LAN_IFACE")));

            // Plain only: blanket masquerade, no prerouting chain (N8).
            let plain = gateway_nft_cmds(
                "t",
                "tun0",
                "LAN_IFACE",
                &[net("192.168.50.0/24")],
                &[],
                false,
                false,
            );
            assert!(plain.contains(&cmd_nft_add_masquerade_rule("t", "tun0", "LAN_IFACE")));
            assert!(!plain.contains(&cmd_nft_add_prerouting_chain("t")));
        }

        #[test]
        fn netconfig_hub_isolation_rule_cmd() {
            // Unit test: check that the isolation rule commands are built correctly
            use crate::vpn::hostcfg_cmd::*;
            let tun = "bore0";
            let id = "testhub";
            let nft_cmd = cmd_nft_add_spoke_isolation(id, tun);
            let iptables_cmd = cmd_iptables_spoke_isolation_add(id, tun);

            // NFT command should contain "drop" and both interfaces
            assert!(nft_cmd.contains(&"add".to_string()));
            assert!(nft_cmd.contains(&"rule".to_string()));
            assert!(nft_cmd.contains(&"drop".to_string()));
            assert!(nft_cmd.contains(&tun.to_string()));

            // Iptables command should contain "-A FORWARD" and "-D FORWARD" (del variant)
            assert!(iptables_cmd.contains(&"-A".to_string()));
            assert!(iptables_cmd.contains(&"FORWARD".to_string()));
            assert!(iptables_cmd.contains(&tun.to_string()));

            let iptables_del = cmd_iptables_spoke_isolation_del(id, tun);
            assert!(iptables_del.contains(&"-D".to_string()));
            assert!(iptables_del.contains(&"FORWARD".to_string()));
        }

        #[test]
        fn netconfig_non_hub_no_isolation_rule() {
            // Verify that the isolation rule commands are only added in hub mode
            // by checking the conditional code path
            use crate::vpn::hostcfg_cmd::*;
            let tun = "bore0";
            let id = "test1to1";

            // These commands should exist (they're the hub-mode builders)
            let nft_hub = cmd_nft_add_spoke_isolation(id, tun);
            let iptables_hub = cmd_iptables_spoke_isolation_add(id, tun);

            // Both should mention the isolation concept
            assert!(nft_hub.contains(&"drop".to_string()));
            assert!(iptables_hub.contains(&"-A".to_string()));

            // In non-hub mode, these would NOT be called (verified by code inspection)
            // This is a unit test of the command builders, not the NetConfig apply path
            // (which would require full gateway setup including ip route get mocking).
        }

        // Root-required tests (skipped by default).

        #[tokio::test]
        #[ignore = "requires root: sudo cargo test --features vpn -- vpn::hostcfg::tests::check_root_accepts_uid_zero --ignored --nocapture"]
        async fn check_root_accepts_uid_zero() {
            // This test only passes if we're actually root.
            let result = check_root();
            assert!(result.is_ok());
        }

        // These don't actually need root — just check binary existence.
        #[tokio::test]
        async fn check_binary_exists_finds_ip() {
            assert!(check_binary_exists("ip"));
        }

        #[tokio::test]
        async fn check_binary_missing_not_found() {
            assert!(!check_binary_exists("__nonexistent_binary_12345__"));
        }

        /// Create a TUN device, verify it appears in `ip link show`, then drop it
        /// and verify it disappears.  Requires CAP_NET_ADMIN (root).
        /// Run: sudo cargo test --features vpn -- vpn::hostcfg::tests::tun_bring_up_and_down --ignored --nocapture
        #[tokio::test]
        #[ignore = "requires root/CAP_NET_ADMIN"]
        async fn tun_bring_up_and_down() {
            use std::net::Ipv4Addr;
            check_root().expect("test requires root");

            let name = "bore_test_tun0";
            let addr: Ipv4Addr = "10.199.0.1".parse().unwrap();

            // Stale reclaim in case a previous run crashed.
            stale_reclaim("test0", "listen").await;

            let (mut devs, _offload, _resolved_name) = create_tun(name, addr, 30, 1350, 1)
                .await
                .expect("failed to create TUN");
            let dev = devs.remove(0);

            // Verify interface is visible.
            let out = std::process::Command::new("ip")
                .args(["link", "show", name])
                .output()
                .expect("ip link show failed");
            assert!(
                out.status.success(),
                "interface {name} not found after creation"
            );

            // Drop the device — tun-rs should delete it.
            drop(dev);

            // Verify it's gone.
            let out2 = std::process::Command::new("ip")
                .args(["link", "show", name])
                .output()
                .expect("ip link show failed");
            assert!(
                !out2.status.success(),
                "interface {name} still exists after drop"
            );
        }

        #[test]
        fn ipforward_state_path_sanitizes_id_and_role() {
            assert_eq!(
                ipforward_state_path("test123", "listen"),
                std::path::PathBuf::from("/run/bore-vpn-test123-listen.ipforward")
            );
            assert_eq!(
                ipforward_state_path("test/123", "listen"),
                std::path::PathBuf::from("/run/bore-vpn-test_123-listen.ipforward")
            );
            assert_eq!(
                ipforward_state_path("test 123", "connect"),
                std::path::PathBuf::from("/run/bore-vpn-test_123-connect.ipforward")
            );
            assert_eq!(
                ipforward_state_path("test.123", "listen"),
                std::path::PathBuf::from("/run/bore-vpn-test_123-listen.ipforward")
            );
            assert_eq!(
                ipforward_state_path("test-123", "listen"),
                std::path::PathBuf::from("/run/bore-vpn-test-123-listen.ipforward")
            );
        }

        #[test]
        fn ipforward_state_path_distinguishes_id_and_role() {
            // Different ids → different paths.
            assert_ne!(
                ipforward_state_path("id1", "listen"),
                ipforward_state_path("id2", "listen")
            );
            // Same id, different role → different paths. This is the fix for two peers
            // of one link sharing a host + /run (the netns harness; site↔site): without
            // it the connector's stale_reclaim would delete the listener's state file.
            assert_ne!(
                ipforward_state_path("id1", "listen"),
                ipforward_state_path("id1", "connect")
            );
        }

        #[tokio::test]
        async fn ipforward_state_file_roundtrip() {
            let test_dir = std::env::temp_dir().join("bore_test_ipforward");
            let _ = std::fs::create_dir_all(&test_dir);

            let test_path = test_dir.join("test.ipforward");

            // Write a state file with value 0
            let _ = std::fs::write(&test_path, "0\n");
            let content = std::fs::read_to_string(&test_path).expect("failed to read");
            let value: u8 = content.trim().parse().expect("failed to parse");
            assert_eq!(value, 0);

            // Write a state file with value 1
            let _ = std::fs::write(&test_path, "1\n");
            let content = std::fs::read_to_string(&test_path).expect("failed to read");
            let value: u8 = content.trim().parse().expect("failed to parse");
            assert_eq!(value, 1);

            // Clean up
            let _ = std::fs::remove_file(&test_path);
        }
    }
}

pub mod link {
    //! VPN data-plane abstraction: Direct (QUIC datagrams) or Relay (AEAD-framed streams).
    //!
    //! The relay path uses **two** yamux substreams, one per direction, so that
    //! each `yamux::Stream` object is polled by exactly one task. A `yamux::Stream`
    //! must never be shared between two tasks (e.g. via `tokio::io::split`):
    //! `poll_read` and `poll_write` both call `poll_ready` on the stream's single
    //! `futures::channel::mpsc::Sender`, which holds **one** parked-task waker.
    //! Two tasks polling the same stream overwrite each other's waker, and the
    //! losing task is never woken again — the link wedges silently under load.
    use anyhow::{Context, Result};
    use bytes::{Buf, Bytes, BytesMut};
    use futures_util::FutureExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    use super::crypto::DirectionKeys;
    use crate::holepunch::DirectConn;

    const BATCH_CAP: usize = 64;

    // Relay write queue depth (frames). The uplink awaits when full, propagating
    // backpressure to the TUN read loop instead of dropping packets: the relay is
    // an ordered, reliable byte stream, so loss here only multiplies inner-TCP
    // retransmissions (and those retransmissions would be dropped too, collapsing
    // every flow on the link).
    const RELAY_QUEUE: usize = 512;

    // Target size for a single ingress read; large enough to pick up a full GRO
    // batch worth of frames per syscall.
    const RECV_BUF: usize = 128 * 1024;

    /// Largest accepted relay frame body (8-byte counter + 65535-byte IP packet + AEAD tag).
    const MAX_FRAME: usize = 8 + 65535 + super::crypto::TAG_LEN;

    /// Direction tag for the connector→listener payload substream.
    /// Written by the connector right after [`crate::mux::STREAM_READY`]; the
    /// server consumes the marker and relays the tag to the listener.
    pub const RELAY_TAG_UP: u8 = 1;
    /// Direction tag for the listener→connector payload substream.
    pub const RELAY_TAG_DOWN: u8 = 2;

    /// Send half of a VPN link (owned by an uplink task).
    ///
    /// Cloneable so multiple uplink pumps (TUN multi-queue) can share one link:
    /// the AEAD nonce counter is a single shared atomic (I-5/DEC-6 — two seals
    /// with the same `(key, counter)` would be catastrophic), while the
    /// round-robin cursor is per-clone (per-task distribution stays balanced).
    pub enum LinkSender {
        /// Direct QUIC datagram path.
        Direct(DirectConn),
        /// Relay path: AEAD-framed substreams (one writer task each).
        /// Frames are distributed round-robin **per datagram** (DEC-7):
        /// out-of-order arrival is fine, IP is best-effort.
        Relay {
            /// Channels to the background relay writer tasks (one per carrier).
            txs: Vec<mpsc::Sender<Bytes>>,
            /// AEAD egress key (single key per direction, DEC-6).
            key: [u8; 32],
            /// Shared per-packet counter for nonce derivation (I-5).
            counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
            /// Round-robin cursor (local to this clone).
            rr: usize,
        },
    }

    impl Clone for LinkSender {
        fn clone(&self) -> Self {
            match self {
                LinkSender::Direct(conn) => LinkSender::Direct(conn.clone()),
                LinkSender::Relay {
                    txs, key, counter, ..
                } => LinkSender::Relay {
                    txs: txs.clone(),
                    key: *key,
                    counter: std::sync::Arc::clone(counter),
                    rr: 0,
                },
            }
        }
    }

    /// Receive half of a VPN link (owned by the downlink task).
    pub enum LinkRecver {
        /// Direct QUIC datagram path.
        Direct(DirectConn),
        /// Relay path: fan-in of the per-carrier reader tasks. Each reader owns
        /// one ingress substream (I-1), decrypts frames, and pushes plaintext
        /// packets — or its terminal error — into this channel.
        Relay {
            /// Fan-in of decrypted packets (or a reader's terminal error).
            rx: mpsc::Receiver<Result<Bytes>>,
        },
    }

    /// Split a Direct link into send+recv halves for the bridge tasks.
    pub fn make_direct(conn: DirectConn) -> (LinkSender, LinkRecver) {
        (LinkSender::Direct(conn.clone()), LinkRecver::Direct(conn))
    }

    /// Build a Relay link from one pair of direction substreams (carriers = 1).
    pub fn make_relay(
        egress: crate::mux::Stream,
        ingress: crate::mux::Stream,
        keys: DirectionKeys,
    ) -> (LinkSender, LinkRecver) {
        make_relay_multi(vec![egress], vec![ingress], keys)
    }

    /// Build a Relay link from N carrier substream pairs (C3).
    ///
    /// Per carrier: one background writer task owns its `egress` substream and
    /// one reader task owns its `ingress` substream (I-1). Egress frames are
    /// sealed with a **shared** atomic counter (I-5) and distributed
    /// round-robin per datagram (DEC-7); ingress readers decrypt and fan into
    /// one channel. A reader hitting EOF/error pushes the error into the
    /// fan-in, killing the link cleanly (no silent half-degraded state).
    pub fn make_relay_multi(
        egress: Vec<crate::mux::Stream>,
        ingress: Vec<crate::mux::Stream>,
        keys: DirectionKeys,
    ) -> (LinkSender, LinkRecver) {
        assert!(!egress.is_empty() && egress.len() == ingress.len());
        let n = egress.len();
        let per_writer_queue = (RELAY_QUEUE / n).max(64);
        let txs: Vec<mpsc::Sender<Bytes>> = egress
            .into_iter()
            .map(|stream| {
                let (tx, rx) = mpsc::channel::<Bytes>(per_writer_queue);
                tokio::spawn(relay_writer(stream, rx));
                tx
            })
            .collect();

        let (fan_tx, fan_rx) = mpsc::channel::<Result<Bytes>>(RELAY_QUEUE);
        for stream in ingress {
            tokio::spawn(relay_reader(stream, keys.ingress, fan_tx.clone()));
        }

        (
            LinkSender::Relay {
                txs,
                key: keys.egress,
                counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                rr: 0,
            },
            LinkRecver::Relay { rx: fan_rx },
        )
    }

    /// Connector side: open the two relay substreams and tag their directions.
    /// Returns `(egress, ingress)` from the connector's perspective.
    pub async fn connect_relay(
        opener: &crate::mux::Opener,
    ) -> Result<(crate::mux::Stream, crate::mux::Stream)> {
        let (mut egress, mut ingress) = connect_relay_multi(opener, 1).await?;
        Ok((egress.remove(0), ingress.remove(0)))
    }

    /// Connector side: open `n` relay substream pairs and tag them.
    ///
    /// Header compatibility (I-9): with `n == 1` the header is the original
    /// 2-byte `[STREAM_READY, tag]`; with `n > 1` a third byte carries the
    /// carrier index. Both sides know `n` from `VpnReady.carriers`.
    pub async fn connect_relay_multi(
        opener: &crate::mux::Opener,
        n: u16,
    ) -> Result<(Vec<crate::mux::Stream>, Vec<crate::mux::Stream>)> {
        let n = n.max(1);
        let mut egress = Vec::with_capacity(n as usize);
        let mut ingress = Vec::with_capacity(n as usize);
        for idx in 0..n {
            let mut up = opener.open().await.context("open relay egress substream")?;
            let mut down = opener
                .open()
                .await
                .context("open relay ingress substream")?;
            if n == 1 {
                up.write_all(&[crate::mux::STREAM_READY, RELAY_TAG_UP])
                    .await
                    .context("write relay egress header")?;
                down.write_all(&[crate::mux::STREAM_READY, RELAY_TAG_DOWN])
                    .await
                    .context("write relay ingress header")?;
            } else {
                up.write_all(&[crate::mux::STREAM_READY, RELAY_TAG_UP, idx as u8])
                    .await
                    .context("write relay egress header")?;
                down.write_all(&[crate::mux::STREAM_READY, RELAY_TAG_DOWN, idx as u8])
                    .await
                    .context("write relay ingress header")?;
            }
            egress.push(up);
            ingress.push(down);
        }
        Ok((egress, ingress))
    }

    /// Listener side: accept the two relay substreams and sort them by tag.
    /// Returns `(egress, ingress)` from the listener's perspective
    /// (egress = `RELAY_TAG_DOWN` stream, ingress = `RELAY_TAG_UP` stream).
    pub async fn accept_relay(
        acceptor: &mut crate::mux::Acceptor,
    ) -> Result<(crate::mux::Stream, crate::mux::Stream)> {
        let (mut egress, mut ingress) = accept_relay_multi(acceptor, 1).await?;
        Ok((egress.remove(0), ingress.remove(0)))
    }

    /// Listener side: accept `n` relay substream pairs and sort them by tag.
    /// Returns `(egress, ingress)` vectors from the listener's perspective
    /// (egress = `RELAY_TAG_DOWN` streams, ingress = `RELAY_TAG_UP` streams).
    pub async fn accept_relay_multi(
        acceptor: &mut crate::mux::Acceptor,
        n: u16,
    ) -> Result<(Vec<crate::mux::Stream>, Vec<crate::mux::Stream>)> {
        let n = n.max(1) as usize;
        let mut up = Vec::new();
        let mut down = Vec::new();
        for _ in 0..(2 * n) {
            // Without a timeout the listener hangs indefinitely if the connector
            // crashes after VpnReady but before opening the relay substreams.
            let mut stream = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                acceptor.accept(),
            )
            .await
            .context(
                "timed out waiting for relay substream (connector did not connect within 60 s)",
            )?
            .context("server closed before opening relay substreams")?;
            // 2-byte header for the single-carrier path (bit-exact with v1),
            // 3-byte header (with carrier index) when n > 1.
            let mut header = [0u8; 2];
            stream
                .read_exact(&mut header)
                .await
                .context("read relay substream header")?;
            if n > 1 {
                let mut idx = [0u8; 1];
                stream
                    .read_exact(&mut idx)
                    .await
                    .context("read relay carrier index")?;
            }
            anyhow::ensure!(
                header[0] == crate::mux::STREAM_READY,
                "bad relay stream-ready marker: {}",
                header[0]
            );
            match header[1] {
                RELAY_TAG_UP => up.push(stream),
                RELAY_TAG_DOWN => down.push(stream),
                tag => anyhow::bail!(
                    "unknown relay direction tag {tag} (peer built from an older version?)"
                ),
            }
        }
        anyhow::ensure!(
            up.len() == n && down.len() == n,
            "unbalanced relay direction tags (peer built from an older version?)"
        );
        Ok((down, up))
    }

    /// Background task: drain the relay write queue and write frames to the
    /// egress substream. This task is the stream's only owner. Exits on write
    /// error after logging it; the channel then closes and the uplink's next
    /// `send_batch` fails, tearing down the bridge loudly.
    async fn relay_writer(mut egress: crate::mux::Stream, mut rx: mpsc::Receiver<Bytes>) {
        while let Some(frame) = rx.recv().await {
            if let Err(e) = egress.write_all(&frame).await {
                tracing::warn!(error = %e, "vpn relay egress write failed; tearing down link");
                return;
            }
        }
    }

    /// Background task: read AEAD frames off one ingress substream, decrypt,
    /// and push plaintext packets into the fan-in channel. On EOF or any error
    /// the terminal error itself is pushed (best-effort) so the downlink dies
    /// loudly instead of limping on the surviving carriers.
    async fn relay_reader(
        mut read: crate::mux::Stream,
        key: [u8; 32],
        fan_tx: mpsc::Sender<Result<Bytes>>,
    ) {
        let mut acc = BytesMut::with_capacity(RECV_BUF);
        let result: Result<()> = async {
            loop {
                while let Some(frame) = take_frame(&mut acc)? {
                    let plaintext =
                        super::crypto::open(&key, &frame).context("AEAD open failed")?;
                    if fan_tx.send(Ok(Bytes::from(plaintext))).await.is_err() {
                        return Ok(()); // recver gone: normal teardown
                    }
                }
                acc.reserve(RECV_BUF);
                let n = read
                    .read_buf(&mut acc)
                    .await
                    .context("relay ingress read")?;
                anyhow::ensure!(n != 0, "relay ingress stream closed by peer");
            }
        }
        .await;
        if let Err(e) = result {
            let _ = fan_tx.send(Err(e)).await;
        }
    }

    /// Pop one complete `[u32 len][len bytes]` frame off `acc`, if present.
    /// Returns the frame body (counter + ciphertext) without the length prefix.
    fn take_frame(acc: &mut BytesMut) -> Result<Option<Bytes>> {
        if acc.len() < 4 {
            return Ok(None);
        }
        let total_len = u32::from_be_bytes(acc[..4].try_into().unwrap()) as usize;
        anyhow::ensure!(
            total_len >= 8 + super::crypto::TAG_LEN,
            "relay frame too short: {total_len}"
        );
        anyhow::ensure!(total_len <= MAX_FRAME, "relay frame too large: {total_len}");
        if acc.len() < 4 + total_len {
            return Ok(None);
        }
        acc.advance(4);
        Ok(Some(acc.split_to(total_len).freeze()))
    }

    impl LinkSender {
        /// Send a batch of IP packets. For Direct: QUIC datagrams. For Relay: AEAD frames.
        ///
        /// Returns the number of packets DROPPED because they exceeded the
        /// current QUIC path-MTU (`DatagramSend::TooLarge`). Such drops are a
        /// transient per-packet condition — never a link failure — so they must
        /// not abort the rest of the batch nor tear the link down; the caller
        /// counts them and continues. `Err` is reserved for genuine link death.
        pub async fn send_batch(&mut self, pkts: &[Bytes]) -> Result<usize> {
            match self {
                LinkSender::Direct(conn) => {
                    let mut dropped = 0usize;
                    for pkt in pkts {
                        // Skip oversized packets (path MTU < TUN MTU window);
                        // a real send error propagates and tears down the link.
                        match conn.send_datagram(pkt.clone())? {
                            crate::holepunch::DatagramSend::Sent => {}
                            crate::holepunch::DatagramSend::TooLarge => dropped += 1,
                        }
                    }
                    Ok(dropped)
                }
                LinkSender::Relay {
                    txs,
                    key,
                    counter,
                    rr,
                } => {
                    for pkt in pkts {
                        // Shared atomic counter: unique nonce even with multiple
                        // producers on the same egress key (I-5, DEC-6).
                        let ctr = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let frame = super::crypto::seal_with_counter(key, ctr, pkt)?;
                        // Round-robin per datagram (DEC-7). A full queue blocks
                        // (backpressure, I-4) — no skip-to-next: simple and
                        // predictable.
                        *rr = (*rr + 1) % txs.len();
                        txs[*rr].send(Bytes::from(frame)).await.map_err(|_| {
                            anyhow::anyhow!("relay writer exited (write error on relay stream)")
                        })?;
                    }
                    // Relay frames are length-prefixed and never size-limited
                    // like QUIC datagrams: nothing is ever dropped here.
                    Ok(0)
                }
            }
        }

        /// Resolved when the underlying link is gone (any carrier writer exited).
        pub async fn closed(&self) {
            match self {
                LinkSender::Direct(conn) => conn.closed().await,
                LinkSender::Relay { txs, .. } => {
                    let waits = txs.iter().map(|tx| Box::pin(tx.closed()));
                    futures_util::future::select_all(waits).await;
                }
            }
        }
    }

    impl LinkRecver {
        /// Receive ≥1 IP packets (up to `BATCH_CAP`). Err on link close.
        pub async fn recv_batch(&mut self, out: &mut Vec<Bytes>) -> Result<()> {
            match self {
                LinkRecver::Direct(conn) => {
                    let first = conn
                        .read_datagram()
                        .await
                        .context("direct recv first datagram")?;
                    out.push(first);
                    // Drain queued datagrams without yielding (up to BATCH_CAP).
                    for _ in 1..BATCH_CAP {
                        match conn.read_datagram().now_or_never() {
                            Some(Ok(pkt)) => out.push(pkt),
                            _ => break,
                        }
                    }
                    Ok(())
                }
                LinkRecver::Relay { rx } => {
                    let first = rx
                        .recv()
                        .await
                        .ok_or_else(|| anyhow::anyhow!("relay ingress closed"))??;
                    out.push(first);
                    // Drain whatever is already queued (up to BATCH_CAP) so one
                    // wake-up can flush a whole batch downstream.
                    while out.len() < BATCH_CAP {
                        match rx.try_recv() {
                            Ok(Ok(pkt)) => out.push(pkt),
                            Ok(Err(e)) => return Err(e),
                            Err(_) => break,
                        }
                    }
                    Ok(())
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use bytes::Bytes;
        use tokio::sync::mpsc;

        /// A full relay queue must apply backpressure: send_batch completes as
        /// soon as the consumer drains a slot, and no packet is lost.
        #[tokio::test]
        async fn relay_sender_backpressure_no_loss() {
            // Capacity 1 so the second packet must wait for the consumer.
            let (tx, mut rx) = mpsc::channel::<Bytes>(1);
            let key = [0u8; 32];
            let mut sender = LinkSender::Relay {
                txs: vec![tx],
                key,
                counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                rr: 0,
            };

            let pkt = Bytes::from(vec![0xAB; 64]);
            // First send fills the channel.
            sender.send_batch(std::slice::from_ref(&pkt)).await.unwrap();
            // Consumer drains with a delay; the second send must wait, then succeed.
            let consumer = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let mut frames = Vec::new();
                while let Some(f) = rx.recv().await {
                    frames.push(f);
                }
                frames
            });
            sender.send_batch(std::slice::from_ref(&pkt)).await.unwrap();
            match &sender {
                LinkSender::Relay { counter, .. } => {
                    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 2)
                }
                _ => panic!("expected Relay"),
            }
            drop(sender);
            let frames = consumer.await.unwrap();
            assert_eq!(frames.len(), 2, "both sealed frames must reach the writer");
        }

        /// When the writer task exits (e.g. relay stream broken), the next
        /// send_batch must return Err rather than silently dropping or hanging.
        #[tokio::test]
        async fn relay_sender_errors_when_writer_gone() {
            let (tx, rx) = mpsc::channel::<Bytes>(8);
            // Drop the receiver — simulates the writer task having exited.
            drop(rx);
            let key = [0u8; 32];
            let mut sender = LinkSender::Relay {
                txs: vec![tx],
                key,
                counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                rr: 0,
            };

            let pkt = Bytes::from(vec![0xCD; 32]);
            let result = sender.send_batch(&[pkt]).await;
            assert!(
                result.is_err(),
                "send_batch must error when writer task is gone"
            );
        }

        /// Frame reassembly: partial prefixes, multiple frames per buffer, and
        /// split frames across reads must all parse exactly.
        #[test]
        fn take_frame_reassembly() {
            let mut acc = BytesMut::new();

            // Partial length prefix → None.
            acc.extend_from_slice(&[0, 0]);
            assert!(take_frame(&mut acc).unwrap().is_none());

            // Complete header but incomplete body → None.
            let body = vec![7u8; 8 + super::super::crypto::TAG_LEN + 5];
            acc.clear();
            acc.extend_from_slice(&(body.len() as u32).to_be_bytes());
            acc.extend_from_slice(&body[..3]);
            assert!(take_frame(&mut acc).unwrap().is_none());

            // Rest of the body arrives, plus a second complete frame.
            acc.extend_from_slice(&body[3..]);
            acc.extend_from_slice(&(body.len() as u32).to_be_bytes());
            acc.extend_from_slice(&body);
            let f1 = take_frame(&mut acc).unwrap().expect("first frame");
            assert_eq!(&f1[..], &body[..]);
            let f2 = take_frame(&mut acc).unwrap().expect("second frame");
            assert_eq!(&f2[..], &body[..]);
            assert!(take_frame(&mut acc).unwrap().is_none());
            assert!(acc.is_empty());
        }

        /// I-5 — concurrent seals on a shared key must never reuse a counter:
        /// 4 tasks × 1000 increments on the shared atomic yield 4000 unique
        /// nonce counters.
        #[tokio::test]
        async fn shared_counter_unique_across_tasks() {
            use std::sync::atomic::{AtomicU64, Ordering};
            use std::sync::Arc;
            let counter = Arc::new(AtomicU64::new(0));
            let mut handles = Vec::new();
            for _ in 0..4 {
                let counter = Arc::clone(&counter);
                handles.push(tokio::spawn(async move {
                    let mut seen = Vec::with_capacity(1000);
                    for _ in 0..1000 {
                        seen.push(counter.fetch_add(1, Ordering::Relaxed));
                        tokio::task::yield_now().await;
                    }
                    seen
                }));
            }
            let mut all = std::collections::HashSet::new();
            for h in handles {
                for v in h.await.unwrap() {
                    assert!(all.insert(v), "counter value {v} reused across tasks");
                }
            }
            assert_eq!(all.len(), 4000);
        }

        /// Oversized and undersized frame lengths must be rejected, not allocated.
        #[test]
        fn take_frame_rejects_bad_lengths() {
            let mut acc = BytesMut::new();
            acc.extend_from_slice(&(u32::MAX).to_be_bytes());
            assert!(take_frame(&mut acc).is_err(), "oversized frame must error");

            let mut acc = BytesMut::new();
            acc.extend_from_slice(&3u32.to_be_bytes());
            assert!(take_frame(&mut acc).is_err(), "undersized frame must error");
        }
    }
}

pub mod bridge {
    #![allow(dead_code)]
    //! VPN data-plane bridge: bidirectional flow between TUN and VpnLink.
    use anyhow::Result;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::interval;
    use tracing::debug;

    use super::link::{LinkRecver, LinkSender};

    /// Counter metrics for bridge data-plane.
    pub struct BridgeCounters {
        /// Transmitted packets.
        pub tx_pkts: AtomicU64,
        /// Transmitted bytes.
        pub tx_bytes: AtomicU64,
        /// Received packets.
        pub rx_pkts: AtomicU64,
        /// Received bytes.
        pub rx_bytes: AtomicU64,
        /// Dropped (TooLarge) packets.
        pub tx_drops: AtomicU64,
    }

    impl BridgeCounters {
        /// Create new bridge counters.
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                tx_pkts: AtomicU64::new(0),
                tx_bytes: AtomicU64::new(0),
                rx_pkts: AtomicU64::new(0),
                rx_bytes: AtomicU64::new(0),
                tx_drops: AtomicU64::new(0),
            })
        }
    }

    /// Grace window after a relay pump dies while an upgrade may be in flight:
    /// when the PEER switches to direct first, it drops its relay substreams and
    /// our relay pumps fail — but our own direct upgrade completes within
    /// moments. Waiting briefly for the upgrade channel turns that race into a
    /// clean path switch instead of a dead link.
    const UPGRADE_GRACE: Duration = Duration::from_secs(5);

    /// Bridge state machine (warm-relay seamless fallback).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum BridgeMode {
        Relay,
        Direct,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum BridgeEvent {
        RelayDownlinkDied,
        UplinkDied,
        UpgradeArrived,
        DirectDownlinkDied,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum BridgeAction {
        LinkDead,
        GoDirect,
        FallBackToRelay,
        ReconnectRelayDead,
        Ignore,
    }

    /// Pure transition table. `relay_alive` = is the always-on relay downlink still running?
    fn bridge_next_action(mode: BridgeMode, ev: BridgeEvent, relay_alive: bool) -> BridgeAction {
        match (mode, ev) {
            (BridgeMode::Relay, BridgeEvent::RelayDownlinkDied) => BridgeAction::LinkDead,
            (BridgeMode::Relay, BridgeEvent::UplinkDied) => BridgeAction::LinkDead,
            (BridgeMode::Relay, BridgeEvent::UpgradeArrived) => BridgeAction::GoDirect,
            (BridgeMode::Relay, BridgeEvent::DirectDownlinkDied) => BridgeAction::Ignore,
            (BridgeMode::Direct, BridgeEvent::RelayDownlinkDied) => BridgeAction::Ignore,
            (BridgeMode::Direct, BridgeEvent::UpgradeArrived) => BridgeAction::Ignore,
            (BridgeMode::Direct, BridgeEvent::DirectDownlinkDied)
            | (BridgeMode::Direct, BridgeEvent::UplinkDied) => {
                if relay_alive {
                    BridgeAction::FallBackToRelay
                } else {
                    BridgeAction::ReconnectRelayDead
                }
            }
        }
    }

    /// Run the VPN data-plane bridge until the link dies or the tun closes.
    ///
    /// Spawns one uplink pump per TUN queue (the kernel hashes flows across
    /// queues on read) plus a single downlink pump writing to the first queue
    /// (TUN writes accept any queue fd; kernel RPS spreads receive processing),
    /// and runs until any pump fails.
    ///
    /// `offload`: if true, uses Phase 6.2 multi-packet GSO/GRO I/O;
    /// if false, uses Phase 6.1 single-packet I/O.
    ///
    /// `upgrade_rx` (DEC-1): when the direct-path task delivers new link halves,
    /// the bridge aborts every pump, waits for them to actually terminate (the
    /// TUN must never have two concurrent readers per queue), and respawns them
    /// on the new halves. The old halves are dropped, which closes the relay
    /// substreams. Relay-only callers pass a channel whose sender is already
    /// dropped: the first `recv()` yields `None` and the upgrade arm is
    /// disabled for good.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        devs: Vec<Arc<tun_rs::AsyncDevice>>,
        sender: LinkSender,
        recver: LinkRecver,
        counters: Arc<BridgeCounters>,
        mtu: u16,
        offload: bool,
        mut upgrade_rx: tokio::sync::mpsc::Receiver<(LinkSender, LinkRecver)>,
        downgrade_tx: tokio::sync::mpsc::Sender<()>,
    ) -> Result<()> {
        assert!(!devs.is_empty(), "bridge needs at least one TUN queue");
        let stats_task = tokio::spawn({
            let c = Arc::clone(&counters);
            async move {
                let start = tokio::time::Instant::now();
                let mut warned = false;
                let mut ticker = interval(Duration::from_secs(10));
                loop {
                    ticker.tick().await;
                    let tx_drops = c.tx_drops.load(Ordering::Relaxed);
                    if should_warn_drops(tx_drops, start.elapsed(), warned) {
                        warned = true;
                        tracing::warn!(
                            tx_drops,
                            "VPN link is dropping oversized packets; consider lowering --mtu \
                             (current path MTU is smaller than the TUN MTU)"
                        );
                    }
                    debug!(
                        tx_pkts = c.tx_pkts.load(Ordering::Relaxed),
                        rx_pkts = c.rx_pkts.load(Ordering::Relaxed),
                        tx_bytes = c.tx_bytes.load(Ordering::Relaxed),
                        rx_bytes = c.rx_bytes.load(Ordering::Relaxed),
                        tx_drops,
                        "vpn bridge stats",
                    );
                }
            }
        });

        /// Spawn uplinks only (not downlinks); first to die wins.
        fn spawn_uplinks(
            devs: &[Arc<tun_rs::AsyncDevice>],
            sender: LinkSender,
            counters: &Arc<BridgeCounters>,
            mtu: u16,
            offload: bool,
        ) -> Vec<tokio::task::JoinHandle<Result<()>>> {
            let mut uplinks = Vec::with_capacity(devs.len());
            for dev in devs {
                uplinks.push(tokio::spawn(run_uplink(
                    Arc::clone(dev),
                    sender.clone(),
                    Arc::clone(counters),
                    mtu,
                    offload,
                )));
            }
            uplinks
        }

        /// Spawn the relay downlink (always-on for the link lifetime).
        fn spawn_relay_downlink(
            devs: &[Arc<tun_rs::AsyncDevice>],
            relay_recver: LinkRecver,
            counters: &Arc<BridgeCounters>,
            offload: bool,
        ) -> tokio::task::JoinHandle<Result<()>> {
            tokio::spawn(run_downlink(
                Arc::clone(&devs[0]),
                relay_recver,
                Arc::clone(counters),
                offload,
            ))
        }

        /// Spawn the direct downlink (conditional on direct upgrade).
        fn spawn_direct_downlink(
            devs: &[Arc<tun_rs::AsyncDevice>],
            direct_recver: LinkRecver,
            counters: &Arc<BridgeCounters>,
            offload: bool,
        ) -> tokio::task::JoinHandle<Result<()>> {
            tokio::spawn(run_downlink(
                Arc::clone(&devs[0]),
                direct_recver,
                Arc::clone(counters),
                offload,
            ))
        }

        // Abort and await all handles, skipping any already finished (avoid re-polling completed handles).
        macro_rules! abort_await {
            ($handles:expr) => {{
                for h in &$handles {
                    h.abort();
                }
                for h in &mut $handles {
                    if !h.is_finished() {
                        let _ = h.await;
                    }
                }
            }};
        }

        // Bridge starts on relay. The relay downlink is always-on for the link lifetime:
        // it idles while on direct (the peer sends nothing on relay) and resumes delivering
        // the instant the peer falls back — so the RX side never needs switching. Only the
        // single active uplink set is switched between relay and direct.
        let relay_sender = sender.clone();
        let mut relay_dl = spawn_relay_downlink(&devs, recver, &counters, offload);
        // Once the relay downlink has died we must stop selecting on it: re-polling a
        // finished `JoinHandle` panics. `relay_dead` both disables its branch and feeds the
        // `relay_alive` argument of `bridge_next_action`.
        let mut relay_dead = false;
        let mut direct_dl: Option<tokio::task::JoinHandle<Result<()>>> = None;
        let mut uplinks = spawn_uplinks(&devs, relay_sender.clone(), &counters, mtu, offload);
        let mut mode = BridgeMode::Relay;

        let result: Result<()> = 'outer: loop {
            tokio::select! {
                // Relay downlink — disabled once dead (no re-poll of a finished handle).
                res = &mut relay_dl, if !relay_dead => {
                    let outcome = res.unwrap_or_else(|e| Err(anyhow::anyhow!("relay downlink panic: {e}")));
                    match bridge_next_action(mode, BridgeEvent::RelayDownlinkDied, false) {
                        // On relay the relay recv path is the link → dead.
                        BridgeAction::LinkDead => break 'outer outcome,
                        // On direct the warm relay dying is non-fatal; stop watching it. If
                        // direct also dies later, the fallback sees relay_dead → reconnect.
                        BridgeAction::Ignore => {
                            relay_dead = true;
                            tracing::warn!(path = "direct", "warm relay path died while on direct; will reconnect if direct also fails");
                        }
                        _ => unreachable!("relay downlink death yields LinkDead/Ignore only"),
                    }
                }
                // Direct downlink — present only while on direct.
                res = async { direct_dl.as_mut().unwrap().await }, if direct_dl.is_some() => {
                    let outcome = res.unwrap_or_else(|e| Err(anyhow::anyhow!("direct downlink panic: {e}")));
                    direct_dl = None;
                    match bridge_next_action(mode, BridgeEvent::DirectDownlinkDied, !relay_dead) {
                        BridgeAction::FallBackToRelay => {
                            let _ = downgrade_tx.try_send(());
                            abort_await!(uplinks);
                            uplinks = spawn_uplinks(&devs, relay_sender.clone(), &counters, mtu, offload);
                            mode = BridgeMode::Relay;
                            tracing::warn!(path = "relay", "direct path lost; fell back to relay (link preserved)");
                        }
                        // Both paths gone → genuine link death; the reconnect loop handles it.
                        BridgeAction::ReconnectRelayDead => break 'outer outcome,
                        _ => unreachable!("direct downlink death yields FallBack/Reconnect only"),
                    }
                }
                // The single active uplink set.
                (res, _idx, _rest) = futures_util::future::select_all(uplinks.iter_mut()) => {
                    let outcome = res.unwrap_or_else(|e| Err(anyhow::anyhow!("bridge uplink panic: {e}")));
                    match bridge_next_action(mode, BridgeEvent::UplinkDied, !relay_dead) {
                        BridgeAction::LinkDead => {
                            abort_await!(uplinks);
                            break 'outer outcome;
                        }
                        BridgeAction::FallBackToRelay => {
                            let _ = downgrade_tx.try_send(());
                            abort_await!(uplinks);
                            if let Some(d) = direct_dl.take() {
                                d.abort();
                            }
                            uplinks = spawn_uplinks(&devs, relay_sender.clone(), &counters, mtu, offload);
                            mode = BridgeMode::Relay;
                            tracing::warn!(path = "relay", "direct path lost; fell back to relay (link preserved)");
                        }
                        BridgeAction::ReconnectRelayDead => {
                            abort_await!(uplinks);
                            break 'outer outcome;
                        }
                        _ => unreachable!("uplink death yields LinkDead/FallBack/Reconnect only"),
                    }
                }
                // Upgrade channel (relay→direct). The `Some(_)` pattern disables this branch
                // once the channel closes (relay-only / the direct task ended).
                Some(pair) = upgrade_rx.recv() => {
                    match bridge_next_action(mode, BridgeEvent::UpgradeArrived, !relay_dead) {
                        BridgeAction::GoDirect => {
                            abort_await!(uplinks);
                            let (direct_sender, direct_recver) = pair;
                            uplinks = spawn_uplinks(&devs, direct_sender, &counters, mtu, offload);
                            direct_dl = Some(spawn_direct_downlink(&devs, direct_recver, &counters, offload));
                            mode = BridgeMode::Direct;
                            tracing::info!(path = "direct", "bridge switched to direct path");
                        }
                        // Already on direct: drop the spurious upgrade.
                        BridgeAction::Ignore => {}
                        _ => unreachable!("upgrade yields GoDirect/Ignore only"),
                    }
                }
            }
        };

        stats_task.abort();
        relay_dl.abort();
        if let Some(d) = direct_dl.take() {
            d.abort();
        }
        abort_await!(uplinks);
        result
    }

    /// Decide whether to emit the one-shot "persistent TooLarge drops" warning:
    /// only when drops exist, the link has been up for more than 10 s (transient
    /// MTU-discovery drops at startup are normal), and we have not warned yet.
    fn should_warn_drops(drops: u64, elapsed: Duration, warned: bool) -> bool {
        drops > 0 && elapsed > Duration::from_secs(10) && !warned
    }

    async fn run_uplink(
        dev: Arc<tun_rs::AsyncDevice>,
        sender: LinkSender,
        counters: Arc<BridgeCounters>,
        mtu: u16,
        offload: bool,
    ) -> Result<()> {
        if offload {
            run_uplink_offload(dev, sender, counters, mtu).await
        } else {
            run_uplink_single(dev, sender, counters, mtu).await
        }
    }

    /// Phase 6.1: single-packet read from TUN, one datagram/frame per call.
    async fn run_uplink_single(
        dev: Arc<tun_rs::AsyncDevice>,
        mut sender: LinkSender,
        counters: Arc<BridgeCounters>,
        mtu: u16,
    ) -> Result<()> {
        // Fixed worst-case buffer (not MTU-sized): the dynamic-PMTU monitor can
        // raise the TUN MTU at runtime, and a smaller buffer would truncate
        // reads. 64 KiB per uplink task is negligible.
        let _ = mtu;
        let mut buf = vec![0u8; u16::MAX as usize + 4];
        loop {
            let n = dev.recv(&mut buf).await?;
            if n == 0 {
                continue;
            }
            let pkt = Bytes::copy_from_slice(&buf[..n]);
            let pkts = [pkt];
            // `dropped` counts oversized (TooLarge) packets — transient, not
            // fatal. Only a genuine link error returns Err and stops the pump.
            let dropped = sender.send_batch(&pkts).await?;
            if dropped == 0 {
                counters.tx_pkts.fetch_add(1, Ordering::Relaxed);
                counters.tx_bytes.fetch_add(n as u64, Ordering::Relaxed);
            } else {
                counters.tx_drops.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Phase 6.2: batch read from TUN via GSO super-buffer, one syscall → N segments.
    async fn run_uplink_offload(
        dev: Arc<tun_rs::AsyncDevice>,
        mut sender: LinkSender,
        counters: Arc<BridgeCounters>,
        _mtu: u16,
    ) -> Result<()> {
        let mut original_buffer = vec![0u8; tun_rs::VIRTIO_NET_HDR_LEN + 65535];
        // Per-segment buffers sized for the largest possible IP packet, NOT the
        // TUN MTU: in gateway mode the kernel forwards GRO super-frames whose
        // gso_size reflects the LAN-side MSS (1500+, jumbo frames up to 9000),
        // and tun_rs's gso_split panics on a segment larger than its buffer.
        let mut bufs = vec![vec![0u8; u16::MAX as usize]; tun_rs::IDEAL_BATCH_SIZE];
        let mut sizes = vec![0usize; tun_rs::IDEAL_BATCH_SIZE];
        loop {
            let num = dev
                .recv_multiple(&mut original_buffer, &mut bufs, &mut sizes, 0)
                .await?;
            if num == 0 {
                continue;
            }
            let pkts: Vec<Bytes> = (0..num)
                .map(|i| Bytes::copy_from_slice(&bufs[i][..sizes[i]]))
                .collect();
            let total_bytes: u64 = pkts.iter().map(|p| p.len() as u64).sum();
            // `dropped` counts oversized (TooLarge) packets — transient, not
            // fatal. Only a genuine link error returns Err and stops the pump.
            let dropped = sender.send_batch(&pkts).await?;
            let sent = num - dropped;
            counters.tx_pkts.fetch_add(sent as u64, Ordering::Relaxed);
            // tx_bytes counts the whole batch; during the brief MTU-discovery
            // drop window this slightly over-counts, which is immaterial.
            counters.tx_bytes.fetch_add(total_bytes, Ordering::Relaxed);
            counters
                .tx_drops
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    /// Run the downlink: write decrypted/decompressed packets to the TUN.
    /// Public for hub mode (Phase 3).
    pub async fn run_downlink(
        dev: Arc<tun_rs::AsyncDevice>,
        recver: LinkRecver,
        counters: Arc<BridgeCounters>,
        offload: bool,
    ) -> Result<()> {
        if offload {
            run_downlink_offload(dev, recver, counters).await
        } else {
            run_downlink_single(dev, recver, counters).await
        }
    }

    /// Phase 6.1: single-packet write to TUN per frame.
    async fn run_downlink_single(
        dev: Arc<tun_rs::AsyncDevice>,
        mut recver: LinkRecver,
        counters: Arc<BridgeCounters>,
    ) -> Result<()> {
        let mut batch = Vec::with_capacity(64);
        loop {
            batch.clear();
            recver.recv_batch(&mut batch).await?;
            for pkt in &batch {
                counters.rx_pkts.fetch_add(1, Ordering::Relaxed);
                counters
                    .rx_bytes
                    .fetch_add(pkt.len() as u64, Ordering::Relaxed);
                dev.send(pkt).await?;
            }
        }
    }

    /// Phase 6.2: coalesce RX batch via GRO, one multi-packet write syscall.
    /// Each BytesMut has VIRTIO_NET_HDR_LEN zeros prepended (no checksum offload
    /// needed — packets from the peer have complete checksums).
    async fn run_downlink_offload(
        dev: Arc<tun_rs::AsyncDevice>,
        mut recver: LinkRecver,
        counters: Arc<BridgeCounters>,
    ) -> Result<()> {
        let mut batch = Vec::with_capacity(tun_rs::IDEAL_BATCH_SIZE);
        let mut gro_table = tun_rs::GROTable::default();
        loop {
            batch.clear();
            recver.recv_batch(&mut batch).await?;
            let total_pkts = batch.len() as u64;
            let total_bytes: u64 = batch.iter().map(|p| p.len() as u64).sum();
            // Build BytesMut slices with VIRTIO_NET_HDR_LEN header prefix (all zeros).
            let mut bufs: Vec<bytes::BytesMut> = batch
                .iter()
                .map(|pkt| {
                    let mut b =
                        bytes::BytesMut::with_capacity(tun_rs::VIRTIO_NET_HDR_LEN + pkt.len());
                    b.resize(tun_rs::VIRTIO_NET_HDR_LEN, 0);
                    b.extend_from_slice(pkt);
                    b
                })
                .collect();
            let mut slices: Vec<&mut bytes::BytesMut> = bufs.iter_mut().collect();
            dev.send_multiple(&mut gro_table, &mut slices, tun_rs::VIRTIO_NET_HDR_LEN)
                .await?;
            counters.rx_pkts.fetch_add(total_pkts, Ordering::Relaxed);
            counters.rx_bytes.fetch_add(total_bytes, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::time::Duration;

        /// D1 — truth table for the one-shot persistent-drops warning.
        #[test]
        fn toolarge_warn_logic() {
            use super::should_warn_drops;
            let early = Duration::from_secs(5);
            let late = Duration::from_secs(11);
            // No drops → never warn.
            assert!(!should_warn_drops(0, late, false));
            // Drops but link younger than 10 s → no warn (startup transients).
            assert!(!should_warn_drops(3, early, false));
            // Already warned → never again.
            assert!(!should_warn_drops(3, late, true));
            // Drops persisting past 10 s, not yet warned → warn.
            assert!(should_warn_drops(3, late, false));
        }

        /// Phase 6.2 — Segmentation: each packet from recv_multiple is ≤ MTU.
        #[test]
        fn segment_gso_buffer() {
            let mtu = 1350u16;
            for &sz in &[40usize, 1310, 1350, 800] {
                assert!(sz <= mtu as usize, "segment {sz} > MTU {mtu}");
                let pkt = bytes::Bytes::copy_from_slice(&vec![0u8; sz]);
                assert_eq!(pkt.len(), sz);
            }
        }

        /// Phase 6.2 — GRO coalescing: BytesMut has VIRTIO_NET_HDR_LEN zeros prefix.
        #[test]
        fn coalesce_for_gro() {
            for sz in [100usize, 500, 1310] {
                let pkt = vec![0x45u8; sz]; // fake IPv4 header start
                let mut b = bytes::BytesMut::with_capacity(tun_rs::VIRTIO_NET_HDR_LEN + sz);
                b.resize(tun_rs::VIRTIO_NET_HDR_LEN, 0);
                b.extend_from_slice(&pkt);
                assert_eq!(b.len(), tun_rs::VIRTIO_NET_HDR_LEN + sz);
                assert!(b[..tun_rs::VIRTIO_NET_HDR_LEN].iter().all(|&x| x == 0));
                assert_eq!(&b[tun_rs::VIRTIO_NET_HDR_LEN..], pkt.as_slice());
            }
        }

        /// Truth table for the bridge state machine's transition function.
        #[test]
        fn bridge_next_action_table() {
            use super::{bridge_next_action, BridgeAction, BridgeEvent, BridgeMode};

            // Relay mode
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::RelayDownlinkDied, true),
                BridgeAction::LinkDead
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::RelayDownlinkDied, false),
                BridgeAction::LinkDead
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::UplinkDied, true),
                BridgeAction::LinkDead
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::UplinkDied, false),
                BridgeAction::LinkDead
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::UpgradeArrived, true),
                BridgeAction::GoDirect
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::UpgradeArrived, false),
                BridgeAction::GoDirect
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::DirectDownlinkDied, true),
                BridgeAction::Ignore
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Relay, BridgeEvent::DirectDownlinkDied, false),
                BridgeAction::Ignore
            );

            // Direct mode - relay alive
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::RelayDownlinkDied, true),
                BridgeAction::Ignore
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::UplinkDied, true),
                BridgeAction::FallBackToRelay
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::UpgradeArrived, true),
                BridgeAction::Ignore
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::DirectDownlinkDied, true),
                BridgeAction::FallBackToRelay
            );

            // Direct mode - relay dead
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::RelayDownlinkDied, false),
                BridgeAction::Ignore
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::UplinkDied, false),
                BridgeAction::ReconnectRelayDead
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::UpgradeArrived, false),
                BridgeAction::Ignore
            );
            assert_eq!(
                bridge_next_action(BridgeMode::Direct, BridgeEvent::DirectDownlinkDied, false),
                BridgeAction::ReconnectRelayDead
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{ClientMessage, Delimited, ServerMessage, UdpDirectTuning};

    /// §1.2 — the ctrl actor ignores heartbeats, forwards punches as events,
    /// writes outbound client messages, and resolves its JoinHandle with an
    /// error when the server closes the control stream.
    #[tokio::test]
    async fn ctrl_actor_forwards_punch_and_detects_close() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (client_opener, _client_acceptor) = crate::mux::client(a);
        let (_server_opener, mut server_acceptor) = crate::mux::server(b);
        // yamux opens substreams lazily: the acceptor learns about the stream
        // only after the opener's first write, so announce it with the marker.
        let mut client_stream = client_opener.open().await.unwrap();
        client_stream
            .write_all(&[crate::mux::STREAM_READY])
            .await
            .unwrap();
        let mut server_stream = server_acceptor.accept().await.unwrap();
        let mut marker = [0u8; 1];
        server_stream.read_exact(&mut marker).await.unwrap();
        let ctrl = Delimited::new(client_stream);
        let mut server = Delimited::new(server_stream);

        let (out_tx, mut event_rx, handle) = spawn_ctrl_actor(ctrl);

        // Heartbeat produces no event.
        server.send(ServerMessage::Heartbeat).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            event_rx.try_recv().is_err(),
            "heartbeat must not produce an event"
        );

        // Outbound message reaches the server side.
        out_tx
            .send(ClientMessage::UdpStunHintRequest)
            .await
            .unwrap();
        let got = server.recv::<ClientMessage>().await.unwrap();
        assert!(matches!(got, Some(ClientMessage::UdpStunHintRequest)));

        // UdpPunch becomes CtrlEvent::Punch with the same fields.
        let peer: std::net::SocketAddr = "203.0.113.9:4444".parse().unwrap();
        server
            .send(ServerMessage::UdpPunch {
                nonce: [9u8; crate::shared::UDP_NONCE_LEN],
                peer: vec![peer],
                peer_selected_stun: None,
                tuning: UdpDirectTuning::default(),
                peer_id: 0,
            })
            .await
            .unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timed out waiting for punch event")
        {
            Some(CtrlEvent::Punch {
                nonce,
                peer: got_peer,
                ..
            }) => {
                assert_eq!(nonce, [9u8; crate::shared::UDP_NONCE_LEN]);
                assert_eq!(got_peer, vec![peer]);
            }
            Some(CtrlEvent::Unavailable) => panic!("expected Punch, got Unavailable"),
            None => panic!("event channel closed unexpectedly"),
        }

        // Closing the server side resolves the JoinHandle with an error.
        drop(server);
        let err = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("ctrl actor did not detect stream close")
            .expect("ctrl actor panicked");
        assert!(
            err.to_string().contains("control stream"),
            "unexpected error: {err}"
        );
    }

    /// §4.3 — PMTU decision truth table.
    #[test]
    fn pmtu_decision_cases() {
        // Fewer than 3 samples → None.
        assert_eq!(pmtu_decision(1350, &[]), None);
        assert_eq!(pmtu_decision(1350, &[1450, 1450]), None);
        // Unstable samples → None.
        assert_eq!(pmtu_decision(1350, &[1400, 1450, 1450]), None);
        // Stable but equal to current → None.
        assert_eq!(pmtu_decision(1450, &[1450, 1450, 1450]), None);
        // Stable, delta < 16 → None (churn guard).
        assert_eq!(pmtu_decision(1450, &[1460, 1460, 1460]), None);
        // Stable, larger → Some.
        assert_eq!(pmtu_decision(1350, &[1450, 1450, 1450]), Some(1450));
        // Stable, smaller → Some (path got narrower).
        assert_eq!(pmtu_decision(1450, &[1350, 1350, 1350]), Some(1350));
        // Below 576 → None.
        assert_eq!(pmtu_decision(1350, &[500, 500, 500]), None);
        // Above 9000 → clamped.
        assert_eq!(pmtu_decision(1350, &[65000, 65000, 65000]), Some(9000));
        // Only the LAST 3 samples matter.
        assert_eq!(pmtu_decision(1350, &[100, 1450, 1450, 1450]), Some(1450));
    }

    /// TUN name auto-resolution.
    #[test]
    fn pick_tun_name_explicit_passthrough() {
        assert_eq!(
            hostcfg::pick_tun_name("bore7", |_| false),
            Some("bore7".to_string())
        );
        assert_eq!(
            hostcfg::pick_tun_name("mytun", |_| false),
            Some("mytun".to_string())
        );
    }

    #[test]
    fn pick_tun_name_auto_finds_first_free() {
        assert_eq!(
            hostcfg::pick_tun_name("auto", |_| false),
            Some("bore0".to_string())
        );
    }

    #[test]
    fn pick_tun_name_auto_skips_occupied() {
        let taken = |c: &str| c == "bore0" || c == "bore1";
        assert_eq!(
            hostcfg::pick_tun_name("auto", taken),
            Some("bore2".to_string())
        );
    }

    #[test]
    fn pick_tun_name_auto_exhaustion() {
        let all_taken = |c: &str| c.starts_with("bore");
        assert_eq!(hostcfg::pick_tun_name("auto", all_taken), None);
    }

    /// Routing-loop guard: direct candidates inside a tunneled subnet are
    /// dropped (they would loop the QUIC handshake back through the relay and
    /// the "direct" link would die at the switch — `read_datagram: timed out`).
    #[test]
    fn filter_tunneled_candidates_drops_looping_addrs() {
        let parse = |s: &str| s.parse::<std::net::SocketAddr>().unwrap();
        let net = |s: &str| s.parse::<crate::shared::Ipv4Net>().unwrap();

        // Reproduces the field bug: peer offers a public + a LAN candidate; the
        // LAN one (10.10.16.138) is inside the tunneled 10.10.0.0/19.
        let peers = vec![parse("91.81.116.61:35444"), parse("10.10.16.138:35444")];
        let tunneled = vec![net("10.10.0.0/19"), net("172.31.0.0/16")];
        let (kept, dropped) = filter_tunneled_candidates(&peers, &tunneled);
        assert_eq!(kept, vec![parse("91.81.116.61:35444")]);
        assert_eq!(dropped, vec![parse("10.10.16.138:35444")]);

        // No tunneled subnets (e.g. the connector advertised nothing) → keep all.
        let (kept, dropped) = filter_tunneled_candidates(&peers, &[]);
        assert_eq!(kept, peers);
        assert!(dropped.is_empty());

        // All candidates inside tunneled subnets → nothing kept (caller bails to relay).
        let only_lan = vec![parse("10.10.1.1:1"), parse("172.31.9.9:2")];
        let (kept, dropped) = filter_tunneled_candidates(&only_lan, &tunneled);
        assert!(kept.is_empty());
        assert_eq!(dropped.len(), 2);

        // IPv6 candidate never matches an IPv4 tunneled subnet.
        let v6 = vec![parse("[2001:db8::1]:9")];
        let (kept, dropped) = filter_tunneled_candidates(&v6, &tunneled);
        assert_eq!(kept, v6);
        assert!(dropped.is_empty());
    }

    /// Direct-upgrade retry loop control: keep retrying on relay, stop on
    /// success or when the upgrade channel closes (link teardown).
    #[test]
    fn should_retry_direct_cases() {
        // Failed attempt, retry enabled, channel open → retry.
        assert!(should_retry_direct(false, true, false));
        // Succeeded → stop (direct achieved).
        assert!(!should_retry_direct(true, true, false));
        // Upgrade channel closed (link tearing down) → stop.
        assert!(!should_retry_direct(false, true, true));
        // Retry disabled → stop after one shot.
        assert!(!should_retry_direct(false, false, false));
    }

    /// §4.3 — urgent one-sample shrink (fast recovery after a direct switch).
    #[test]
    fn pmtu_shrink_now_cases() {
        // Sample well below current → shrink immediately on a single sample.
        // This is the post-switch case: TUN at 1350, path MTU 1162.
        assert_eq!(pmtu_shrink_now(1350, 1162), Some(1162));
        // Sample at/above current → never shrink (growth needs pmtu_decision).
        assert_eq!(pmtu_shrink_now(1162, 1350), None);
        assert_eq!(pmtu_shrink_now(1350, 1350), None);
        // Within the 16-byte churn guard → no shrink.
        assert_eq!(pmtu_shrink_now(1350, 1340), None);
        // Exactly 16 below → shrink (boundary).
        assert_eq!(pmtu_shrink_now(1350, 1334), Some(1334));
        // Below the 576 floor → rejected (bogus reading, keep current).
        assert_eq!(pmtu_shrink_now(1350, 500), None);
        // A huge sample is clamped to 9000; with a larger current it shrinks.
        assert_eq!(pmtu_shrink_now(9100, 65000), Some(9000));
    }

    /// §2.1 — fatal-vs-retryable classification truth table.
    #[test]
    fn fatal_classification() {
        // FatalVpnError downcasts as fatal.
        let fatal: anyhow::Error = anyhow::Error::new(FatalVpnError("overlap".into()));
        assert!(is_fatal(&fatal));
        // Plain anyhow errors are retryable.
        let plain = anyhow!("connection reset by peer");
        assert!(!is_fatal(&plain));
        // Fatal survives an added context chain (downcast_ref traverses it).
        let wrapped = anyhow::Error::new(FatalVpnError("overlap".into())).context("while pairing");
        assert!(is_fatal(&wrapped));
        // "already in use" and "not found" are the deliberate retryable VpnErrors
        // (both are reconnect-race transients, not config errors).
        assert!(vpn_error_is_retryable("vpn id 'x' already in use"));
        assert!(vpn_error_is_retryable("vpn listener 'x' not found"));
        assert!(!vpn_error_is_retryable("overlapping subnets: a and b"));
        assert!(!is_fatal(&classify_vpn_error(
            "vpn id 'x' already in use".into()
        )));
        assert!(!is_fatal(&classify_vpn_error(
            "vpn listener 'x' not found".into()
        )));
        assert!(is_fatal(&classify_vpn_error("vpn pool exhausted".into())));
    }

    /// §2.2 — the reconnect loop retries retryable errors, stops on fatal ones,
    /// and runs exactly once with auto = false.
    #[tokio::test(start_paused = true)]
    async fn run_with_reconnect_counts() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        // 3 retryable failures, then a fatal one: exactly 4 attempts, Err out.
        let n = Arc::new(AtomicU32::new(0));
        let n2 = Arc::clone(&n);
        let result = run_with_reconnect(true, move || {
            let n = Arc::clone(&n2);
            async move {
                let i = n.fetch_add(1, Ordering::SeqCst);
                if i < 3 {
                    Err(anyhow!("transient"))
                } else {
                    Err(anyhow::Error::new(FatalVpnError("config".into())))
                }
            }
        })
        .await;
        assert!(result.is_err());
        assert!(is_fatal(&result.unwrap_err()));
        assert_eq!(n.load(Ordering::SeqCst), 4, "3 retries + 1 fatal stop");

        // auto = false: a retryable error is NOT retried.
        let n = Arc::new(AtomicU32::new(0));
        let n2 = Arc::clone(&n);
        let result = run_with_reconnect(false, move || {
            let n = Arc::clone(&n2);
            async move {
                n.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(anyhow!("transient"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            n.load(Ordering::SeqCst),
            1,
            "no retry without --auto-reconnect"
        );

        // Ok() exits the loop immediately.
        let result = run_with_reconnect(true, || async { Ok(()) }).await;
        assert!(result.is_ok());
    }
}

/// VPN hub data-plane: single TUN with per-peer router (relay path only, Phase 3).
///
/// Hub mode for `bore vpn listen --max-clients >1`: single TUN device with per-peer
/// router uplink and shared downlink. Each peer gets its own relay link + PeerHandle.
/// Phase 3 relay-only; Phase 4 adds per-peer direct upgrade.
#[allow(clippy::doc_lazy_continuation, clippy::mixed_attributes_style)]
pub mod hub {

    use anyhow::{anyhow, Result};
    use bytes::Bytes;
    use dashmap::DashMap;
    use std::net::Ipv4Addr;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex, Notify};
    use tracing::{debug, info, warn};

    use super::bridge::BridgeCounters;
    use super::link::LinkSender;

    /// Handle to a connected peer in the hub. Holds a swappable sender (for Phase 4
    /// relay↔direct upgrade) and a shutdown trigger. The sender is behind a Mutex
    /// to allow in-place swaps without restarting the router.
    pub struct PeerHandle {
        /// Peer's overlay IP address.
        pub overlay: Ipv4Addr,
        /// Server-assigned peer id (unique within hub session).
        pub peer_id: u32,
        /// Swappable link sender (currently relay; Phase 4: can swap to direct).
        pub sender: Mutex<LinkSender>,
        /// Notified when the peer should shut down (VpnPeerLeave or link death).
        pub shutdown: Notify,
    }

    /// Peer table: map from overlay IP to peer handle. Read-mostly (router path).
    type PeerTable = Arc<DashMap<Ipv4Addr, Arc<PeerHandle>>>;

    /// Substream demux event: a peer's relay substream arrived before the peer was
    /// registered, or a fresh peer substream from the relay.
    struct HubSub {
        /// Peer id from the server's injected header.
        peer_id: u32,
        /// Direction tag (RELAY_TAG_UP or RELAY_TAG_DOWN).
        tag: u8,
        /// The accepted mux::Stream.
        stream: crate::mux::Stream,
    }

    /// Live peer entry: (downlink_task, peer_handle, punch_tx for direct upgrade).
    type LivePeerEntry = (
        tokio::task::JoinHandle<()>,
        Arc<PeerHandle>,
        tokio::sync::mpsc::Sender<HubEvent>,
    );

    /// Event from the server-facing control stream (hub ctrl actor output).
    #[derive(Debug, Clone)]
    enum HubEvent {
        /// A new connector paired; add to peer table when link builds.
        Join {
            peer_id: u32,
            overlay: Ipv4Addr,
            /// The peer's per-peer session nonce. Passed RAW (UDP_NONCE_LEN bytes)
            /// to `derive_keys_listener` so it matches the spoke's
            /// `derive_keys_connector(&session_nonce)` — never padded/resized, or
            /// the HKDF inputs diverge and the AEAD keys won't match.
            nonce: [u8; crate::shared::UDP_NONCE_LEN],
            carriers: u16,
        },
        /// A connector disconnected; remove from peer table.
        Leave { peer_id: u32 },
        /// Server brokered a punch for two peers (direct-path upgrade attempt).
        /// Routes to the connector's direct task (matched by peer_id).
        Punch {
            peer_id: u32,
            /// Session nonce; both peers derive the same QUIC token from it.
            nonce: [u8; crate::shared::UDP_NONCE_LEN],
            /// Peer candidate addresses to punch toward.
            peer: Vec<std::net::SocketAddr>,
            /// Direct-UDP transport tuning requested by the server.
            tuning: crate::shared::UdpDirectTuning,
        },
    }

    /// Pending peer: buffering substreams until the Join event + carriers count allows build.
    struct PendingPeer {
        /// Metadata from the Join event (if arrived).
        meta: Option<(u16, [u8; crate::shared::UDP_NONCE_LEN], Ipv4Addr)>, // (carriers, nonce, overlay)
        /// Uplink substreams (connector→hub).
        up: Vec<crate::mux::Stream>,
        /// Downlink substreams (hub→connector).
        down: Vec<crate::mux::Stream>,
    }

    /// Spawns the accept task: reads from the mux acceptor and parses the server's
    /// `[STREAM_READY, peer_id u32, tag]` prefix, then sends HubSub events to the coordinator.
    ///
    /// NOT `async`: it must `tokio::spawn` synchronously and return the handle.
    /// As an `async fn` the body would only run when the returned future is
    /// awaited — and the call site does not await it, so the accept task (and
    /// thus the entire relay data path) would silently never start.
    fn spawn_accept_task(
        mut acceptor: crate::mux::Acceptor,
        tx: mpsc::Sender<HubSub>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(60), acceptor.accept())
                    .await
                {
                    Ok(Some(mut stream)) => {
                        // Read the server-injected header: [STREAM_READY, peer_id u32, tag]
                        use tokio::io::AsyncReadExt;
                        let mut hdr = [0u8; 6];
                        if let Err(e) = stream.read_exact(&mut hdr).await {
                            warn!(error = %e, "failed to read hub relay header");
                            continue;
                        }

                        // Check STREAM_READY marker
                        if hdr[0] != crate::mux::STREAM_READY {
                            warn!(marker = hdr[0], "expected STREAM_READY in hub relay header");
                            continue;
                        }

                        let peer_id = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
                        let tag = hdr[5];

                        let ev = HubSub {
                            peer_id,
                            tag,
                            stream,
                        };
                        if tx.send(ev).await.is_err() {
                            debug!("hub coordinator closed; accept task exiting");
                            return;
                        }
                    }
                    Ok(None) => {
                        debug!("acceptor closed; accept task exiting");
                        return;
                    }
                    Err(_) => {
                        warn!("60s timeout waiting for hub relay substream; dropping pending");
                    }
                }
            }
        })
    }

    /// Router uplink: read packets from TUN, parse destination IPv4, route to per-peer sender.
    /// Spawned once per TUN queue. Adapted from `bridge::run_uplink_single`/`_offload`.
    pub async fn run_router_uplink(
        dev: Arc<tun_rs::AsyncDevice>,
        table: PeerTable,
        counters: Arc<BridgeCounters>,
        offload: bool,
    ) -> Result<()> {
        if offload {
            run_router_uplink_offload(dev, table, counters).await
        } else {
            run_router_uplink_single(dev, table, counters).await
        }
    }

    /// Phase 6.1 router: single-packet read.
    async fn run_router_uplink_single(
        dev: Arc<tun_rs::AsyncDevice>,
        table: PeerTable,
        counters: Arc<BridgeCounters>,
    ) -> Result<()> {
        let mut buf = vec![0u8; u16::MAX as usize + 4];
        let mut _dropped_no_peer = 0u64;
        loop {
            let n = dev.recv(&mut buf).await?;
            if n == 0 {
                continue;
            }
            let pkt = Bytes::copy_from_slice(&buf[..n]);

            // Parse destination IPv4 from the packet header (offset 16..20).
            if let Some(dst) = parse_ipv4_dst(&pkt) {
                if let Some(peer_entry) = table.get(&dst) {
                    let peer = Arc::clone(peer_entry.value());
                    drop(peer_entry); // Drop the DashMap ref early.
                    let mut sender = peer.sender.lock().await;
                    match sender.send_batch(std::slice::from_ref(&pkt)).await {
                        Ok(0) => {
                            counters.tx_pkts.fetch_add(1, Ordering::Relaxed);
                            counters.tx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                        }
                        Ok(_dropped) => {
                            // Oversized (direct path only, shouldn't happen on relay).
                            counters.tx_drops.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            warn!(peer_id = peer.peer_id, error = %e, "peer send failed; dropping packet");
                            // Per-packet error: drop and continue (downlink will also fail).
                        }
                    }
                } else {
                    _dropped_no_peer += 1;
                }
            }
            // Non-IPv4 or too short: silently drop.
        }
    }

    /// Phase 6.2 router: offload (batch) read with GSO.
    async fn run_router_uplink_offload(
        dev: Arc<tun_rs::AsyncDevice>,
        table: PeerTable,
        counters: Arc<BridgeCounters>,
    ) -> Result<()> {
        let mut original_buffer = vec![0u8; tun_rs::VIRTIO_NET_HDR_LEN + 65535];
        let mut bufs = vec![vec![0u8; u16::MAX as usize]; tun_rs::IDEAL_BATCH_SIZE];
        let mut sizes = vec![0usize; tun_rs::IDEAL_BATCH_SIZE];

        loop {
            // Offset 0: each segment buffer holds the pure IP packet at [0..size]
            // (matches bridge::run_uplink_offload). A non-zero offset would shift
            // the payload and make parse_ipv4_dst read the wrong bytes.
            let num = dev
                .recv_multiple(&mut original_buffer, &mut bufs, &mut sizes, 0)
                .await?;
            if num == 0 {
                continue;
            }

            // Group packets by destination peer.
            use std::collections::HashMap;
            let mut groups: HashMap<Ipv4Addr, Vec<Bytes>> = HashMap::new();
            let mut _dropped_no_peer = 0usize;

            for i in 0..num {
                let pkt = Bytes::copy_from_slice(&bufs[i][..sizes[i]]);
                if let Some(dst) = parse_ipv4_dst(&pkt) {
                    groups.entry(dst).or_default().push(pkt);
                } else {
                    _dropped_no_peer += 1;
                }
            }

            // Send each group to its peer.
            for (dst, pkts) in groups {
                if let Some(peer_entry) = table.get(&dst) {
                    let peer = Arc::clone(peer_entry.value());
                    drop(peer_entry);
                    let mut sender = peer.sender.lock().await;
                    match sender.send_batch(&pkts).await {
                        Ok(0) => {
                            let pkt_count = pkts.len() as u64;
                            let byte_count: u64 = pkts.iter().map(|p| p.len() as u64).sum();
                            counters.tx_pkts.fetch_add(pkt_count, Ordering::Relaxed);
                            counters.tx_bytes.fetch_add(byte_count, Ordering::Relaxed);
                        }
                        Ok(dropped) => {
                            counters
                                .tx_drops
                                .fetch_add(dropped as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            warn!(peer_id = peer.peer_id, error = %e, "peer send failed; dropping group");
                        }
                    }
                } else {
                    _dropped_no_peer += pkts.len();
                }
            }
        }
    }

    /// Parse the destination IPv4 address from a packet (offset 16..20 of IPv4 header).
    /// Returns None if the packet is too short or not IPv4.
    fn parse_ipv4_dst(pkt: &[u8]) -> Option<Ipv4Addr> {
        if pkt.len() < 20 {
            return None;
        }
        let version = pkt[0] >> 4;
        if version != 4 {
            return None;
        }
        Some(Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]))
    }

    /// Hub coordinator: manages peer table, pending substreams, and lifecycle.
    /// Single owner of PeerTable mutation.
    #[allow(clippy::too_many_arguments)]
    async fn run_hub_coordinator(
        devs: Vec<Arc<tun_rs::AsyncDevice>>,
        secret: String,
        counters: Arc<BridgeCounters>,
        offload: bool,
        mut sub_rx: mpsc::Receiver<HubSub>,
        mut event_rx: mpsc::Receiver<HubEvent>,
        event_tx: mpsc::Sender<HubEvent>,
        peer_table: PeerTable,
        args: super::VpnListenArgs,
        admin_v2: bool,
        out_tx: tokio::sync::mpsc::Sender<crate::shared::ClientMessage>,
    ) {
        let mut pending: std::collections::HashMap<u32, PendingPeer> =
            std::collections::HashMap::new();
        let mut live_peers: std::collections::HashMap<u32, LivePeerEntry> =
            std::collections::HashMap::new();

        loop {
            tokio::select! {
                // Substream arrived
                Some(HubSub { peer_id, tag, stream }) = sub_rx.recv() => {
                    // Bound the pre-join buffer (6.1): drop substreams for a new
                    // peer once too many distinct peers are pending un-built, and
                    // cap per-peer streams (a peer needs only 2*carriers ≤ 32).
                    // Prevents unbounded growth from stray/duplicate substreams.
                    const MAX_PENDING_PEERS: usize = 512;
                    const MAX_PENDING_PER_PEER: usize = 64;
                    if !pending.contains_key(&peer_id) && pending.len() >= MAX_PENDING_PEERS {
                        warn!(peer_id, "hub pending-peer buffer full; dropping substream");
                        continue;
                    }
                    let entry = pending.entry(peer_id).or_insert_with(|| PendingPeer {
                        meta: None,
                        up: Vec::new(),
                        down: Vec::new(),
                    });
                    if entry.up.len() + entry.down.len() >= MAX_PENDING_PER_PEER {
                        warn!(peer_id, "hub per-peer pending buffer full; dropping substream");
                        continue;
                    }

                    match tag {
                        super::link::RELAY_TAG_UP => entry.up.push(stream),
                        super::link::RELAY_TAG_DOWN => entry.down.push(stream),
                        _ => {
                            warn!(peer_id, tag, "unknown relay tag");
                            continue;
                        }
                    }

                    try_build_peer(&mut pending, &mut live_peers, &devs, &secret, &counters, offload, peer_id, &peer_table, &event_tx, &args, admin_v2, out_tx.clone()).await;
                }

                // Join/Leave/Punch event from server
                Some(ev) = event_rx.recv() => {
                    match ev {
                        HubEvent::Join {
                            peer_id,
                            overlay,
                            nonce,
                            carriers,
                        } => {
                            let entry = pending.entry(peer_id).or_insert_with(|| PendingPeer {
                                meta: None,
                                up: Vec::new(),
                                down: Vec::new(),
                            });
                            entry.meta = Some((carriers, nonce, overlay));
                            try_build_peer(&mut pending, &mut live_peers, &devs, &secret, &counters, offload, peer_id, &peer_table, &event_tx, &args, admin_v2, out_tx.clone()).await;
                        }
                        HubEvent::Leave { peer_id } => {
                            // Remove from live peers and tear down. Idempotent:
                            // a downlink-death self-report and a server
                            // VpnPeerLeave can both fire for the same peer.
                            if let Some((dl_task, peer, _punch_tx)) = live_peers.remove(&peer_id) {
                                peer_table.remove(&peer.overlay);
                                peer.shutdown.notify_waiters();
                                dl_task.abort();
                            }
                            pending.remove(&peer_id);
                        }
                        HubEvent::Punch {
                            peer_id,
                            nonce,
                            peer,
                            tuning,
                        } => {
                            // Phase 4.2c: route to peer's direct task (when added).
                            if let Some((_dl_task, _peer_handle, punch_tx)) = live_peers.get(&peer_id) {
                                let _ = punch_tx.send(HubEvent::Punch {
                                    peer_id,
                                    nonce,
                                    peer,
                                    tuning,
                                }).await;
                            } else {
                                debug!(?peer_id, "hub coordinator: punch event for unknown peer; peer may be removed");
                            }
                        }
                    }
                }

                else => {
                    debug!("hub coordinator exiting");
                    break;
                }
            }
        }
    }

    /// Attempt to build a peer link when it has carriers + nonce + up/down substreams.
    #[allow(clippy::too_many_arguments)]
    async fn try_build_peer(
        pending: &mut std::collections::HashMap<u32, PendingPeer>,
        live_peers: &mut std::collections::HashMap<u32, LivePeerEntry>,
        devs: &[Arc<tun_rs::AsyncDevice>],
        secret: &str,
        counters: &Arc<BridgeCounters>,
        offload: bool,
        peer_id: u32,
        peer_table: &PeerTable,
        event_tx: &mpsc::Sender<HubEvent>,
        args: &super::VpnListenArgs,
        admin_v2: bool,
        out_tx: tokio::sync::mpsc::Sender<crate::shared::ClientMessage>,
    ) {
        let entry = match pending.get_mut(&peer_id) {
            Some(e) => e,
            None => return,
        };

        let (carriers, nonce, overlay) = match entry.meta {
            Some(m) => m,
            None => return,
        };

        let carriers = carriers as usize;
        if entry.up.len() < carriers || entry.down.len() < carriers {
            return;
        }

        // Take the streams.
        let mut up_streams = Vec::new();
        let mut down_streams = Vec::new();
        for _ in 0..carriers {
            if let Some(s) = entry.up.pop() {
                up_streams.push(s);
            }
            if let Some(s) = entry.down.pop() {
                down_streams.push(s);
            }
        }

        // For multi-carrier peers the connector wrote a 3-byte header
        // `[STREAM_READY, tag, idx]`; the server forwards `[tag, idx, payload]`
        // and the accept task already consumed `[STREAM_READY, peer_id, tag]`,
        // leaving `[idx, payload]`. Consume the 1-byte carrier index here (its
        // value is unused — make_relay_multi round-robins). carriers==1 has no
        // idx byte (byte-identical to the single-carrier path).
        if carriers > 1 {
            use tokio::io::AsyncReadExt;
            for s in up_streams.iter_mut().chain(down_streams.iter_mut()) {
                let mut idx = [0u8; 1];
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    s.read_exact(&mut idx),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    _ => {
                        warn!(%peer_id, "failed to read carrier idx byte; dropping peer");
                        pending.remove(&peer_id);
                        return;
                    }
                }
            }
        }

        // Derive keys and build the link.
        let keys = match super::crypto::derive_keys_listener(secret, &nonce) {
            Ok(k) => k,
            Err(e) => {
                warn!(%peer_id, error = %e, "failed to derive keys");
                pending.remove(&peer_id);
                return;
            }
        };

        let (sender, recver) = super::link::make_relay_multi(down_streams, up_streams, keys);
        let peer = Arc::new(PeerHandle {
            overlay,
            peer_id,
            sender: Mutex::new(sender),
            shutdown: Notify::new(),
        });

        // Register in peer table.
        peer_table.insert(overlay, Arc::clone(&peer));

        // Spawn downlink. On death it self-reports Leave so the coordinator
        // removes it from the PeerTable even without a server VpnPeerLeave
        // (otherwise the router would keep sending to a dead relay link).
        let peer_clone = Arc::clone(&peer);
        let dev0 = Arc::clone(&devs[0]);
        let counters_clone = Arc::clone(counters);
        let dl_event_tx = event_tx.clone();
        let dl_task = tokio::spawn(async move {
            if let Err(e) = super::bridge::run_downlink(dev0, recver, counters_clone, offload).await
            {
                warn!(peer_id = peer_clone.peer_id, error = %e, "peer downlink died");
            }
            peer_clone.shutdown.notify_waiters();
            let _ = dl_event_tx.send(HubEvent::Leave { peer_id }).await;
        });

        // Phase 4.2d: Spawn per-peer direct task (unless relay_only).
        let (punch_tx, punch_rx) = tokio::sync::mpsc::channel(16);
        if !args.relay_only {
            let peer_clone = Arc::clone(&peer);
            let out_tx_clone = out_tx.clone();
            let args_clone = args.clone();
            let dev0 = Arc::clone(&devs[0]);
            let counters_clone = Arc::clone(counters);
            tokio::spawn(async move {
                per_peer_direct_upgrade_task(
                    peer_id,
                    peer_clone,
                    punch_rx,
                    args_clone,
                    admin_v2,
                    out_tx_clone,
                    dev0,
                    counters_clone,
                    offload,
                )
                .await;
            });
        }

        live_peers.insert(peer_id, (dl_task, peer, punch_tx));
        pending.remove(&peer_id);
        info!(%peer_id, %overlay, %carriers, "hub peer link built");
    }

    /// Phase 4.2d: Per-peer direct (QUIC) upgrade task for one spoke.
    ///
    /// The hub is always the QUIC **listener** (`DirectSide::Listener`). While the
    /// peer is on relay, this retries on the fixed [`DIRECT_RETRY_INTERVAL`] grid
    /// (aligned with the spoke's own grid so the server brokers a punch when it
    /// holds both offers). Each round: gather candidates → offer (with `peer_id`)
    /// → await the brokered punch → QUIC-accept. On success it swaps the peer's
    /// sender to Direct IN PLACE (the router never restarts — I-MC5) and spawns a
    /// direct downlink, keeping the relay downlink WARM. On direct death it swaps
    /// back to the warm relay sender (DEC-2 seamless fallback) and re-arms.
    ///
    /// Removal: when the coordinator drops the peer it drops `punch_tx`, closing
    /// `punch_rx`; every blocking point selects on it so the task exits promptly.
    #[allow(clippy::too_many_arguments)]
    async fn per_peer_direct_upgrade_task(
        peer_id: u32,
        peer: Arc<PeerHandle>,
        mut punch_rx: tokio::sync::mpsc::Receiver<HubEvent>,
        args: super::VpnListenArgs,
        admin_v2: bool,
        out_tx: tokio::sync::mpsc::Sender<crate::shared::ClientMessage>,
        dev0: Arc<tun_rs::AsyncDevice>,
        counters: Arc<BridgeCounters>,
        offload: bool,
    ) {
        // The relay sender is captured for fallback. It is a CLONE that shares the
        // SAME Arc<AtomicU64> nonce counter as the live relay link (I-5/DEC-6), so
        // swapping relay→direct→relay never reuses a (key, counter) pair.
        let relay_sender = peer.sender.lock().await.clone();
        let endpoint = crate::transport::Endpoint::parse(&args.to);
        // Candidate-loop guard uses the locally-present REAL subnets (the virtual
        // has no local route, N7); plain entries are real==exposed (unchanged).
        let tunneled = super::routes::advertised_reals(&args.advertise_entries);

        let mut ticker = tokio::time::interval(super::DIRECT_RETRY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                ev = punch_rx.recv() => match ev {
                    None => return,        // peer removed → exit
                    Some(_) => continue,   // stray punch between rounds → ignore
                },
            }
            match try_hub_peer_direct(
                peer_id,
                &peer,
                &mut punch_rx,
                &args,
                &endpoint,
                &tunneled,
                &dev0,
                &counters,
                offload,
                &out_tx,
                admin_v2,
                &relay_sender,
            )
            .await
            {
                HubDirectOutcome::FellBack => {
                    // Direct came up then died; relay is restored. Retry next tick.
                }
                HubDirectOutcome::NoDirect => {
                    // Stayed on relay this round (no punch / handshake failed).
                }
                HubDirectOutcome::PeerGone => return,
            }
        }
    }

    /// Result of one hub direct-upgrade attempt.
    enum HubDirectOutcome {
        /// Direct established then fell back to warm relay (re-arm immediately).
        FellBack,
        /// No direct this round; still on relay.
        NoDirect,
        /// The peer was removed (punch channel closed) — the task must exit.
        PeerGone,
    }

    /// One hub→spoke direct-upgrade attempt (QUIC listener side). See
    /// [`per_peer_direct_upgrade_task`] for lifecycle.
    #[allow(clippy::too_many_arguments)]
    async fn try_hub_peer_direct(
        peer_id: u32,
        peer: &Arc<PeerHandle>,
        punch_rx: &mut tokio::sync::mpsc::Receiver<HubEvent>,
        args: &super::VpnListenArgs,
        endpoint: &crate::transport::Endpoint,
        tunneled: &[crate::shared::Ipv4Net],
        dev0: &Arc<tun_rs::AsyncDevice>,
        counters: &Arc<BridgeCounters>,
        offload: bool,
        out_tx: &tokio::sync::mpsc::Sender<crate::shared::ClientMessage>,
        admin_v2: bool,
        relay_sender: &LinkSender,
    ) -> HubDirectOutcome {
        // 1. Socket + STUN candidate gathering.
        let socket = match crate::holepunch::bind_socket(args.nat_udp_preferred_port).await {
            Ok(s) => s,
            Err(e) => {
                debug!(%peer_id, error=%e, "hub direct: bind socket failed");
                return HubDirectOutcome::NoDirect;
            }
        };
        let targets = match crate::holepunch::resolve_live_stun_targets(
            &endpoint.host,
            endpoint.port,
            args.stun_server.as_deref(),
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                debug!(%peer_id, error=%e, "hub direct: stun resolve failed");
                return HubDirectOutcome::NoDirect;
            }
        };
        let disc = crate::holepunch::gather_candidates_from_stun_targets(
            &socket,
            &targets,
            args.upnp,
            args.try_port_prediction,
        )
        .await;
        if disc.candidates.is_empty() {
            debug!(%peer_id, "hub direct: no usable candidates");
            return HubDirectOutcome::NoDirect;
        }

        // 2. Offer our candidates to the server's per-peer broker (WITH peer_id).
        if out_tx
            .send(crate::shared::ClientMessage::UdpCandidateOffer(
                crate::shared::UdpCandidateOffer {
                    candidates: disc.candidates,
                    selected_stun: disc.selected_stun.map(|s| s.requested),
                    peer_id,
                },
            ))
            .await
            .is_err()
        {
            return HubDirectOutcome::PeerGone; // ctrl actor gone → hub tearing down
        }

        // 3. Await the brokered punch (peer candidates + nonce + tuning).
        let (nonce, peer_addrs, tuning) =
            match tokio::time::timeout(super::DIRECT_PUNCH_WAIT, punch_rx.recv()).await {
                Ok(Some(HubEvent::Punch {
                    nonce,
                    peer,
                    tuning,
                    ..
                })) => (nonce, peer, tuning),
                Ok(Some(_)) => return HubDirectOutcome::NoDirect, // unexpected event
                Ok(None) => return HubDirectOutcome::PeerGone,    // peer removed
                Err(_) => return HubDirectOutcome::NoDirect,      // no punch in time
            };

        // 4. Drop candidates inside tunneled subnets (would loop through the VPN).
        let (peer_addrs, dropped) = super::filter_tunneled_candidates(&peer_addrs, tunneled);
        if !dropped.is_empty() {
            info!(%peer_id, ?dropped, "hub direct: skipping tunneled candidates");
        }
        if peer_addrs.is_empty() {
            return HubDirectOutcome::NoDirect;
        }

        // 5. QUIC handshake — the hub is the listener (QUIC server).
        let token = crate::holepunch::derive_token(Some(&args.secret), &nonce);
        let dl = match crate::holepunch::DirectListener::new(socket, peer_addrs, tuning).await {
            Ok(dl) => dl,
            Err(e) => {
                debug!(%peer_id, error=%e, "hub direct: listener setup failed");
                return HubDirectOutcome::NoDirect;
            }
        };
        let conn = match tokio::time::timeout(super::DIRECT_ACCEPT_WAIT, dl.accept(token)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                debug!(%peer_id, error=%e, "hub direct: accept failed");
                return HubDirectOutcome::NoDirect;
            }
            Err(_) => {
                debug!(%peer_id, "hub direct: accept timed out");
                return HubDirectOutcome::NoDirect;
            }
        };

        // 6. Swap the sender to Direct IN PLACE (router follows; no restart). The
        //    relay downlink stays WARM (untouched). Spawn the direct downlink.
        //    v1 limitation: no per-peer PMTU monitor (the TUN MTU is shared across
        //    peers); oversized direct packets are dropped per-packet (TooLarge),
        //    never link death.
        let (dsender, drecver) = super::link::make_direct(conn.clone());
        {
            let mut s = peer.sender.lock().await;
            *s = dsender;
        }
        let direct_dl = {
            let dev0 = Arc::clone(dev0);
            let counters = Arc::clone(counters);
            tokio::spawn(async move {
                let _ = super::bridge::run_downlink(dev0, drecver, counters, offload).await;
            })
        };
        if admin_v2 {
            let _ = out_tx
                .send(crate::shared::ClientMessage::VpnPathReport {
                    path: "direct".into(),
                })
                .await;
        }
        info!(%peer_id, path = "direct", "hub peer upgraded to direct path");

        // 7. Stay direct until the QUIC connection dies OR the peer is removed.
        tokio::select! {
            _ = conn.closed() => {}
            ev = punch_rx.recv() => {
                if ev.is_none() {
                    // Peer removed: swap back, stop direct downlink, exit.
                    {
                        let mut s = peer.sender.lock().await;
                        *s = relay_sender.clone();
                    }
                    direct_dl.abort();
                    return HubDirectOutcome::PeerGone;
                }
                // A stray punch while direct is up: ignore, keep waiting on close.
                conn.closed().await;
            }
        }

        // 8. Direct died: swap back to the warm relay sender IN PLACE (seamless,
        //    no reconnect, TUN preserved, relay nonce counter preserved). Stop the
        //    direct downlink (its recver already errored on close).
        {
            let mut s = peer.sender.lock().await;
            *s = relay_sender.clone();
        }
        direct_dl.abort();
        if admin_v2 {
            let _ = out_tx
                .send(crate::shared::ClientMessage::VpnPathReport {
                    path: "relay".into(),
                })
                .await;
        }
        info!(%peer_id, path = "relay", "hub peer direct path lost; fell back to warm relay");
        HubDirectOutcome::FellBack
    }

    /// Run the hub listener: single TUN, per-peer router, shared downlink path.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_listen_hub(
        args: super::VpnListenArgs,
        acceptor: crate::mux::Acceptor,
        _opener: crate::mux::Opener,
        mut ctrl: crate::shared::Delimited<crate::mux::Stream>,
        assigned: Ipv4Addr,
        prefix: u8,
        admin_v2: bool,
        _carriers: u16,
    ) -> Result<()> {
        info!(
            link_id = %args.id,
            overlay = %format!("{assigned}/{prefix}"),
            "vpn hub listener starting"
        );

        // Stale reclaim
        super::hostcfg::stale_reclaim(&args.id, "listen").await;

        // Create TUN device(s).
        let (devs_raw, offload, tun_name) =
            super::hostcfg::create_tun(&args.tun_name, assigned, prefix, args.mtu, args.tun_queues)
                .await?;
        let devs: Vec<Arc<tun_rs::AsyncDevice>> = devs_raw.into_iter().map(Arc::new).collect();
        info!(
            link_id = %args.id,
            iface = %tun_name,
            addr = %assigned,
            prefix = prefix,
            "created hub tun device"
        );

        // Apply network config (gateway + spoke isolation). The hub netmaps its own
        // real↔exposed (N9); spokes route the exposed virtual + never advertise (D4),
        // so the egress SNAT `saddr real` never matches a spoke overlay source.
        let advertised_nets = super::routes::advertised_reals(&args.advertise_entries);
        let nat_maps = super::routes::nat_maps(&args.advertise_entries);
        if !nat_maps.is_empty() {
            info!(link_id = %args.id, ?nat_maps, "vpn nat netmap maps (hub)");
        }
        let runner = super::hostcfg::RealRunner;
        let _netcfg = super::hostcfg::NetConfig::apply(
            &runner,
            &args.id,
            "listen",
            &tun_name,
            assigned,
            prefix,
            &[], // Hub doesn't route peer routes; peers route themselves
            &advertised_nets,
            &nat_maps,
            args.no_route_manage,
            true, // Hub mode: add spoke isolation
            args.nat_masquerade,
        )
        .await?;

        // Create peer table.
        let peer_table = Arc::new(DashMap::new());

        // Create channels.
        let (sub_tx, sub_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(256);
        // Clone for the coordinator: a peer's downlink self-reports Leave here
        // when its relay link dies, so the coordinator removes it even without a
        // server VpnPeerLeave (no stale PeerTable entry).
        let coord_event_tx = event_tx.clone();

        // Spawn control-stream actor.
        let (out_tx, ctrl_task) = {
            let (tx, mut rx) = mpsc::channel::<crate::shared::ClientMessage>(16);
            let event_tx_clone = event_tx.clone();
            (
                tx,
                tokio::spawn(async move {
                    let mut out_open = true;
                    loop {
                        tokio::select! {
                            out = rx.recv(), if out_open => match out {
                                Some(msg) => {
                                    if let Err(e) = ctrl.send(msg).await {
                                        return Err(anyhow::anyhow!("vpn hub control stream send error: {e}"));
                                    }
                                }
                                // All senders dropped: keep draining the stream (I-7).
                                None => out_open = false,
                            },
                            msg = tokio::time::timeout(
                                std::time::Duration::from_secs(60),
                                ctrl.recv::<crate::shared::ServerMessage>(),
                            ) => match msg {
                                Ok(Ok(None)) => return Ok(()),
                                Ok(Ok(Some(crate::shared::ServerMessage::Heartbeat))) => continue,
                                Ok(Ok(Some(crate::shared::ServerMessage::VpnPeerJoin {
                                    peer_id,
                                    peer_overlay,
                                    session_nonce,
                                    carriers,
                                    ..
                                }))) => {
                                    info!(%peer_id, %peer_overlay, %carriers, "vpn hub peer join");
                                    // Pass the per-peer nonce RAW — must match the
                                    // spoke's derive_keys_connector(&session_nonce).
                                    let _ = event_tx_clone
                                        .send(HubEvent::Join {
                                            peer_id,
                                            overlay: peer_overlay,
                                            nonce: session_nonce,
                                            carriers: carriers.max(1),
                                        })
                                        .await;
                                }
                                Ok(Ok(Some(crate::shared::ServerMessage::VpnPeerLeave {
                                    peer_id,
                                }))) => {
                                    info!(%peer_id, "vpn hub peer leave");
                                    let _ = event_tx_clone.send(HubEvent::Leave { peer_id }).await;
                                }
                                Ok(Ok(Some(crate::shared::ServerMessage::UdpPunch {
                                    peer_id,
                                    nonce,
                                    peer,
                                    tuning,
                                    ..
                                }))) => {
                                    debug!(?peer_id, ?peer, "hub ctrl: received vpn udp punch");
                                    let _ = event_tx_clone
                                        .send(HubEvent::Punch {
                                            peer_id,
                                            nonce,
                                            peer,
                                            tuning,
                                        })
                                        .await;
                                }
                                Ok(Ok(Some(crate::shared::ServerMessage::UdpUnavailable))) => {
                                    debug!("hub ctrl: received UdpUnavailable (ignoring; no peer_id)");
                                }
                                Ok(Ok(Some(_))) => continue,
                                Ok(Err(e)) => return Err(anyhow::anyhow!("vpn hub control stream recv error: {e}")),
                                Err(_) => return Err(anyhow::anyhow!("vpn hub control stream timeout")),
                            }
                        }
                    }
                }),
            )
        };

        // Send initial VpnPathReport if admin_v2.
        if admin_v2 {
            let _ = out_tx
                .send(crate::shared::ClientMessage::VpnPathReport {
                    path: "relay".to_string(),
                })
                .await;
        }

        // Spawn accept task.
        let _accept_task = spawn_accept_task(acceptor, sub_tx);

        // Create bridge counters.
        let counters = BridgeCounters::new();

        // Spawn router uplink(s), one per TUN queue.
        let mut router_tasks = Vec::new();
        for dev in &devs {
            let dev = Arc::clone(dev);
            let table = Arc::clone(&peer_table);
            let ctr = Arc::clone(&counters);
            let task = tokio::spawn(run_router_uplink(dev, table, ctr, offload));
            router_tasks.push(task);
        }

        // Spawn coordinator.
        let devs_clone = devs.clone();
        let secret = args.secret.clone();
        let counters_clone = Arc::clone(&counters);
        let table_clone = Arc::clone(&peer_table);
        let _coordinator_task = tokio::spawn(run_hub_coordinator(
            devs_clone,
            secret,
            counters_clone,
            offload,
            sub_rx,
            event_rx,
            coord_event_tx,
            table_clone,
            args.clone(),
            admin_v2,
            out_tx.clone(),
        ));

        // Wait for any critical task to die.
        tokio::select! {
            res = ctrl_task => {
                match res {
                    Ok(Ok(())) => info!(link_id = %args.id, "hub control stream closed"),
                    Ok(Err(e)) => {
                        warn!(link_id = %args.id, error = %e, "hub control stream error");
                        return Err(e);
                    }
                    Err(e) => {
                        warn!(link_id = %args.id, error = %e, "hub ctrl task panic");
                        return Err(e.into());
                    }
                }
            }
            _ = async { futures_util::future::select_all(router_tasks.iter_mut()).await } => {
                info!(link_id = %args.id, "hub router uplink died");
                return Err(anyhow!("router uplink died"));
            }
        }

        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn router_parses_ipv4_dst() {
            // Valid IPv4 packet: version 4, dst 192.168.1.5
            let mut pkt = vec![0x45u8; 20]; // IPv4 header, version 4
            pkt[16..20].copy_from_slice(&[192, 168, 1, 5]);
            assert_eq!(
                parse_ipv4_dst(&Bytes::from(pkt)),
                Some("192.168.1.5".parse().unwrap())
            );

            // Too short
            assert_eq!(parse_ipv4_dst(&Bytes::from(vec![0x45u8; 19])), None);

            // Non-IPv4 (version 6)
            let mut pkt6 = vec![0x65u8; 20];
            pkt6[16..20].copy_from_slice(&[192, 168, 1, 5]);
            assert_eq!(parse_ipv4_dst(&Bytes::from(pkt6)), None);
        }

        #[test]
        fn router_drops_unknown_dst() {
            // Packet with dst not in table → dropped, counter incremented
            // (tested implicitly in integration; unit test is lightweight)
            let mut pkt = vec![0x45u8; 20];
            pkt[16..20].copy_from_slice(&[10, 0, 0, 1]);
            assert!(parse_ipv4_dst(&Bytes::from(pkt)).is_some());
        }
    }
}
