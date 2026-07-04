#![cfg(feature = "ssh-gateway")]
//! Phase 4.3 end-to-end tests (`docs/plans/plan_SshGateway/phase_04.md`): drives a
//! real `bore_cli::server::Server` with `set_ssh_gateway` configured through the
//! real OpenSSH CLI, exercising `-R` (`tcpip-forward`) public-tunnel handling,
//! `permit=`/`max-conns=`/`notes=` enforcement, transport-only-key warnings, and
//! forward teardown on session end.
//!
//! Skips (prints a warning, passes) when `ssh`/`ssh-keygen` are not on `PATH` —
//! CI installs them in phase 7.3, mirroring `ssh_gateway_spike_test.rs`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use bore_cli::{server::Server, shared::CONTROL_PORT, sshgw::SshGatewayConfig};
use lazy_static::lazy_static;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

// A realistic >=32-char admin token (same shape as `tests/admin_test.rs`).
const TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

// ---------------------------------------------------------------------------
// Harness (mirrors `tests/ssh_gateway_spike_test.rs` for the SSH CLI side and
// `tests/admin_test.rs` for the full-`Server`/admin-HTTP side).
// ---------------------------------------------------------------------------

async fn has_ssh_cli() -> bool {
    for bin in ["ssh", "ssh-keygen"] {
        match Command::new(bin).arg("-V").output().await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => {}
        }
    }
    true
}

macro_rules! skip_without_ssh_cli {
    () => {
        if !has_ssh_cli().await {
            eprintln!(
                "WARNING: `ssh`/`ssh-keygen` not found on PATH — skipping {}",
                module_path!()
            );
            return Ok(());
        }
    };
}

/// Generates a throwaway ed25519 keypair via the real `ssh-keygen` CLI and
/// returns the private key path (the matching `<path>.pub` is created
/// alongside it).
async fn gen_keypair(dir: &Path, name: &str) -> Result<PathBuf> {
    let priv_path = dir.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&priv_path)
        .args(["-C", "gwtest"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("spawn ssh-keygen")?;
    ensure!(status.success(), "ssh-keygen exited with {status}");
    Ok(priv_path)
}

/// Writes an `authorized_keys` file in `auth_dir` granting `priv_path`'s
/// public key, with an optional leading authorized-keys options string (e.g.
/// `permit="port/9000-9010",max-conns=1`).
fn write_authorized_keys(auth_dir: &Path, priv_path: &Path, options: Option<&str>) -> Result<()> {
    let pub_line = std::fs::read_to_string(priv_path.with_extension("pub"))?;
    let pub_line = pub_line.trim();
    let line = match options {
        Some(opts) => format!("{opts} {pub_line}\n"),
        None => format!("{pub_line}\n"),
    };
    std::fs::write(auth_dir.join("authorized_keys"), line)?;
    Ok(())
}

/// Binds an ephemeral port and immediately releases it. Small accepted TOCTOU
/// race, matching the bind-then-drop idiom already used elsewhere in this
/// suite (e.g. `tests/vhost_test.rs`) to pick a free port for a listener that
/// something else will bind moments later.
async fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?.port())
}

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// A local TCP echo service standing in for the "service on localhost" that
/// every named Phase 4.3 test forwards through the gateway.
async fn spawn_echo_service() -> Result<u16> {
    let listener = TcpListener::bind("localhost:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    let n = stream.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n]).await?;
                }
                anyhow::Ok(())
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });
    Ok(port)
}

/// Issues one HTTP/1.1 GET over `stream` and returns the full response text.
async fn http_get<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    path: &str,
    token: Option<&str>,
) -> Result<String> {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Polls `GET /admin/status/data` until the response contains `needle`,
/// opening a fresh connection each attempt (the endpoint closes after one
/// reply). Unlike probing the forwarded port directly, this only touches
/// `CONTROL_PORT` — it never risks stealing the forward's own connection
/// permit (see T-SSH-PUB3: a `wait_port` probe against a `max-conns=1`
/// forward is a real proxied connection from the gateway's point of view and
/// raced the test's own first connection for the tunnel's sole permit).
async fn wait_admin_data_contains(needle: &str) -> Result<String> {
    for _ in 0..200 {
        let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
        let resp = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
        if resp.contains(needle) {
            return Ok(resp);
        }
        time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("admin data never contained {needle:?} within timeout")
}

/// Round-trips `payload` through a plain TCP connection to `port` and reports
/// whether the bytes came back unchanged — proof the forward actually carries
/// traffic end to end (real OpenSSH client -> gateway -> local service).
async fn roundtrip(port: u16, payload: &[u8]) -> Result<bool> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream.write_all(payload).await?;
    let mut buf = vec![0u8; payload.len()];
    time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf)).await??;
    Ok(buf == payload)
}

/// Starts a full `Server` with the ssh-gateway enabled against `host_key_file`
/// and `authorized_keys_dir`, admin API enabled with [`TOKEN`]. Returns the
/// gateway's dedicated listening port.
async fn start_gateway_server(host_key_file: PathBuf, authorized_keys_dir: PathBuf) -> Result<u16> {
    let gw_port = free_port().await?;
    let config = SshGatewayConfig {
        port: Some(gw_port),
        host_key_file,
        authorized_keys_dir: Some(authorized_keys_dir),
        passwords_file: None,
        banner: None,
    };
    let mut server = Server::new(1024..=65535, None);
    server.set_admin_token(Some(TOKEN.to_string()));
    server.set_ssh_gateway(config)?;
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;
    wait_port(gw_port, true).await;
    Ok(gw_port)
}

/// Builds `ssh` CLI args connecting to the gateway on `gw_port` as `identity`,
/// with one `-R <bind_port>:127.0.0.1:<local_port>` per entry in `forwards`,
/// and an optional trailing remote command (mutually exclusive with `-N`,
/// which is added automatically when `command` is `None`).
fn ssh_args(
    gw_port: u16,
    identity: &Path,
    forwards: &[(u16, u16)],
    command: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "GlobalKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        "-o".into(),
        "PreferredAuthentications=publickey".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-i".into(),
        identity.display().to_string(),
        "-p".into(),
        gw_port.to_string(),
    ];
    if command.is_none() {
        args.push("-N".into());
    }
    for (bind_port, local_port) in forwards {
        args.push("-R".into());
        args.push(format!("{bind_port}:127.0.0.1:{local_port}"));
    }
    args.push("gwtest@127.0.0.1".into());
    if let Some(cmd) = command {
        args.push(cmd.into());
    }
    args
}

// ---------------------------------------------------------------------------
// T-SSH-PUB1 — fixed-port `-R`: roundtrip + admin API shows an ssh-transport
// Public entry.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_pub1_fixed_port_forward_and_admin_entry() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;
    let svc_port = spawn_echo_service().await?;

    let fwd_port = 19005u16;
    let args = ssh_args(gw_port, &client_priv, &[(fwd_port, svc_port)], None);
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh -R (T-SSH-PUB1)")?;

    wait_port(fwd_port, true).await;
    assert!(
        roundtrip(fwd_port, b"ping-pub1").await?,
        "forwarded connection did not echo the request"
    );

    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let resp = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(
        resp.contains("\"transport\":\"ssh\""),
        "admin data missing ssh transport: {resp}"
    );
    assert!(
        resp.contains(&format!("\"public_port\":{fwd_port}")),
        "admin data missing public_port {fwd_port}: {resp}"
    );

    child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-PUB2 — `-R 0:...`: parse the "Allocated port" ssh announces, roundtrip
// against it.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_pub2_auto_assigned_port_forward() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;
    let svc_port = spawn_echo_service().await?;

    let args = ssh_args(gw_port, &client_priv, &[(0, svc_port)], None);
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh -R 0:... (T-SSH-PUB2)")?;

    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut collected = String::new();
    let allocated_port: u16 = time::timeout(Duration::from_secs(10), async {
        loop {
            let mut buf = [0u8; 256];
            let n = stderr.read(&mut buf).await?;
            ensure!(n > 0, "ssh exited before announcing an allocated port");
            collected.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(port) = collected
                .lines()
                .find_map(|line| line.split("Allocated port ").nth(1))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|tok| tok.parse::<u16>().ok())
            {
                return anyhow::Ok(port);
            }
        }
    })
    .await
    .context("timed out waiting for \"Allocated port\"")??;

    wait_port(allocated_port, true).await;
    assert!(
        roundtrip(allocated_port, b"ping-pub2").await?,
        "auto-assigned forwarded connection did not echo the request"
    );

    child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-PUB3 — exec `notes=... max-conns=1`: admin JSON carries the note, a
// second concurrent connection is refused while the first is held, and a
// third succeeds once the first releases the permit.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_pub3_notes_and_max_conns_enforced() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;
    let svc_port = spawn_echo_service().await?;

    let fwd_port = 19006u16;
    let args = ssh_args(
        gw_port,
        &client_priv,
        &[(fwd_port, svc_port)],
        Some("notes=itest max-conns=1"),
    );
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh with exec params (T-SSH-PUB3)")?;

    // Wait for the forward to be registered via the admin API rather than
    // probing `fwd_port` directly: a bare connect-and-drop probe against a
    // `max-conns=1` forward IS a real proxied connection to the gateway and
    // would race the "first" connection below for the tunnel's sole permit.
    wait_admin_data_contains("\"notes\":\"itest\"").await?;

    // First connection holds the tunnel's sole permit: prove it is actually
    // live through the tunnel before racing a second connection against it.
    let mut first = TcpStream::connect(("127.0.0.1", fwd_port)).await?;
    first.write_all(b"first").await?;
    let mut buf = [0u8; 5];
    time::timeout(Duration::from_secs(5), first.read_exact(&mut buf)).await??;
    assert_eq!(&buf, b"first");

    // A second concurrent connection is accepted at the TCP level, then
    // dropped immediately by the gateway because the tunnel's permit
    // (max-conns=1) is exhausted — never echoed.
    let mut second = TcpStream::connect(("127.0.0.1", fwd_port)).await?;
    let _ = second.write_all(b"second").await;
    let mut sbuf = [0u8; 8];
    match time::timeout(Duration::from_secs(5), second.read(&mut sbuf)).await {
        Ok(Ok(0)) => {} // clean close — expected refusal
        Ok(Ok(n)) => panic!("second connection should be refused at max-conns=1, got {n} bytes"),
        Ok(Err(_)) => {} // reset — also an acceptable refusal signal
        Err(_) => panic!("second connection was neither echoed nor closed within 5s"),
    }

    // Releasing the first connection frees the permit for a new one.
    drop(first);
    let mut freed = false;
    for _ in 0..40 {
        if matches!(roundtrip(fwd_port, b"third").await, Ok(true)) {
            freed = true;
            break;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        freed,
        "third connection should succeed once the permit is freed"
    );

    child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-WARN1 — exec `udp=on`: a transport-only key produces a warning on the
// channel rather than being silently accepted or dropped (I-2).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_warn1_transport_only_key_warns() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;
    let svc_port = spawn_echo_service().await?;

    let fwd_port = 19012u16;
    let args = ssh_args(
        gw_port,
        &client_priv,
        &[(fwd_port, svc_port)],
        Some("udp=on"),
    );
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh with exec udp=on (T-SSH-WARN1)")?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut collected = String::new();
    time::timeout(Duration::from_secs(10), async {
        let mut buf = [0u8; 256];
        loop {
            let n = stdout.read(&mut buf).await?;
            ensure!(n > 0, "ssh exited before sending the warning");
            collected.push_str(&String::from_utf8_lossy(&buf[..n]));
            if collected.contains("not available via SSH") {
                return anyhow::Ok(());
            }
        }
    })
    .await
    .context("timed out waiting for the transport-only-key warning")??;

    child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-CANCEL1 — closing the whole SSH session tears down every outstanding
// forward (`ConnState::Drop`) within 2s.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_cancel1_session_close_frees_forwards() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;
    let svc_a = spawn_echo_service().await?;
    let svc_b = spawn_echo_service().await?;

    let fwd_a = 19010u16;
    let fwd_b = 19011u16;
    let args = ssh_args(
        gw_port,
        &client_priv,
        &[(fwd_a, svc_a), (fwd_b, svc_b)],
        None,
    );
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh with two -R forwards (T-SSH-CANCEL1)")?;

    wait_port(fwd_a, true).await;
    wait_port(fwd_b, true).await;
    assert!(
        roundtrip(fwd_a, b"ping-a").await?,
        "forward A not live before teardown"
    );
    assert!(
        roundtrip(fwd_b, b"ping-b").await?,
        "forward B not live before teardown"
    );

    // Closing the whole SSH session must tear down both forwards' listeners.
    child.kill().await.ok();

    let freed = time::timeout(Duration::from_secs(2), async {
        loop {
            let a_gone = TcpStream::connect(("127.0.0.1", fwd_a)).await.is_err();
            let b_gone = TcpStream::connect(("127.0.0.1", fwd_b)).await.is_err();
            if a_gone && b_gone {
                return;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        freed.is_ok(),
        "both forward listeners must be freed within 2s of session close"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-PREAUTH1 — a connection that never completes the SSH handshake is
// disconnected unilaterally by russh's `inactivity_timeout` (`SSH_PREAUTH_GRACE`,
// set in `SshGateway::russh_config`), not left to hang forever. Pure raw-socket
// probe: no real `ssh` client involved, so no `skip_without_ssh_cli!()`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_preauth1_stalled_handshake_is_disconnected() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;

    let mut stream = TcpStream::connect(("127.0.0.1", gw_port)).await?;
    stream
        .write_all(b"SSH-2.0-t-ssh-preauth1-probe\r\n")
        .await?;

    // Never send another byte, never complete key exchange. The gateway's
    // own version banner may arrive first; keep draining until it closes
    // the socket (or errors) rather than expecting an immediate EOF.
    let closed = time::timeout(Duration::from_secs(35), async {
        let mut buf = [0u8; 64];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => return,
                Ok(_) => continue,
                Err(_) => return,
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "gateway did not disconnect a stalled pre-auth handshake within SSH_PREAUTH_GRACE + margin"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-KEEP1 — an idle, authenticated `-N -R` session with zero tunnel
// traffic survives well past `SSH_CTRL_TIMEOUT` (60s) on `ServerAliveInterval`
// keepalives alone, and the tunnel still relays afterwards (I-3: the reaper
// must not fire on a healthy connection — see `SshGateway::russh_config`'s
// doc for why this is russh's own `keepalive_max`, not a callback-driven
// tracker).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_keep1_idle_session_survives_ctrl_timeout() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;
    let svc_port = spawn_echo_service().await?;

    let fwd_port = 19012u16;
    let mut args = vec![
        "-o".into(),
        "ServerAliveInterval=15".to_string(),
        "-o".into(),
        "ServerAliveCountMax=6".to_string(),
    ];
    args.extend(ssh_args(
        gw_port,
        &client_priv,
        &[(fwd_port, svc_port)],
        None,
    ));
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh -R with ServerAlive keepalives (T-SSH-KEEP1)")?;

    wait_port(fwd_port, true).await;
    assert!(
        roundtrip(fwd_port, b"ping-keep1-before").await?,
        "forward not live before the idle window"
    );

    // Zero tunnel traffic for well past SSH_CTRL_TIMEOUT (60s): only the
    // client's own ServerAliveInterval keepalives (invisible to any
    // Handler callback) keep the connection alive on russh's side.
    time::sleep(Duration::from_secs(90)).await;

    assert!(
        roundtrip(fwd_port, b"ping-keep1-after").await?,
        "idle session was reaped despite healthy keepalives"
    );

    child.kill().await.ok();
    Ok(())
}
