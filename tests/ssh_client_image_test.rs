//! The `bore-ssh-client` image (`docker/ssh-entrypoint.sh`) is how an OpenSSH
//! tunnel is deployed unattended, so its DEFAULT keepalive options are the ones
//! that decide how long that mode stays down after an outage (plan 005, O-1).
//!
//! The entrypoint is run for real with an `autossh` shim on `PATH` that prints
//! the command line it was given, so the test reads exactly what the container
//! would exec — no parsing of the script's source.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use bore_cli::liveness::TRANSPORT_REAP_FLOOR;

/// Linux's retransmission of a segment lost in a flick lands at most 12.6 s
/// after the first loss (RTO 200 ms doubling: 0.2, 0.6, 1.4, 3, 6.2, 12.6 s).
const RETRANSMIT_LADDER: Duration = Duration::from_millis(12_600);

fn run_entrypoint(dir: &Path, extra_env: &[(&str, &str)]) -> String {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("autossh");
    std::fs::write(
        &shim,
        "#!/bin/sh\necho \"AUTOSSH_GATETIME=$AUTOSSH_GATETIME\"\nfor a in \"$@\"; do echo \"ARG $a\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let key = dir.join("id_key");
    std::fs::write(&key, "not-a-real-key\n").unwrap();

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("docker/ssh-entrypoint.sh");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .env_clear()
        .env("PATH", path)
        .env("BORE_SSH_HOST", "bore.example.test")
        .env("TUNNEL_MODE", "vhost")
        .env("VHOST_LABEL", "app")
        .env("LOCAL_PORT", "8080")
        .env("SSH_KEY_FILE", &key)
        .env("KNOWN_HOSTS_FILE", dir.join("absent_known_hosts"))
        .env("BORE_SSH_RUNTIME_DIR", dir.join("runtime"));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("bash runs the entrypoint");
    assert!(
        out.status.success(),
        "entrypoint failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn option(out: &str, name: &str) -> Option<u64> {
    let prefix = format!("ARG {name}=");
    out.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.parse().expect("numeric option"))
}

/// The image's defaults drop a dead server in about the time the gateway takes
/// to free its name, and a probe lost in a flick still reaches the gateway
/// before its deadline. RED-CHECK: the old `15 x 3` defaults fail both bounds.
#[test]
fn image_keepalive_defaults_recover_fast_and_survive_a_flick() {
    if Command::new("bash").arg("--version").output().is_err() {
        eprintln!("SKIP: bash not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = run_entrypoint(dir.path(), &[]);

    let interval = option(&out, "ServerAliveInterval").expect("ServerAliveInterval set");
    let count = option(&out, "ServerAliveCountMax").expect("ServerAliveCountMax set");
    assert_eq!((interval, count), (2, 7), "shipped defaults: {out}");

    // Recovery: the client gives up on a dead server no later than the
    // gateway's reaper releases the name it held, so autossh's reconnect is
    // neither slow nor refused as "in use".
    let gives_up = Duration::from_secs(interval * count);
    assert!(
        gives_up <= TRANSPORT_REAP_FLOOR,
        "client detects a dead server after {gives_up:?}, gateway reaps after {TRANSPORT_REAP_FLOOR:?}"
    );
    // Stability: after a flick the client's next probe, retransmitted on the
    // kernel's ladder, still reaches the gateway inside its 15 s deadline.
    let probe_lands = Duration::from_secs(interval) + RETRANSMIT_LADDER;
    assert!(
        probe_lands < TRANSPORT_REAP_FLOOR,
        "a probe lost in a flick lands after {probe_lands:?}, past the {TRANSPORT_REAP_FLOOR:?} deadline"
    );

    // autossh restarts at once, forever; -M 0 relies on the ssh keepalives above.
    assert!(out.contains("AUTOSSH_GATETIME=0"), "{out}");
    assert!(out.contains("ARG -M\nARG 0\n"), "{out}");
    assert!(out.contains("ARG ExitOnForwardFailure=yes"), "{out}");
}

/// An operator's explicit value still wins over the default.
#[test]
fn image_keepalive_defaults_can_be_overridden() {
    if Command::new("bash").arg("--version").output().is_err() {
        eprintln!("SKIP: bash not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = run_entrypoint(
        dir.path(),
        &[
            ("SERVER_ALIVE_INTERVAL", "3"),
            ("SERVER_ALIVE_COUNT_MAX", "4"),
        ],
    );
    assert_eq!(option(&out, "ServerAliveInterval"), Some(3), "{out}");
    assert_eq!(option(&out, "ServerAliveCountMax"), Some(4), "{out}");
}
