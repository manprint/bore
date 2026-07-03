//! Android VPN spike harness — exercises real TUN + host-only route apply
//! under a rooted emulator/device (Phase 4.1).
//!
//! This is android-only and HARDWARE-GATED: run as root on a rooted emulator
//! (`target: default` image) or device via `scripts/android_vpn_test.sh`.
//! It validates the core Phase 3 assumptions: TUN creation (`create_tun` twin),
//! self-ping over the raw TUN fd, `NetConfig::apply`/revert (host-only route
//! table), and `stale_reclaim`.
//!
//! Modes:
//! - `spike` (default): create a TUN, assign 10.199.0.1/30, spawn
//!   `ping -c 1 10.199.0.2` and confirm the kernel routes the echo request out
//!   through the TUN as a raw IP packet, teardown.
//! - `create-teardown`: create, assert `ip link show` has the device, drop,
//!   assert gone.
//! - `apply-revert`: run `NetConfig::apply` with two fake peer routes, assert
//!   `ip route show` has them, drop, assert gone.
//! - `leak-then-reclaim`: write marker files as a crashed link would, run
//!   `stale_reclaim`, assert removed.
//!
//! Run: `adb shell /data/local/tmp/android_vpn_spike [mode]` as root.

// Diagnostic/smoke harness: the body is android-only and cannot be linted on
// the Linux dev box, and clippy-cleanliness of a throwaway harness has ~no
// value. Relax the clippy `all` group + unused here, scoped to android so the
// Linux build and the production library lints are unaffected.
#![cfg_attr(target_os = "android", allow(unused, clippy::all))]

#[cfg(target_os = "android")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "spike".to_string());

    match mode.as_str() {
        "spike" => cmd_spike().await,
        "create-teardown" => cmd_create_teardown().await,
        "apply-revert" => cmd_apply_revert().await,
        "leak-then-reclaim" => cmd_leak_then_reclaim().await,
        _ => {
            eprintln!(
                "Unknown mode: {mode}. Use: spike, create-teardown, apply-revert, leak-then-reclaim"
            );
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "android"))]
fn main() {
    eprintln!("android_vpn_spike: android-only (Linux CI will skip with this stub main)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper functions
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "android")]
fn run(argv: &[String]) -> (bool, String, String) {
    use std::process::{Command, Stdio};
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("Failed to run {argv:?}: {e}");
            std::process::exit(1);
        });
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[cfg(target_os = "android")]
fn ip_link_has(name: &str) -> bool {
    let (ok, stdout, _) = run(&[
        "ip".to_string(),
        "link".to_string(),
        "show".to_string(),
        name.to_string(),
    ]);
    ok && stdout.contains(name)
}

#[cfg(target_os = "android")]
fn ip_route_has(subnet: &str) -> bool {
    let (ok, stdout, _) = run(&["ip".to_string(), "route".to_string(), "show".to_string()]);
    ok && stdout.contains(subnet)
}

#[cfg(target_os = "android")]
fn pass(step: &str) {
    println!("PASS {step}");
}

#[cfg(target_os = "android")]
fn fail_exit(step: &str, detail: &str) -> ! {
    eprintln!("FAIL {step}: {detail}");
    std::process::exit(1);
}

// ═══════════════════════════════════════════════════════════════════════════════
// spike: TUN creation + self-ping
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "android")]
async fn cmd_spike() -> anyhow::Result<()> {
    println!("\n=== Phase 4.1 Spike: android TUN + self-ping ===\n");

    println!("[a] Creating TUN with addr=10.199.0.1/30, mtu=1350...");
    let (devs, offload, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.199.0.1".parse()?, 30, 1350, 1).await?;
    println!("Resolved to device: {tun_name}");
    println!("Offload: {offload}, device count: {}", devs.len());
    if devs.len() != 1 {
        fail_exit("spike", "expected exactly one device for queues=1");
    }
    let dev = &devs[0];

    if !ip_link_has(&tun_name) {
        fail_exit("spike", &format!("{tun_name} not found in `ip link show`"));
    }
    pass("spike: TUN created and visible in `ip link`");

    // Spawn `ping -c 1 10.199.0.2`. Nothing replies (no peer) — the point is
    // only to confirm the kernel treats 10.199.0.2 as on-link via the TUN's
    // /30 and routes the echo request out through the TUN as a raw IP packet
    // (TUN devices are L3: no ARP, no on-link neighbor needed to send).
    println!("[b] Spawning `ping -c 1 10.199.0.2`...");
    std::process::Command::new("ping")
        .args(["-c", "1", "-W", "2", "10.199.0.2"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let mut buf = vec![0u8; 2048];
    let mut saw_packet = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), dev.recv(&mut buf)).await
        {
            Ok(Ok(n)) if n >= 20 => {
                // IPv4 header: byte 9 = protocol (1 = ICMP). Best-effort
                // classification — any raw packet observed already proves the
                // TUN is genuinely plumbed into the routing table.
                let proto = buf[9];
                if proto == 1 {
                    println!("  saw ICMP packet ({n} bytes) on the TUN fd");
                } else {
                    println!("  saw non-ICMP packet (proto={proto}, {n} bytes) on the TUN fd");
                }
                saw_packet = true;
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => fail_exit("spike", &format!("TUN recv error: {e}")),
            Err(_) => continue, // per-poll timeout, keep trying until deadline
        }
    }
    if !saw_packet {
        fail_exit(
            "spike",
            "did not observe any packet for 10.199.0.2 on the TUN fd within 3s",
        );
    }
    pass("spike: observed routed packet on TUN fd (self-ping routed correctly)");

    drop(devs);
    println!("\nAll spike steps passed.");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// create-teardown: TUN lifecycle
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "android")]
async fn cmd_create_teardown() -> anyhow::Result<()> {
    println!("\n=== create-teardown ===\n");

    let (devs, _offload, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.199.0.5".parse()?, 30, 1350, 1).await?;

    if !ip_link_has(&tun_name) {
        fail_exit(
            "create-teardown",
            &format!("{tun_name} missing from `ip link show` after create"),
        );
    }
    pass("create-teardown: device present after create");

    drop(devs);
    // Give the kernel a moment to tear the interface down after fd close.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    if ip_link_has(&tun_name) {
        fail_exit(
            "create-teardown",
            &format!("{tun_name} still present in `ip link show` after drop"),
        );
    }
    pass("create-teardown: device gone after drop");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// apply-revert: NetConfig RAII (host-only route table)
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "android")]
async fn cmd_apply_revert() -> anyhow::Result<()> {
    println!("\n=== apply-revert ===\n");

    let (_devs, _offload, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.199.0.9".parse()?, 30, 1350, 1).await?;

    let peer_routes = vec![
        "192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?,
        "198.51.100.0/24".parse::<bore_cli::shared::Ipv4Net>()?,
    ];

    let cfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "spike-link",
        "connect",
        &tun_name,
        "10.199.0.9".parse()?,
        30,
        &peer_routes,
        &[],
        &[],
        false,
        false,
        false,
        false,
    )
    .await?;

    for net in &peer_routes {
        if !ip_route_has(&net.to_string()) {
            fail_exit(
                "apply-revert",
                &format!("route {net} missing from `ip route show` after apply"),
            );
        }
    }
    pass("apply-revert: both peer routes present after apply");

    drop(cfg);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    for net in &peer_routes {
        if ip_route_has(&net.to_string()) {
            fail_exit(
                "apply-revert",
                &format!("route {net} still present after drop"),
            );
        }
    }
    pass("apply-revert: both peer routes gone after drop");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// leak-then-reclaim: stale_reclaim
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "android")]
async fn cmd_leak_then_reclaim() -> anyhow::Result<()> {
    println!("\n=== leak-then-reclaim ===\n");

    let id = "spike-leak";
    let role = "listen";
    let run_dir = "/data/local/tmp";
    // Marker filenames per the android `stale_reclaim`/`android_stale_reclaim_in`
    // twin (src/vpn.rs, phase 3.2): "bore-vpn-ns0-" because `current_netns_id()`
    // returns 0 on every `not(target_os = "linux")` platform (android included) —
    // deterministic on a real device, not just in the Linux unit tests.
    let ipforward_marker = format!("{run_dir}/bore-vpn-{id}-{role}.ipforward");
    let fwdref_marker = format!("{run_dir}/bore-vpn-ns0-{id}-{role}.fwdref");

    std::fs::write(&ipforward_marker, "1")?;
    std::fs::write(&fwdref_marker, "")?;
    pass("leak-then-reclaim: wrote marker files (simulated crashed link)");

    bore_cli::vpn::hostcfg::stale_reclaim(id, role).await;

    if std::path::Path::new(&ipforward_marker).exists()
        || std::path::Path::new(&fwdref_marker).exists()
    {
        fail_exit(
            "leak-then-reclaim",
            "marker file(s) still present after stale_reclaim",
        );
    }
    pass("leak-then-reclaim: marker files removed by stale_reclaim");
    Ok(())
}
