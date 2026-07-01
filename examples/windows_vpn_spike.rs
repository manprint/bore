//! Windows VPN spike harness — exercises real WinTun adapter + host-config
//! runtime on `windows-latest` (Phase 5.4 de-risk, mirrors `macos_vpn_spike.rs`).
//!
//! Windows-only. Intended to run on the hosted `windows-latest` CI runner
//! (`windows-vpn-e2e` job) — no self-hosted runner assumed. Validates the
//! single-host operations: WinTun adapter create/teardown, route add/del,
//! `NetConfig` RAII apply/revert (firewall rules + WinNAT + IPEnableRouter),
//! SIGKILL stale-reclaim, and the two-link `ip_forward` refcount.
//!
//! Explicitly OUT of scope here (needs a second real host, stays manual —
//! see `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md`): any test that requires actual
//! traffic through the gateway (T-WIN-MTU1) or a second VPN peer
//! (T-WIN-VPN-RELAY*/DIRECT*/CARR*, T-WIN-HUB*, T-WIN-GW*).
//!
//! Modes:
//! - `spike` (default): full workflow — create adapter, configure MTU, apply
//!   gateway NetConfig (forward-accept + nat-masquerade), verify, revert.
//! - `create-teardown`: WinTun adapter lifecycle only (T-WIN-TUN1/TUN4).
//! - `missing-dll`: `WintunDevice::open_or_create` with a bogus DLL path fails
//!   cleanly, before any adapter/host mutation (T-WIN-TUN5).
//! - `route-add-del`: `NetConfig::apply` peer-route add, visible via
//!   `Get-NetRoute`, removed on drop (T-WIN-HOST1).
//! - `apply-revert`: `NetConfig` RAII with `--forward-accept` +
//!   `--nat-masquerade` — firewall rules + WinNAT instance + IPEnableRouter
//!   appear, then vanish/restore on drop (T-WIN-FWD2, T-WIN-NAT1 partial).
//! - `forward-accept-off-warn`: gateway mode WITHOUT `--forward-accept` adds
//!   no firewall rules (the warn-only path, T-WIN-FWD1 partial).
//! - `two-link-refcount`: two concurrent gateway links share IPEnableRouter;
//!   dropping one leaves it enabled for the other, dropping the last restores
//!   the original value (T-WIN-HOST2).
//! - `leak-then-reclaim <leak|reclaim>`: SIGKILL simulation — leak then
//!   `stale_reclaim` cleans firewall/NAT/registry state (T-WIN-HOST3,
//!   T-WIN-STALE1/STALE2).
//!
//! Run: `target/debug/examples/windows_vpn_spike [mode]` on Windows. Unlike
//! the macOS spike this does NOT need an explicit `sudo`-equivalent prefix —
//! `check_root()`/`cmd_is_elevated` gates the real `bore vpn` CLI before this
//! harness is reached; whether the hosted runner's default process token is
//! already elevated is exactly what running this job empirically answers.

// Diagnostic/smoke harness: the body is Windows-only and cannot be linted on
// the Linux dev box, and clippy-cleanliness of a throwaway harness has ~no
// value. Relax the clippy `all` group + unused here, scoped to Windows so the
// Linux build and the production library lints are unaffected. (Real compile
// errors still fail the build — only style/lint noise is silenced.)
#![cfg_attr(target_os = "windows", allow(unused, clippy::all))]

#[cfg(target_os = "windows")]
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
        "missing-dll" => cmd_missing_dll().await,
        "route-add-del" => cmd_route_add_del().await,
        "apply-revert" => cmd_apply_revert().await,
        "forward-accept-off-warn" => cmd_forward_accept_off_warn().await,
        "two-link-refcount" => cmd_two_link_refcount().await,
        "leak-then-reclaim" => cmd_leak_then_reclaim().await,
        "diag-firewall" => cmd_diag_firewall().await,
        _ => {
            eprintln!(
                "Unknown mode: {}. Use: spike, create-teardown, missing-dll, route-add-del, \
                 apply-revert, forward-accept-off-warn, two-link-refcount, leak-then-reclaim <leak|reclaim>, \
                 diag-firewall",
                mode
            );
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("windows_vpn_spike: Windows-only (non-Windows CI will skip with this stub main)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper functions
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
fn ps(script: &str) -> Vec<String> {
    vec![
        "powershell".to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-Command".to_string(),
        script.to_string(),
    ]
}

#[cfg(target_os = "windows")]
fn adapter_exists(name: &str) -> bool {
    let (ok, stdout, _) = run(&ps(&format!(
        "(Get-NetAdapter -Name '{name}' -ErrorAction SilentlyContinue) -ne $null"
    )));
    ok && stdout.trim().eq_ignore_ascii_case("true")
}

// Scoped to one link's own rule-name prefix rather than the whole `bore-vpn`
// group — the CI job runs every mode's spike as a separate process on the
// SAME host, so firewall rules from an earlier step (or a prior failed run's
// leak) are otherwise indistinguishable from this step's own rules.
//
// Retries briefly: the first windows-vpn-e2e run showed `New-NetFirewallRule`
// report success while a `Get-NetFirewallRule` query from a freshly-spawned
// process found nothing for a few seconds afterward — a real propagation
// lag in the underlying firewall policy store, not a bore bug (nothing in
// the production apply() path reads back immediately after create; only
// this diagnostic script's eager check does). Retrying disambiguates "not
// visible yet" from "genuinely never created."
#[cfg(target_os = "windows")]
fn firewall_rule_count_for_link(id: &str, role: &str) -> usize {
    // Filters with `Where-Object -like` on the piped objects rather than the
    // `-DisplayName` param's own (unconfirmed) wildcard support — this is the
    // exact pattern the diag-firewall investigation proved works.
    for attempt in 0..5 {
        let (ok, stdout, _) = run(&ps(&format!(
            "(Get-NetFirewallRule -Group 'bore-vpn' -ErrorAction SilentlyContinue | Where-Object {{ $_.DisplayName -like 'bore-{id}-{role}-*' }} | Measure-Object).Count"
        )));
        let count: usize = if ok {
            stdout.trim().parse().unwrap_or(0)
        } else {
            0
        };
        if count > 0 || attempt == 4 {
            return count;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    0
}

#[cfg(target_os = "windows")]
fn nat_instance_exists(name: &str) -> bool {
    let (ok, stdout, _) = run(&ps(&format!(
        "(Get-NetNat -Name '{name}' -ErrorAction SilentlyContinue) -ne $null"
    )));
    ok && stdout.trim().eq_ignore_ascii_case("true")
}

#[cfg(target_os = "windows")]
fn read_ip_forward() -> u8 {
    let argv = bore_cli::vpn::hostcfg_cmd::windows::cmd_ip_forward_get();
    let (_ok, stdout, _) = run(&argv);
    bore_cli::vpn::hostcfg_cmd::windows::parse_ip_forward_output(&stdout)
}

// ═══════════════════════════════════════════════════════════════════════════════
// spike: full workflow
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_spike() -> anyhow::Result<()> {
    println!("\n=== Phase 5.4 Spike: WinTun + host-config workflow ===\n");

    println!("[a] Creating WinTun adapter addr=10.255.255.1/30, mtu=1350...");
    let (devs, offload, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!(
        "  Resolved adapter: {tun_name} (offload={offload}, devices={})",
        devs.len()
    );
    if !adapter_exists(&tun_name) {
        return Err(anyhow::anyhow!(
            "{tun_name} not found via Get-NetAdapter after creation"
        ));
    }
    println!("  OK: {tun_name} visible in Get-NetAdapter");

    println!("\n[b] Setting MTU via netsh...");
    let mtu_argv = bore_cli::vpn::hostcfg_cmd::windows::cmd_link_set_mtu(&tun_name, 1350);
    println!("  argv: {:?}", mtu_argv);
    let (ok, _, stderr) = run(&mtu_argv);
    println!(
        "  {}",
        if ok {
            "OK: MTU set"
        } else {
            "WARN: MTU set failed"
        }
    );
    if !ok {
        println!("    stderr: {stderr}");
    }

    println!("\n[c] Applying gateway NetConfig (forward-accept + nat-masquerade)...");
    let advertised = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let netcfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "spike0",
        "listen",
        &tun_name,
        "10.255.255.1".parse()?,
        30,
        &[],
        &advertised,
        &[],
        false, // no_route_manage
        false, // hub
        true,  // nat_masquerade
        true,  // forward_accept
    )
    .await?;
    println!("  OK: NetConfig applied");

    let fwd = read_ip_forward();
    println!("  IPEnableRouter after apply: {fwd}");
    if fwd != 1 {
        eprintln!("  WARN: expected IPEnableRouter=1, got {fwd}");
    }

    let rules = firewall_rule_count_for_link("spike0", "listen");
    println!("  bore-vpn firewall rules present: {rules}");
    if rules == 0 {
        eprintln!("  WARN: expected >=1 forward-accept firewall rule, found 0");
    }

    println!("\n[d] Dropping NetConfig (RAII revert)...");
    drop(netcfg);
    let fwd_after = read_ip_forward();
    let rules_after = firewall_rule_count_for_link("spike0", "listen");
    println!(
        "  IPEnableRouter after revert: {fwd_after} (expected 0 unless another link is active)"
    );
    println!("  bore-vpn firewall rules after revert: {rules_after} (expected 0)");
    if rules_after != 0 {
        eprintln!("  WARN: firewall rules did not fully revert");
    }

    println!("\n[e] Dropping adapter...");
    drop(devs);
    if adapter_exists(&tun_name) {
        eprintln!("  WARN: {tun_name} still visible after drop");
    } else {
        println!("  OK: {tun_name} is gone");
    }

    println!("\n=== SPIKE OK ===\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// create-teardown: T-WIN-TUN1 / T-WIN-TUN4
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_create_teardown() -> anyhow::Result<()> {
    println!("\n=== T-WIN-TUN1/TUN4: WinTun adapter create/teardown ===\n");

    let (devs, _, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("[1] Created: {tun_name}");

    if !adapter_exists(&tun_name) {
        return Err(anyhow::anyhow!("{tun_name} not found after creation"));
    }
    println!("[2] OK: visible in Get-NetAdapter");

    drop(devs);
    println!("[3] Dropped device");

    if adapter_exists(&tun_name) {
        return Err(anyhow::anyhow!("{tun_name} still visible after drop"));
    }
    println!("[4] OK: gone from Get-NetAdapter");

    println!("\n=== CREATE-TEARDOWN OK ===\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// missing-dll: T-WIN-TUN5
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_missing_dll() -> anyhow::Result<()> {
    println!("\n=== T-WIN-TUN5: missing WinTun DLL fails cleanly ===\n");

    // Bypass `create_tun`'s env-var indirection and call the crate directly
    // with an explicit bogus DLL path — avoids mutating process-wide env vars
    // (recent stable Rust made `env::set_var`/`remove_var` unsafe).
    let result = bore_wintun::WintunDevice::open_or_create(
        Some(std::path::Path::new("C:\\nonexistent\\wintun.dll")),
        "bore0",
        "bore",
        "10.255.255.1".parse()?,
        "255.255.255.252".parse()?,
        1350,
        bore_wintun::DEFAULT_RING_CAPACITY,
    );

    match result {
        Ok(_) => Err(anyhow::anyhow!(
            "open_or_create unexpectedly SUCCEEDED with a bogus DLL path"
        )),
        Err(e) => {
            println!("OK: open_or_create failed cleanly before any adapter mutation: {e}");
            println!("\n=== MISSING-DLL OK ===\n");
            Ok(())
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// route-add-del: T-WIN-HOST1
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_route_add_del() -> anyhow::Result<()> {
    println!("\n=== T-WIN-HOST1: route add/delete visible and cleaned ===\n");

    let (devs, _, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("[1] Created adapter: {tun_name}");

    let peer_routes = vec!["198.51.100.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let netcfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "route0",
        "listen",
        &tun_name,
        "10.255.255.1".parse()?,
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
    println!("[2] NetConfig applied with peer route 198.51.100.0/24");

    let (ok, stdout, _) = run(&ps(&format!(
        "Get-NetRoute -InterfaceAlias '{tun_name}' | Where-Object {{ $_.DestinationPrefix -eq '198.51.100.0/24' }}"
    )));
    if !ok || stdout.trim().is_empty() {
        eprintln!("  WARN: route not visible via Get-NetRoute (stdout empty)");
    } else {
        println!("[3] OK: route visible via Get-NetRoute");
    }

    drop(netcfg);
    drop(devs);
    println!("[4] Dropped NetConfig + adapter (route is adapter-scoped, gone with it)");

    println!("\n=== ROUTE-ADD-DEL OK ===\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// apply-revert: T-WIN-FWD2, T-WIN-NAT1 (partial)
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_apply_revert() -> anyhow::Result<()> {
    println!("\n=== T-WIN-FWD2/NAT1: NetConfig apply/revert (RAII) ===\n");

    let (devs, _, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("[1] Created adapter: {tun_name}");

    let original_fwd = read_ip_forward();
    println!("  Original IPEnableRouter: {original_fwd}");

    println!("\n[2] Applying NetConfig (forward-accept + nat-masquerade)...");
    let advertised = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let netcfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "fwdnat0",
        "listen",
        &tun_name,
        "10.255.255.1".parse()?,
        30,
        &[],
        &advertised,
        &[],
        false,
        false,
        true,
        true,
    )
    .await?;
    println!("  OK: applied");

    let fwd = read_ip_forward();
    println!("  IPEnableRouter after apply: {fwd}");
    if fwd != 1 {
        eprintln!("  WARN: expected 1, got {fwd}");
    }

    let rules = firewall_rule_count_for_link("fwdnat0", "listen");
    println!("  bore-vpn firewall rules: {rules} (expected 2: tun->lan, lan->tun)");
    if rules < 2 {
        eprintln!("  WARN: expected >=2 forward-accept rules, found {rules}");
    }

    // The WinNAT instance name is `bore-fwdnat0-listen-nat-<sanitized-subnet>`.
    let nat_name = "bore-fwdnat0-listen-nat-192_0_2_0_24";
    println!("  Checking WinNAT instance {nat_name}...");
    if nat_instance_exists(nat_name) {
        println!("  OK: WinNAT instance present");
    } else {
        eprintln!("  WARN: WinNAT instance not found by expected name (naming may differ; check Get-NetNat manually)");
    }

    println!("\n[3] Dropping NetConfig (RAII revert)...");
    drop(netcfg);

    let fwd_after = read_ip_forward();
    let rules_after = firewall_rule_count_for_link("fwdnat0", "listen");
    println!("  IPEnableRouter after revert: {fwd_after} (expected {original_fwd})");
    println!("  bore-vpn firewall rules after revert: {rules_after} (expected 0)");
    if fwd_after != original_fwd {
        eprintln!("  WARN: forwarding not restored to original value");
    }
    if rules_after != 0 {
        eprintln!("  WARN: firewall rules did not fully revert");
    }

    drop(devs);
    println!("\n=== APPLY-REVERT OK ===\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// forward-accept-off-warn: T-WIN-FWD1 (partial — the warn-only code path)
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_forward_accept_off_warn() -> anyhow::Result<()> {
    println!("\n=== T-WIN-FWD1: gateway mode without --forward-accept adds no rules ===\n");

    let (devs, _, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("[1] Created adapter: {tun_name}");

    let advertised = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let netcfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "nofwd0",
        "listen",
        &tun_name,
        "10.255.255.1".parse()?,
        30,
        &[],
        &advertised,
        &[],
        false,
        false,
        false,
        false, // forward_accept = false: the warn-only path (see vpn.rs apply())
    )
    .await?;
    println!(
        "[2] NetConfig applied WITHOUT --forward-accept (should have warned in the log above)"
    );

    let rules = firewall_rule_count_for_link("nofwd0", "listen");
    println!(
        "  bore-vpn firewall rules: {rules} (expected 0 — no rules added on the warn-only path)"
    );
    let result = if rules == 0 {
        println!("\n=== FORWARD-ACCEPT-OFF-WARN OK ===\n");
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "expected 0 firewall rules without --forward-accept, found {rules}"
        ))
    };

    drop(netcfg);
    drop(devs);
    result
}

// ═══════════════════════════════════════════════════════════════════════════════
// two-link-refcount: T-WIN-HOST2
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_two_link_refcount() -> anyhow::Result<()> {
    println!("\n=== T-WIN-HOST2: ip_forward refcount across two concurrent links ===\n");

    let original_fwd = read_ip_forward();
    println!("[0] Original IPEnableRouter: {original_fwd}");

    let (devs_a, _, tun_a) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    let (devs_b, _, tun_b) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.5".parse()?, 30, 1350, 1).await?;
    println!("[1] Created adapters: {tun_a}, {tun_b}");

    let advertised_a = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let advertised_b = vec!["203.0.113.0/24".parse::<bore_cli::shared::Ipv4Net>()?];

    let link_a = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "refA",
        "listen",
        &tun_a,
        "10.255.255.1".parse()?,
        30,
        &[],
        &advertised_a,
        &[],
        false,
        false,
        false,
        false,
    )
    .await?;
    println!(
        "[2] Applied link A (refA) — IPEnableRouter: {}",
        read_ip_forward()
    );

    let link_b = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "refB",
        "listen",
        &tun_b,
        "10.255.255.5".parse()?,
        30,
        &[],
        &advertised_b,
        &[],
        false,
        false,
        false,
        false,
    )
    .await?;
    println!(
        "[3] Applied link B (refB) — IPEnableRouter: {}",
        read_ip_forward()
    );

    if read_ip_forward() != 1 {
        return Err(anyhow::anyhow!(
            "expected IPEnableRouter=1 with two active links"
        ));
    }

    println!("\n[4] Dropping link A — link B still needs forwarding...");
    drop(link_a);
    let fwd_mid = read_ip_forward();
    println!("  IPEnableRouter: {fwd_mid} (expected 1 — B is still active)");
    if fwd_mid != 1 {
        return Err(anyhow::anyhow!(
            "dropping link A incorrectly disabled forwarding while link B is still active"
        ));
    }

    println!("\n[5] Dropping link B — last link out, should restore original...");
    drop(link_b);
    let fwd_end = read_ip_forward();
    println!("  IPEnableRouter: {fwd_end} (expected {original_fwd})");
    if fwd_end != original_fwd {
        return Err(anyhow::anyhow!(
            "expected IPEnableRouter restored to {original_fwd}, got {fwd_end}"
        ));
    }

    drop(devs_a);
    drop(devs_b);
    println!("\n=== TWO-LINK-REFCOUNT OK ===\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// leak-then-reclaim: T-WIN-HOST3, T-WIN-STALE1/STALE2 (SIGKILL simulation)
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
async fn cmd_leak() -> anyhow::Result<()> {
    println!("\n=== Leak (SIGKILL simulation) ===\n");

    let (devs, _, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("[1] Created adapter: {tun_name}");

    let advertised = vec!["192.0.2.0/24".parse::<bore_cli::shared::Ipv4Net>()?];
    let netcfg = bore_cli::vpn::hostcfg::NetConfig::apply(
        &bore_cli::vpn::hostcfg::RealRunner,
        "leak0",
        "listen",
        &tun_name,
        "10.255.255.1".parse()?,
        30,
        &[],
        &advertised,
        &[],
        false,
        false,
        true,
        true,
    )
    .await?;
    println!("[2] Applied NetConfig (forward-accept + nat-masquerade)");

    let rules = firewall_rule_count_for_link("leak0", "listen");
    println!("  bore-vpn firewall rules present: {rules}");

    println!("\n[3] Leaking (std::mem::forget) — simulating SIGKILL...");
    std::mem::forget(netcfg);
    std::mem::forget(devs);
    println!("OK: resources forgotten. Now run: target/debug/examples/windows_vpn_spike leak-then-reclaim reclaim\n");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn cmd_reclaim() -> anyhow::Result<()> {
    println!("\n=== Reclaim (SIGKILL recovery) ===\n");

    println!("[1] Running stale_reclaim for leak0/listen...");
    bore_cli::vpn::hostcfg::stale_reclaim("leak0", "listen").await;
    println!("OK: stale_reclaim completed");

    let rules = firewall_rule_count_for_link("leak0", "listen");
    println!("\n[2] bore-vpn firewall rules after reclaim: {rules} (expected 0)");
    let fwd = read_ip_forward();
    println!("[3] IPEnableRouter after reclaim: {fwd} (expected 0, no other link active)");

    if rules != 0 || fwd != 0 {
        return Err(anyhow::anyhow!(
            "stale_reclaim left state behind: rules={rules}, ip_forward={fwd}"
        ));
    }

    println!("\n=== RECLAIM OK ===\n");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// diag-firewall: raw investigation (not a pass/fail test) — isolates why
// New-NetFirewallRule with -InterfaceAlias pointing at a fresh WinTun adapter
// is unqueryable afterward, even with -ErrorAction SilentlyContinue and
// -Confirm:$false already applied to the delete side (both already tried and
// both insufficient per the windows-vpn-e2e log). Prints raw stdout/stderr
// for each probe instead of asserting anything, so the CI log itself is the
// diagnostic.
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn cmd_diag_firewall() -> anyhow::Result<()> {
    println!("\n=== DIAG: raw firewall rule investigation ===\n");

    let (devs, _, tun_name) =
        bore_cli::vpn::hostcfg::create_tun("auto", "10.255.255.1".parse()?, 30, 1350, 1).await?;
    println!("[0] Created adapter: {tun_name}\n");

    println!("[1] Get-NetAdapter full details for {tun_name}:");
    let (ok, stdout, stderr) = run(&ps(&format!(
        "Get-NetAdapter -Name '{tun_name}' | Format-List *"
    )));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[2] Get-NetConnectionProfile for {tun_name} (network classification):");
    let (ok, stdout, stderr) = run(&ps(&format!(
        "Get-NetConnectionProfile -InterfaceAlias '{tun_name}' | Format-List *"
    )));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[3] New-NetFirewallRule WITH -InterfaceAlias (raw, not via bore's runner):");
    let (ok, stdout, stderr) = run(&ps(&format!(
        "New-NetFirewallRule -DisplayName 'diag-with-iface' -Group 'bore-vpn' -Direction Inbound -Action Allow -InterfaceAlias '{tun_name}'"
    )));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[4] Immediately query it back by exact DisplayName:");
    let (ok, stdout, stderr) = run(&ps(
        "Get-NetFirewallRule -DisplayName 'diag-with-iface' | Format-List DisplayName,Group,Enabled,Profile,Direction,Action",
    ));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[5] New-NetFirewallRule WITHOUT -InterfaceAlias (isolates the variable):");
    let (ok, stdout, stderr) = run(&ps(
        "New-NetFirewallRule -DisplayName 'diag-no-iface' -Group 'bore-vpn' -Direction Inbound -Action Allow",
    ));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[6] Immediately query it back:");
    let (ok, stdout, stderr) = run(&ps(
        "Get-NetFirewallRule -DisplayName 'diag-no-iface' | Format-List DisplayName,Group,Enabled,Profile,Direction,Action",
    ));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[7] Broad query: every rule in the bore-vpn group, by ANY means:");
    let (ok, stdout, stderr) = run(&ps(
        "Get-NetFirewallRule -Group 'bore-vpn' | Format-List DisplayName,Group,Enabled,Profile,Direction,Action",
    ));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[8] Broadest query: every rule whose DisplayName starts with 'diag':");
    let (ok, stdout, stderr) = run(&ps(
        "Get-NetFirewallRule | Where-Object { $_.DisplayName -like 'diag*' } | Format-List DisplayName,Group,Enabled,Profile,Direction,Action",
    ));
    println!("  ok={ok}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n");

    println!("[9] Cleanup: delete both diag rules (best-effort, raw):");
    for name in ["diag-with-iface", "diag-no-iface"] {
        let (ok, stdout, stderr) = run(&ps(&format!(
            "Get-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue | Remove-NetFirewallRule -Confirm:$false"
        )));
        println!("  delete {name}: ok={ok} stdout={stdout:?} stderr={stderr:?}");
    }

    drop(devs);
    println!("\n=== DIAG DONE (see above — not a pass/fail check) ===\n");
    Ok(())
}
