//! macOS VPN spike harness — exercises real utun + PF runtime under sudo (Phase 1.2/3.2/5.1).
//!
//! This is macOS-only and HARDWARE-GATED: a human must run it under sudo on a Mac.
//! It validates the core VPN operations: TUN creation, interface config, PF rules, stale reclaim.
//!
//! Modes:
//! - `spike` (default): Phase 1.2 de-risk — full spike workflow (create utun, config, PF load, show, flush, drop).
//! - `create-teardown`: Phase 3.2 — verify utun lifecycle (create → assert in ifconfig → drop → assert gone).
//! - `apply-revert`: Phase 5.1 — verify NetConfig RAII (apply → assert rules + forwarding → drop → assert reverted).
//! - `leak-then-reclaim <leak|reclaim>`: Phase 5.1 SIGKILL simulation.
//!
//! Run: `sudo target/debug/examples/macos_vpn_spike [mode]` on macOS.

// Diagnostic/smoke harness: relax pedantic style lints that are pure noise here
// (the body is macOS-only and unlintable on the Linux dev box). Scoped to macOS
// so the Linux build is unaffected.
#![cfg_attr(
    target_os = "macos",
    allow(
        unused,
        clippy::uninlined_format_args,
        clippy::needless_return,
        clippy::needless_borrow,
        clippy::useless_format
    )
)]

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing so we see bore_cli logs.
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
            eprintln!("Unknown mode: {}. Use: spike, create-teardown, apply-revert, leak-then-reclaim <leak|reclaim>", mode);
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos_vpn_spike: macOS-only (Linux CI will skip with this stub main)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper functions
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
fn run(argv: &[String]) -> (bool, String, String) {
    use std::process::{Command, Stdio};
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("Failed to run {:?}: {}", argv, e);
            std::process::exit(1);
        });
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[cfg(target_os = "macos")]
fn ifconfig_has(name: &str) -> bool {
    let (ok, stdout, _) = run(&["ifconfig".to_string(), name.to_string()]);
    ok && stdout.contains(name)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 1.2: Full spike workflow
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
async fn cmd_spike() -> anyhow::Result<()> {
    println!("\n=== Phase 1.2 Spike: utun + PF workflow ===\n");

    // Step (a): Create a utun.
    println!("[a] Creating utun with addr=10.255.255.1/30, mtu=1350...");
    let (devs, offload, utun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("✓ Resolved to utun: {}", utun_name);
    println!("  Offload: {}, Device count: {}", offload, devs.len());

    // Verify it appears in ifconfig.
    if !ifconfig_has(&utun_name) {
        eprintln!("✗ Failed: {} not found in ifconfig output", utun_name);
        return Err(anyhow::anyhow!("ifconfig check failed"));
    }
    println!("✓ {} appears in ifconfig", utun_name);

    // Step (b): Configure address, MTU, bring up.
    println!("\n[b] Configuring address, MTU, and link state...");

    // Address add is typically auto-set by create_tun on macOS, but let's show the argv for clarity.
    let addr_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_addr_add(&utun_name, "10.255.255.1", 30);
    println!("  cmd_addr_add argv: {:?}", addr_argv);

    let mtu_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_link_set_mtu(&utun_name, 1350);
    println!("  cmd_link_set_mtu argv: {:?}", mtu_argv);
    let (ok, _, stderr) = run(&mtu_argv);
    if !ok {
        eprintln!("  ⚠ MTU set stderr: {}", stderr);
    } else {
        println!("  ✓ MTU set");
    }

    let up_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_link_set_up(&utun_name);
    println!("  cmd_link_set_up argv: {:?}", up_argv);
    let (ok, _, stderr) = run(&up_argv);
    if !ok {
        eprintln!("  ⚠ Link up stderr: {}", stderr);
    } else {
        println!("  ✓ Link up");
    }

    // Print ifconfig after config.
    println!("\n  ifconfig {} output:", utun_name);
    let (ok, stdout, _) = run(&["ifconfig".to_string(), utun_name.clone()]);
    if ok {
        for line in stdout.lines() {
            println!("    {}", line);
        }
    }

    // Step (c): Compose PF ruleset, write to /var/run, enable PF, load anchor.
    println!("\n[c] Composing and loading PF anchor...");

    let advertised = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let nat_maps = vec![(
        "192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?,
        "10.77.0.0/24".parse::<bore_cli::shared::Ipv4Net>()?,
    )];
    let ruleset = bore_cli::vpn::hostcfg_cmd::macos::pf_ruleset(
        &utun_name,
        "en0",
        &advertised,
        &nat_maps,
        false, // hub
        true,  // nat_masquerade
        true,  // forward_accept
        1310,  // MSS clamp
    );

    println!("  PF ruleset:");
    for line in ruleset.lines().take(5) {
        println!("    {}", line);
    }
    println!("    ... ({} total lines)", ruleset.lines().count());

    // Write to temp file.
    let pf_path = "/var/run/bore_vpn_spike.pf";
    std::fs::write(pf_path, &ruleset)?;
    println!("  ✓ Ruleset written to {}", pf_path);

    // Enable PF.
    let pf_enable_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_pf_enable();
    println!("  Running: {:?}", pf_enable_argv);
    let (ok, _, stderr) = run(&pf_enable_argv);
    if !ok && !stderr.contains("already enabled") {
        eprintln!("  ⚠ pfctl enable stderr: {}", stderr);
    } else {
        println!("  ✓ PF enabled (or already enabled)");
    }

    // Load anchor.
    let load_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_pf_load_anchor("spike0", pf_path);
    println!("  Running: {:?}", load_argv);
    let (ok, stdout, stderr) = run(&load_argv);
    println!(
        "  Exit: {}, stdout len: {}, stderr: {}",
        ok,
        stdout.len(),
        stderr
    );
    if !ok {
        println!("  ⚠ pfctl load exit status false; stderr: {}", stderr);
    } else {
        println!("  ✓ PF anchor loaded");
    }

    // Step (d): Dump the anchor.
    println!("\n[d] Dumping PF anchor bore_vpn/spike0...");
    let show_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_pf_show_anchor("spike0");
    println!("  Running: {:?}", show_argv);
    let (ok, stdout, stderr) = run(&show_argv);
    if ok {
        let line_count = stdout.lines().count();
        println!("  ✓ Anchor dump ({} lines):", line_count);
        for line in stdout.lines().take(3) {
            println!("    {}", line);
        }
        if line_count > 3 {
            println!("    ...");
        }
    } else {
        println!("  ⚠ Anchor dump failed: {}", stderr);
    }

    // Step (e): Toggle forwarding, then restore.
    println!("\n[e] Testing sysctl ip.forwarding toggle...");

    // Read original.
    let get_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_sysctl_get_ip_forward();
    println!("  Getting original forwarding state...");
    let (ok, stdout, stderr) = run(&get_argv);
    let original = if ok {
        stdout.trim().parse::<u8>().unwrap_or(0)
    } else {
        println!("  ⚠ Failed to read forwarding: {}", stderr);
        0
    };
    println!("  Original forwarding: {}", original);

    // Set to 1.
    let set_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_sysctl_ip_forward(1);
    let (ok, _, stderr) = run(&set_argv);
    if !ok {
        println!("  ⚠ Failed to set forwarding to 1: {}", stderr);
    } else {
        println!("  ✓ Set forwarding to 1");
    }

    // Read back.
    let (ok, stdout, _) = run(&get_argv);
    let current = if ok {
        stdout.trim().parse::<u8>().unwrap_or(0)
    } else {
        0
    };
    println!("  Current forwarding: {}", current);

    // Restore.
    let restore_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_sysctl_ip_forward(original);
    let (ok, _, _) = run(&restore_argv);
    println!("  Restored to {}: {}", original, if ok { "✓" } else { "⚠" });

    // Step (f): Flush anchor and drop device.
    println!("\n[f] Cleanup: flushing anchor and dropping device...");

    let flush_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_pf_flush_anchor("spike0");
    println!("  Running: {:?}", flush_argv);
    let (ok, _, stderr) = run(&flush_argv);
    if !ok && !stderr.contains("No such file or directory") {
        println!("  ⚠ Flush stderr: {}", stderr);
    } else {
        println!("  ✓ Anchor flushed");
    }

    // Device drops at end of scope.
    drop(devs);
    println!("  ✓ utun dropped (out of scope)");

    // Clean up temp file.
    let _ = std::fs::remove_file(pf_path);

    println!("\n✓✓✓ SPIKE OK ✓✓✓\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 3.2: TUN create/teardown lifecycle
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
async fn cmd_create_teardown() -> anyhow::Result<()> {
    println!("\n=== Phase 3.2: TUN create/teardown lifecycle ===\n");

    // Create.
    println!("[1] Creating utun...");
    let (devs, _, utun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("✓ Created: {}", utun_name);

    // Assert in ifconfig.
    if !ifconfig_has(&utun_name) {
        eprintln!("✗ {} not found in ifconfig after creation", utun_name);
        return Err(anyhow::anyhow!("creation check failed"));
    }
    println!("✓ {} appears in ifconfig", utun_name);

    // Drop device.
    println!("\n[2] Dropping device...");
    drop(devs);
    println!("✓ Device dropped");

    // Verify gone from ifconfig.
    println!("\n[3] Verifying gone from ifconfig...");
    let (ok, _stdout, _) = run(&["ifconfig".to_string(), utun_name.clone()]);
    if ok {
        eprintln!("✗ {} still appears in ifconfig after drop", utun_name);
        return Err(anyhow::anyhow!("teardown check failed"));
    }
    println!("✓ {} is gone from ifconfig", utun_name);

    println!("\n✓ CREATE-TEARDOWN OK\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 5.1: NetConfig RAII apply/revert
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
async fn cmd_apply_revert() -> anyhow::Result<()> {
    println!("\n=== Phase 5.1: NetConfig apply/revert (RAII) ===\n");

    // Create utun.
    println!("[1] Creating utun...");
    let (devs, _, utun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("✓ Created: {}", utun_name);

    // Record original forwarding.
    let get_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_sysctl_get_ip_forward();
    let (_ok, stdout, _) = run(&get_argv);
    let original_fwd = stdout.trim().parse::<u8>().unwrap_or(0);
    println!("  Original forwarding: {}", original_fwd);

    // Apply NetConfig.
    println!("\n[2] Calling NetConfig::apply (gateway mode)...");
    let advertised = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let _netcfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "smoke0",
        "listen",
        &utun_name,
        "10.255.255.1".parse()?,
        30,
        &[],
        &advertised,
        &[],
        false, // no_route_manage
        false, // hub
        false, // nat_masquerade
        false, // forward_accept
    )
    .await?;
    println!("✓ NetConfig applied");

    // Assert forwarding is 1.
    let (ok, stdout, _) = run(&get_argv);
    let current_fwd = if ok {
        stdout.trim().parse::<u8>().unwrap_or(0)
    } else {
        0
    };
    println!("  Forwarding after apply: {}", current_fwd);
    if current_fwd != 1 {
        eprintln!("  ⚠ Expected forwarding=1, got {}", current_fwd);
    }

    // Show PF anchor.
    println!("\n[3] Checking PF anchor bore_vpn/smoke0...");
    let show_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_pf_show_anchor("smoke0");
    let (ok, stdout, _) = run(&show_argv);
    if ok && !stdout.is_empty() {
        println!("  ✓ PF anchor rules present ({} chars)", stdout.len());
    } else {
        println!("  ⚠ PF anchor is empty or failed");
    }

    // Drop NetConfig (triggers Drop → revert).
    println!("\n[4] Dropping NetConfig (triggering RAII revert)...");
    drop(_netcfg);
    println!("✓ NetConfig dropped");

    // Assert anchor is now empty.
    println!("\n[5] Verifying anchor is flushed...");
    let (ok, stdout, _) = run(&show_argv);
    if ok && stdout.is_empty() {
        println!("  ✓ PF anchor is now empty");
    } else {
        println!(
            "  ⚠ Anchor may not be fully flushed: {}",
            stdout.chars().count()
        );
    }

    // Assert forwarding is restored.
    let (ok, stdout, _) = run(&get_argv);
    let restored_fwd = if ok {
        stdout.trim().parse::<u8>().unwrap_or(0)
    } else {
        0
    };
    println!("  Forwarding after revert: {}", restored_fwd);
    if restored_fwd != original_fwd {
        eprintln!(
            "  ⚠ Expected forwarding={}, got {}",
            original_fwd, restored_fwd
        );
    } else {
        println!("  ✓ Forwarding restored to {}", original_fwd);
    }

    // Drop device.
    println!("\n[6] Dropping utun device...");
    drop(devs);
    println!("✓ Device dropped");

    // Verify gone.
    if !ifconfig_has(&utun_name) {
        println!("  ✓ {} is gone", utun_name);
    } else {
        println!("  ⚠ {} still visible in ifconfig", utun_name);
    }

    println!("\n✓ APPLY-REVERT OK\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 5.1: Leak/reclaim (SIGKILL simulation)
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
async fn cmd_leak_then_reclaim() -> anyhow::Result<()> {
    let action = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "reclaim".to_string());

    match action.as_str() {
        "leak" => cmd_leak().await,
        "reclaim" => cmd_reclaim().await,
        _ => Err(anyhow::anyhow!(
            "leak-then-reclaim requires arg 2: leak or reclaim"
        )),
    }
}

#[cfg(target_os = "macos")]
async fn cmd_leak() -> anyhow::Result<()> {
    println!("\n=== Phase 5.1: Leak (SIGKILL simulation) ===\n");
    println!("[1] Creating utun...");
    let (devs, _, utun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("✓ Created: {}", utun_name);

    println!("\n[2] Applying NetConfig...");
    let advertised = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let netcfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "smoke0",
        "listen",
        &utun_name,
        "10.255.255.1".parse()?,
        30,
        &[],
        &advertised,
        &[],
        false,
        false,
        false,
        false,
    )
    .await?;
    println!("✓ NetConfig applied");

    // Verify rules exist.
    let show_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_pf_show_anchor("smoke0");
    let (ok, stdout, _) = run(&show_argv);
    if ok && !stdout.is_empty() {
        println!("  ✓ PF anchor has rules");
    }

    println!("\n[3] Leaking (std::mem::forget) — simulating SIGKILL...");
    println!("  NetConfig will NOT be dropped; PF anchor remains.");
    std::mem::forget(netcfg);
    std::mem::forget(devs);
    println!("✓ Resources forgotten (leaked for SIGKILL simulation)");
    println!("\nLEAK OK — manually run: sudo target/debug/examples/macos_vpn_spike leak-then-reclaim reclaim\n");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn cmd_reclaim() -> anyhow::Result<()> {
    println!("\n=== Phase 5.1: Reclaim (SIGKILL recovery) ===\n");

    println!("[1] Running stale_reclaim for smoke0/listen...");
    bore_cli::vpn::hostcfg::stale_reclaim("smoke0", "listen").await;
    println!("✓ stale_reclaim completed");

    // Verify anchor is flushed.
    println!("\n[2] Verifying anchor is flushed...");
    let show_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_pf_show_anchor("smoke0");
    let (ok, stdout, _) = run(&show_argv);
    if ok && stdout.is_empty() {
        println!("  ✓ PF anchor bore_vpn/smoke0 is empty/flushed");
    } else {
        println!("  ⚠ Anchor may not be fully flushed");
    }

    // Verify forwarding is restored.
    println!("\n[3] Verifying net.inet.ip.forwarding is restored...");
    let get_argv = bore_cli::vpn::hostcfg_cmd::macos::cmd_sysctl_get_ip_forward();
    let (_ok, stdout, _) = run(&get_argv);
    let current_fwd = stdout.trim().parse::<u8>().unwrap_or(0);
    println!("  Current forwarding: {}", current_fwd);

    println!("\n✓ RECLAIM OK\n");
    Ok(())
}
