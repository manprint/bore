#![cfg(feature = "ssh-gateway")]
//! Phase 4.3 end-to-end tests (`docs/plans/plan_SshGateway/phase_04.md`): drives a
//! real `bore_cli::server::Server` with `set_ssh_gateway` configured through the
//! real OpenSSH CLI, exercising `-R` (`tcpip-forward`) public-tunnel handling,
//! `permit=`/`max-conns=`/`notes=` enforcement, transport-only-key warnings, and
//! forward teardown on session end.
//!
//! Skips (prints a warning, passes) when `ssh`/`ssh-keygen` are not on `PATH` —
//! CI installs them in phase 7.3, mirroring `ssh_gateway_spike_test.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use bore_cli::vhost::{Reservation, VhostConfig, VhostModeCfg};
use bore_cli::{
    client::{Client, ProviderMeta},
    secret::Proxy,
    server::Server,
    shared::CONTROL_PORT,
    sshgw::SshGatewayConfig,
    transport,
};
use lazy_static::lazy_static;
use rcgen::generate_simple_self_signed;
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

/// For T-SSH-TLS1 (SSH-over-TLS via an `openssl s_client` `ProxyCommand`).
async fn has_openssl() -> bool {
    Command::new("openssl")
        .arg("version")
        .output()
        .await
        .is_ok()
}

macro_rules! skip_without_openssl {
    () => {
        if !has_openssl().await {
            eprintln!(
                "WARNING: `openssl` not found on PATH — skipping {}",
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
    gen_keypair_with_comment(dir, name, "gwtest").await
}

/// Like [`gen_keypair`], but with a caller-chosen pubkey comment — the
/// comment becomes the key's [`KeyGrant::identity`] (`crate::sshgw_auth`),
/// which `resolve_route`'s reservation `client_id` check compares against.
async fn gen_keypair_with_comment(dir: &Path, name: &str, comment: &str) -> Result<PathBuf> {
    let priv_path = dir.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&priv_path)
        .args(["-C", comment])
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
    write_authorized_keys_named(auth_dir, "authorized_keys", priv_path, options)
}

/// Like [`write_authorized_keys`], but under a caller-chosen filename —
/// `KeyStore` (`src/sshgw_auth.rs`) scans every extensionless/`.pub` file in
/// `auth_dir`, so granting a SECOND, distinct identity alongside an existing
/// one (e.g. for a same-identity-takeover test where a third session must
/// use a genuinely different key) just needs its own file.
fn write_authorized_keys_named(
    auth_dir: &Path,
    filename: &str,
    priv_path: &Path,
    options: Option<&str>,
) -> Result<()> {
    let pub_line = std::fs::read_to_string(priv_path.with_extension("pub"))?;
    let pub_line = pub_line.trim();
    let line = match options {
        Some(opts) => format!("{opts} {pub_line}\n"),
        None => format!("{pub_line}\n"),
    };
    std::fs::write(auth_dir.join(filename), line)?;
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

// 20s cap (was 5s): a real `ssh`/subprocess-backed check needs headroom for
// CI scheduling contention, not just localhost network latency — see
// PARAMS_GRACE's history in `src/sshgw.rs` for the same class of bug.
async fn wait_port(port: u16, listening: bool) {
    for _ in 0..2000 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// A local TCP echo service standing in for the "service on localhost" that
/// every named Phase 4.3 test forwards through the gateway. Binds the literal
/// IPv4 loopback, not the `"localhost"` hostname: every `-R` spec in this
/// suite targets `127.0.0.1` literally (`ssh_args`), and on a host where
/// `"localhost"` resolves IPv6-first/-only (observed on GH Actions' hosted
/// runners, not reproducible on every dev machine) `TcpListener::bind` would
/// bind `[::1]` only — the real `ssh` client's own local connect attempt to
/// `127.0.0.1:<port>` then gets a genuine `ECONNREFUSED`, which surfaces as
/// `russh::Error::ChannelOpenFailure` on the server and (pre-hardening, see
/// `run_public_forward` in `src/sshgw.rs`) tore down the whole forward. This
/// was the actual root cause of every CI failure in this suite, not a race.
async fn spawn_echo_service() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
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
    time::timeout(Duration::from_secs(20), stream.read_to_end(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Polls `GET /admin/status/data` until the response contains `needle`,
/// opening a fresh connection each attempt (the endpoint closes after one
/// reply). Unlike probing the forwarded port directly, this only touches
/// `CONTROL_PORT` — it never risks stealing the forward's own connection
/// permit (see T-SSH-PUB3: a `wait_port` probe against a `max-conns=1`
/// forward is a real proxied connection from the gateway's point of view and
/// raced the test's own first connection for the tunnel's sole permit).
// 20s cap (was 5s) — same CI-scheduling-headroom reasoning as `wait_port`.
async fn wait_admin_data_contains(needle: &str) -> Result<String> {
    for _ in 0..800 {
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
    time::timeout(Duration::from_secs(20), stream.read_exact(&mut buf)).await??;
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
        window_size: bore_cli::sshgw::SSH_DEFAULT_WINDOW_SIZE,
    };
    let mut server = Server::new(1024..=65535, None);
    server.set_admin_token(Some(TOKEN.to_string()));
    server.set_ssh_gateway(config)?;
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;
    wait_port(gw_port, true).await;
    Ok(gw_port)
}

/// Minimal HTTP-only [`VhostConfig`] for `base_domain` on `http_port`, with
/// the given `reservations` (empty = unrestricted).
fn vhost_config(base_domain: &str, http_port: u16, reservations: Vec<Reservation>) -> VhostConfig {
    VhostConfig {
        base_domain: base_domain.to_string(),
        mode: VhostModeCfg::Http,
        http_port,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers: BTreeMap::new(),
        default_response_headers: BTreeMap::new(),
        reservations,
    }
}

/// Like [`start_gateway_server`], but also wires vhost (`vhost/<label>`
/// forwards route through the vhost HTTP frontend on `cfg.http_port`).
/// `set_vhost` must run before `set_ssh_gateway` — the gateway snapshots
/// `Server::vhost_config` at that call (see `src/sshgw.rs`
/// `tcpip_forward_vhost`).
async fn start_gateway_server_vhost(
    host_key_file: PathBuf,
    authorized_keys_dir: PathBuf,
    cfg: VhostConfig,
) -> Result<u16> {
    let http_port = cfg.http_port;
    let gw_port = free_port().await?;
    let config = SshGatewayConfig {
        port: Some(gw_port),
        host_key_file,
        authorized_keys_dir: Some(authorized_keys_dir),
        passwords_file: None,
        banner: None,
        window_size: bore_cli::sshgw::SSH_DEFAULT_WINDOW_SIZE,
    };
    let mut server = Server::new(1024..=65535, None);
    server.set_admin_token(Some(TOKEN.to_string()));
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    server.set_ssh_gateway(config)?;
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;
    wait_port(gw_port, true).await;
    wait_port(http_port, true).await;
    Ok(gw_port)
}

/// Generates a throwaway self-signed cert/key pair for "localhost" (PEM),
/// same pattern as `tests/tls_test.rs`.
fn self_signed_cert() -> Result<(String, String)> {
    let key = generate_simple_self_signed(["localhost".to_string()])?;
    Ok((key.cert.pem(), key.signing_key.serialize_pem()))
}

/// Starts a full `Server` on [`CONTROL_PORT`] with the ssh-gateway enabled
/// WITHOUT a dedicated `--ssh-port` (demux only, Phase 6, D8) AND TLS
/// configured on that same control port — the scenario T-SSH-DMX1/DMX2/TLS1
/// exercise: one port serving SSH, TLS, HTTP (admin) and native bore
/// concurrently.
async fn start_demux_server(host_key_file: PathBuf, authorized_keys_dir: PathBuf) -> Result<()> {
    let (cert_pem, key_pem) = self_signed_cert()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let config = SshGatewayConfig {
        port: None,
        host_key_file,
        authorized_keys_dir: Some(authorized_keys_dir),
        passwords_file: None,
        banner: None,
        window_size: bore_cli::sshgw::SSH_DEFAULT_WINDOW_SIZE,
    };
    let mut server = Server::new(1024..=65535, None);
    server.set_tls(acceptor);
    server.set_admin_token(Some(TOKEN.to_string()));
    server.set_ssh_gateway(config)?;
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;
    Ok(())
}

/// Like [`start_gateway_server`], but the server also has a TLS certificate
/// configured (`set_tls`, called before `set_ssh_gateway` per `main.rs`'s
/// real ordering — `SshGateway` snapshots `Server::tls` at that call). Used
/// by T-SSH-PUB4/PUB5 to exercise `https=on`/`force-https=on` termination on
/// a PUBLIC tunnel's own port (`edge::accept`, §5.6 of `README-SSH-GATEWAY.md`).
async fn start_gateway_server_tls(
    host_key_file: PathBuf,
    authorized_keys_dir: PathBuf,
) -> Result<u16> {
    let (cert_pem, key_pem) = self_signed_cert()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let gw_port = free_port().await?;
    let config = SshGatewayConfig {
        port: Some(gw_port),
        host_key_file,
        authorized_keys_dir: Some(authorized_keys_dir),
        passwords_file: None,
        banner: None,
        window_size: bore_cli::sshgw::SSH_DEFAULT_WINDOW_SIZE,
    };
    let mut server = Server::new(1024..=65535, None);
    server.set_tls(acceptor);
    server.set_admin_token(Some(TOKEN.to_string()));
    server.set_ssh_gateway(config)?;
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;
    wait_port(gw_port, true).await;
    Ok(gw_port)
}

/// Round-trips `payload` through `openssl s_client` (TLS client, self-signed
/// cert accepted via `-verify_quiet`) against `port` and returns whether the
/// decrypted echo matches. Proves the server actually terminates TLS on that
/// port (a plain-TCP client would just see ciphertext, never a valid echo).
async fn tls_roundtrip(port: u16, payload: &[u8]) -> Result<bool> {
    let mut child = Command::new("openssl")
        .args([
            "s_client",
            "-quiet",
            "-verify_quiet",
            "-connect",
            &format!("127.0.0.1:{port}"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn openssl s_client")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    stdin.write_all(payload).await?;
    stdin.flush().await?;
    let mut buf = vec![0u8; payload.len()];
    let matched = time::timeout(Duration::from_secs(20), stdout.read_exact(&mut buf))
        .await
        .is_ok()
        && buf == payload;
    drop(stdin);
    child.kill().await.ok();
    Ok(matched)
}

/// Builds `ssh` CLI args that reach the gateway through an `openssl
/// s_client` TLS tunnel (`ProxyCommand`) instead of a direct TCP connect —
/// SSH-over-TLS (T-SSH-TLS1, D4). `-verify_quiet` accepts the self-signed
/// cert without a trust store, matching how a real deployment would use a
/// CA-issued one instead.
fn ssh_proxycommand_args(
    identity: &Path,
    forwards: &[String],
    command: Option<&str>,
) -> Vec<String> {
    let proxy = format!("openssl s_client -quiet -verify_quiet -connect 127.0.0.1:{CONTROL_PORT}");
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
        "ConnectTimeout=30".into(),
        "-o".into(),
        format!("ProxyCommand={proxy}"),
        "-i".into(),
        identity.display().to_string(),
    ];
    if command.is_none() {
        args.push("-N".into());
    }
    for f in forwards {
        args.push("-R".into());
        args.push(f.clone());
    }
    args.push("gwtest@127.0.0.1".into());
    if let Some(cmd) = command {
        args.push(cmd.into());
    }
    args
}

/// A local HTTP service standing in for the "service on localhost" that a
/// vhost forward proxies to: replies to every request with a fixed 200 body.
async fn spawn_http_stub(body: &'static str) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await?;
            let mut buf = [0u8; 4096];
            let mut total = 0;
            loop {
                let n = conn.read(&mut buf[total..]).await?;
                total += n;
                if n == 0 || buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(resp.as_bytes()).await?;
            conn.shutdown().await?;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });
    Ok(port)
}

/// Issues one HTTP/1.1 GET against `port` with the given `Host` header and
/// returns the full raw response text.
async fn send_http(port: u16, host: &str, path: &str) -> Result<String> {
    send_http_auth(port, host, path, None).await
}

/// Like [`send_http`] but with an optional extra header line (e.g.
/// `"Authorization: Basic ..."`) appended before the terminating blank line.
async fn send_http_auth(
    port: u16,
    host: &str,
    path: &str,
    extra_header: Option<&str>,
) -> Result<String> {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).await?;
    let extra = extra_header.map(|h| format!("{h}\r\n")).unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra}\r\n");
    conn.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    time::timeout(Duration::from_secs(20), conn.read_to_end(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Builds `ssh` CLI args connecting to the gateway on `gw_port` as `identity`,
/// with one `-R <bind_port>:127.0.0.1:<local_port>` per entry in `forwards`,
/// and an optional trailing remote command (mutually exclusive with `-N`,
/// which is added automatically when `command` is `None`).
fn ssh_base_args(gw_port: u16, identity: &Path) -> Vec<String> {
    vec![
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
        // Covers TCP connect AND the full SSH handshake/KEX (ssh_config(5)),
        // not just the TCP connect syscall. 5s was tuned on a fast dev
        // machine and the real `ssh` CLI's handshake reliably takes longer
        // than that on a loaded CI runner — the client then self-aborts,
        // which the server sees as "Connection reset by peer" (closing a
        // socket with unread protocol bytes still buffered sends an RST).
        // This is the actual root cause behind CI's long-standing
        // "Connection reset by peer" failures (`t_ssh_cancel1`/`pub1`/`pub2`/
        // etc.) that raising `PARAMS_GRACE` alone did not fix.
        "-o".into(),
        "ConnectTimeout=30".into(),
        "-i".into(),
        identity.display().to_string(),
        "-p".into(),
        gw_port.to_string(),
    ]
}

fn ssh_args(
    gw_port: u16,
    identity: &Path,
    forwards: &[(u16, u16)],
    command: Option<&str>,
) -> Vec<String> {
    let mut args = ssh_base_args(gw_port, identity);
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

/// Like [`ssh_args`], but each forward is a raw `-R` tail string (e.g.
/// `"vhost/pfx:0:127.0.0.1:9000"`), for the `vhost/`/`secret/` prefixed
/// bind-address forms that don't fit the `(bind_port, local_port)` shape.
fn ssh_args_raw(
    gw_port: u16,
    identity: &Path,
    raw_forwards: &[String],
    command: Option<&str>,
) -> Vec<String> {
    let mut args = ssh_base_args(gw_port, identity);
    if command.is_none() {
        args.push("-N".into());
    }
    for f in raw_forwards {
        args.push("-R".into());
        args.push(f.clone());
    }
    args.push("gwtest@127.0.0.1".into());
    if let Some(cmd) = command {
        args.push(cmd.into());
    }
    args
}

/// Builds `ssh -N -L <bind_port>:<dest>` args for a secret-consumer forward
/// (e.g. `dest = "tcp-id:1"` or `"secret/tcp-id:1"` — the destination port is
/// an ignored placeholder; it must be nonzero because OpenSSH's `-L` parser
/// rejects a literal `0` outright, unlike `-R`), which the gateway dispatches
/// as a `direct-tcpip` channel open (`channel_open_direct_tcpip`, Phase 5.3)
/// rather than a `tcpip-forward` request.
fn ssh_local_forward_args(
    gw_port: u16,
    identity: &Path,
    bind_port: u16,
    dest: &str,
) -> Vec<String> {
    let mut args = ssh_base_args(gw_port, identity);
    args.push("-N".into());
    args.push("-L".into());
    args.push(format!("{bind_port}:{dest}"));
    args.push("gwtest@127.0.0.1".into());
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
    let allocated_port: u16 = time::timeout(Duration::from_secs(20), async {
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
    time::timeout(Duration::from_secs(20), first.read_exact(&mut buf)).await??;
    assert_eq!(&buf, b"first");

    // A second concurrent connection is accepted at the TCP level, then
    // dropped immediately by the gateway because the tunnel's permit
    // (max-conns=1) is exhausted — never echoed.
    let mut second = TcpStream::connect(("127.0.0.1", fwd_port)).await?;
    let _ = second.write_all(b"second").await;
    let mut sbuf = [0u8; 8];
    match time::timeout(Duration::from_secs(20), second.read(&mut sbuf)).await {
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
// T-SSH-PUB4 — exec `https=on`: the SSH gateway terminates TLS on a PUBLIC
// tunnel's own port using the server's configured certificate, exactly like
// the native `bore local --https` client flag (`edge::accept`, shared
// code). A non-TLS raw client is still forwarded as plain (additive, not
// exclusive — see T-SSH-PUB5 for the `force-https=on` redirect case).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_pub4_https_terminates_tls() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    skip_without_openssl!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server_tls(dir.path().join("host_key"), auth_dir).await?;
    let svc_port = spawn_echo_service().await?;

    let fwd_port = 19007u16;
    let args = ssh_args(
        gw_port,
        &client_priv,
        &[(fwd_port, svc_port)],
        Some("https=on"),
    );
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh -R with https=on (T-SSH-PUB4)")?;

    wait_admin_data_contains(&format!("\"public_port\":{fwd_port}")).await?;

    // A real TLS client terminates against the server's cert and gets the
    // decrypted echo from the local service behind the tunnel — `https=on`
    // works exactly like the native `bore local --https` flag.
    assert!(
        tls_roundtrip(fwd_port, b"ping-https").await?,
        "https=on tunnel must terminate TLS and forward the decrypted payload"
    );

    // A raw (non-TLS, non-HTTP) TCP client still gets forwarded as plain —
    // `edge::accept` is additive, not TLS-exclusive (`src/edge.rs`'s own doc:
    // "so raw clients keep working"), same as the native client's `--https`.
    assert!(
        roundtrip(fwd_port, b"ping-plain").await?,
        "https=on tunnel must still forward a non-TLS raw TCP client as plain"
    );

    child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-PUB5 — exec `https=on force-https=on`: a plain HTTP request on the
// tunnel port gets redirected to `https://` instead of forwarded raw.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_pub5_force_https_redirects_plain_http() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server_tls(dir.path().join("host_key"), auth_dir).await?;
    let svc_port = spawn_http_stub("should never be reached").await?;

    let fwd_port = 19008u16;
    let args = ssh_args(
        gw_port,
        &client_priv,
        &[(fwd_port, svc_port)],
        Some("https=on force-https=on"),
    );
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh -R with https=on force-https=on (T-SSH-PUB5)")?;

    let admin_json = wait_admin_data_contains(&format!("\"public_port\":{fwd_port}")).await?;

    // Regression guard: the admin dashboard's "Tunnels" section must reflect
    // `https=on`/`force-https=on` for an SSH-originated public tunnel same
    // as it does for a native client's `--https`/`--force-https` — this was
    // never actually checked by a test before (only the TLS *behavior* was),
    // so a wiring gap between `parse_params`/`NewEntry` and the admin JSON
    // (e.g. from a future refactor) could regress silently.
    assert!(
        admin_json.contains("\"https\":true"),
        "admin entry must report https:true for this https=on SSH tunnel, got: {admin_json}"
    );
    assert!(
        admin_json.contains("\"force_https\":true"),
        "admin entry must report force_https:true for this force-https=on SSH tunnel, got: {admin_json}"
    );

    let stream = TcpStream::connect(("127.0.0.1", fwd_port)).await?;
    let resp = http_get(stream, "/", None).await?;
    assert!(
        resp.starts_with("HTTP/1.1 308"),
        "plain HTTP on a force-https=on tunnel must get a 308 redirect, got: {resp}"
    );
    assert!(
        resp.contains("Location: https://"),
        "308 redirect must point at https://, got: {resp}"
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
    time::timeout(Duration::from_secs(20), async {
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
// T-SSH-PUB6 — an ungraceful disconnect (SIGKILL, no `cancel-tcpip-forward`
// ever sent) on a PUBLIC `-R <port>:...` forward must free the bound
// listener/admin row, just like T-SSH-CANCEL1 already proves for the
// underlying session-close path in general. This is the *same*
// `Arc<ConnState>` reference-cycle bug class that 523fa32 fixed for the
// vhost/secret finalize tasks (`drop(state)` before their `pending()` tail)
// — missed for the public path because its tail is `run_public_forward`'s
// real accept loop, not a bare `pending()`, so a grep for the sibling
// pattern didn't surface it. Symptom in the wild: `Ctrl+C` the ssh client,
// reconnect with the same `-R <port>` a moment later, get "remote port
// forwarding failed" forever.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_pub6_zombie_port_on_ungraceful_disconnect() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir)?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(&auth_dir, &client_priv, None)?;

    let gw_port = start_gateway_server(dir.path().join("host_key"), auth_dir).await?;
    let svc = spawn_echo_service().await?;
    let fwd_port = free_port().await?;

    let args_a = ssh_args(gw_port, &client_priv, &[(fwd_port, svc)], None);
    let mut child_a = Command::new("ssh")
        .args(&args_a)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn first ssh -R holding the port (T-SSH-PUB6)")?;

    wait_port(fwd_port, true).await;
    assert!(
        roundtrip(fwd_port, b"ping-before-kill").await?,
        "forward not live before the ungraceful kill"
    );

    // SIGKILL, not a graceful client exit: no `cancel-tcpip-forward` global
    // request ever reaches the gateway — matches Ctrl+C/a crashed client.
    child_a.kill().await.ok();
    let _ = child_a.wait().await;

    // Deliberately no probe against `fwd_port` between the kill and the
    // reconnect attempts below: a `TcpStream::connect` reaching the dead
    // listener's `accept()` would itself trigger `run_public_forward`'s
    // "channel-open failed, session is gone, return" exit path and mask the
    // exact bug under test — the *idle* zombie listener that just sits there
    // blocking a fresh bind with no intervening traffic on the old port,
    // which is what the real-world report looked like.
    let mut rebound = false;
    for _ in 0..10 {
        let args_b = ssh_args(gw_port, &client_priv, &[(fwd_port, svc)], None);
        let mut child_b = Command::new("ssh")
            .args(&args_b)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawn second ssh -R reusing the same port (T-SSH-PUB6)")?;

        // `ExitOnForwardFailure=yes` (baked into `ssh_base_args`) makes a
        // denied `-R` (e.g. `EADDRINUSE` from a still-live zombie listener)
        // exit the client with a nonzero status once the full handshake +
        // auth + global-request round trip completes; a successful bind
        // instead holds `-N` open indefinitely. The grace window here MUST
        // be generous (3s, not e.g. 200ms): a `try_wait`/short-timeout check
        // taken while the client is still mid-KEX/auth (hasn't even sent its
        // `-R` request yet) reads as "still alive" too and is a false
        // positive for "bind accepted" — this cost real debugging time
        // (confirmed by instrumenting `ss -tlnp` directly: the zombie
        // listener's fd was provably still open across the entire window a
        // shorter check was sampling).
        // Timing out waiting for exit means it's still running, i.e. bind accepted.
        if time::timeout(Duration::from_secs(3), child_b.wait())
            .await
            .is_err()
        {
            rebound = true;
            child_b.kill().await.ok();
            break;
        }
        // Otherwise it already exited (forwarding denied) — retry.
        time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        rebound,
        "same public port must be rebindable shortly after an ungraceful \
         disconnect (Ctrl+C/SIGKILL), not left zombie forever"
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

// ---------------------------------------------------------------------------
// T-SSH-VH1 — `-R vhost/<label>`: real HTTP traffic routes through the vhost
// frontend to the forwarded local service, and the admin API shows an
// ssh-transport Vhost entry (`docs/plans/plan_SshGateway/phase_05.md`).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_vh1_vhost_forward_routes_http_and_admin_entry() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    let http_port = free_port().await?;
    let cfg = vhost_config("bore.sshtest", http_port, vec![]);
    let gw_port = start_gateway_server_vhost(host_key, dir.path().to_path_buf(), cfg).await?;

    let svc_port = spawn_http_stub("hello from vh1").await?;
    let raw_forward = format!("vhost/vh1sub:0:127.0.0.1:{svc_port}");

    let mut child = Command::new("ssh")
        .args(ssh_args_raw(gw_port, &client_priv, &[raw_forward], None))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh (T-SSH-VH1)")?;

    let admin = wait_admin_data_contains("vh1sub").await?;
    assert!(
        admin.contains("\"ssh\""),
        "admin entry for an SSH vhost forward should show transport ssh: {admin}"
    );

    let resp = send_http(http_port, "vh1sub.bore.sshtest", "/").await?;
    assert!(
        resp.contains("hello from vh1"),
        "expected the forwarded stub's body in the vhost response, got: {resp}"
    );

    child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-VH2 (`docs/plans/plan_SshGateway/phase_05.md`) — exec `basic-auth=u:p`
// on an SSH vhost forward: a request without the header gets 401, one with
// the correct `Authorization` header reaches the backend.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_vh2_basic_auth_enforced() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    let http_port = free_port().await?;
    let cfg = vhost_config("bore.sshtest", http_port, vec![]);
    let gw_port = start_gateway_server_vhost(host_key, dir.path().to_path_buf(), cfg).await?;

    let svc_port = spawn_http_stub("hello from vh2").await?;
    let raw_forward = format!("vhost/vh2sub:0:127.0.0.1:{svc_port}");

    let mut child = Command::new("ssh")
        .args(ssh_args_raw(
            gw_port,
            &client_priv,
            &[raw_forward],
            Some("basic-auth=user:pass"),
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh (T-SSH-VH2)")?;

    // Wait for `basic_auth: true` specifically, not just the label's
    // presence: the admin entry registers before the `exec`-carried
    // `basic-auth=user:pass` param is applied (`await_params`, up to
    // `PARAMS_GRACE`), so polling for the label alone races ahead of
    // enforcement actually being armed and can observe a plain 200.
    wait_admin_data_contains("\"basic_auth\":true").await?;

    // No credentials -> 401.
    let unauthed = send_http(http_port, "vh2sub.bore.sshtest", "/").await?;
    assert!(
        unauthed.starts_with("HTTP/1.1 401"),
        "expected 401 without credentials, got: {unauthed}"
    );

    // base64("user:pass") == "dXNlcjpwYXNz".
    let authed = send_http_auth(
        http_port,
        "vh2sub.bore.sshtest",
        "/",
        Some("Authorization: Basic dXNlcjpwYXNz"),
    )
    .await?;
    assert!(
        authed.contains("hello from vh2"),
        "expected the backend body with correct credentials, got: {authed}"
    );

    child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-PFX1 — `vhost/`/`secret/` prefixes override the bare-label port
// heuristic (`parse_forward_spec`, `src/sshgw.rs`): `vhost/pfx:0:...`
// registers a vhost subdomain despite port 0 (which alone means "secret
// provider"), and `secret/sid:80:...` is dispatched as a secret provider
// forward despite port 80 (which alone means "vhost").
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_pfx1_prefix_overrides_port_heuristic() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    let http_port = free_port().await?;
    let cfg = vhost_config("bore.sshtest", http_port, vec![]);
    let gw_port = start_gateway_server_vhost(host_key, dir.path().to_path_buf(), cfg).await?;

    // `vhost/pfx` on port 0: the prefix wins over the "port 0 -> secret"
    // heuristic, so this must register as a vhost subdomain and proxy real
    // traffic exactly like T-SSH-VH1.
    let svc_port = spawn_http_stub("hello from pfx").await?;
    let vhost_forward = format!("vhost/pfx:0:127.0.0.1:{svc_port}");
    let mut vhost_child = Command::new("ssh")
        .args(ssh_args_raw(gw_port, &client_priv, &[vhost_forward], None))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh vhost/pfx (T-SSH-PFX1)")?;

    wait_admin_data_contains("pfx").await?;
    let resp = send_http(http_port, "pfx.bore.sshtest", "/").await?;
    assert!(
        resp.contains("hello from pfx"),
        "vhost/pfx on port 0 should register as a vhost subdomain, got: {resp}"
    );
    vhost_child.kill().await.ok();
    time::sleep(Duration::from_millis(200)).await;

    // `secret/sid` on port 80: the prefix wins over the "port 80 -> vhost"
    // heuristic, so this dispatches as a secret-provider forward (Phase 5.3).
    // If this had instead been misclassified as `vhost/sid` on port 80, the
    // admin dashboard would show a vhost entry for "sid", not a
    // `secret-provider`-role entry with `secret_id":"sid"` — so asserting
    // that row is specifically proof the `secret/` prefix was honored over
    // the port heuristic.
    let secret_svc_port = spawn_http_stub("unreachable").await?;
    let secret_forward = format!("secret/sid:80:127.0.0.1:{secret_svc_port}");
    let mut secret_child = Command::new("ssh")
        .args(ssh_args_raw(gw_port, &client_priv, &[secret_forward], None))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh secret/sid (T-SSH-PFX1)")?;

    let admin = wait_admin_data_contains("\"secret_id\":\"sid\"").await?;
    assert!(
        admin.contains("\"role\":\"secret-provider\""),
        "secret/sid on port 80 must dispatch as a secret-provider forward, \
         not silently succeed as a vhost forward: {admin}"
    );
    secret_child.kill().await.ok();

    Ok(())
}

/// Locates the single-level JSON object containing `needle` within an
/// `/admin/status/data` response — `EntryView` (`src/admin.rs`) has no nested
/// `{}` (arrays like `vpn_advertised` use `[]`), so the nearest enclosing
/// `{`/`}` pair around a field match is exactly one tunnel row.
fn entry_json_containing<'a>(data: &'a str, needle: &str) -> Option<&'a str> {
    let at = data.find(needle)?;
    let start = data[..at].rfind('{')?;
    let end = at + data[at..].find('}')?;
    Some(&data[start..=end])
}

// ---------------------------------------------------------------------------
// T-SSH-SEC1 — `secret/` provider over SSH (`-R secret/<id>:0:...`) served by
// a NATIVE consumer (`secret::Proxy`, the same library API `secret_test.rs`
// uses): proves `channel_open_direct_tcpip`'s provider side (Phase 5.3) works
// transparently against an unmodified native consumer.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_sec1_ssh_provider_native_consumer() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    let gw_port = start_gateway_server(host_key, dir.path().to_path_buf()).await?;
    let svc_port = spawn_echo_service().await?;

    let id = "sec1";
    let forward = format!("secret/{id}:0:127.0.0.1:{svc_port}");
    let mut ssh_child = Command::new("ssh")
        .args(ssh_args_raw(gw_port, &client_priv, &[forward], None))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh secret provider (T-SSH-SEC1)")?;

    wait_admin_data_contains(&format!("\"secret_id\":\"{id}\"")).await?;

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        id,
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0,
        1,
        None,
        false,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    assert!(
        roundtrip(addr.port(), b"hello from native consumer").await?,
        "native consumer must round-trip through the ssh-backed secret provider"
    );

    ssh_child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-SEC2 — NATIVE provider (`Client::new_secret_provider`) served by a
// secret consumer over SSH (`-L <local>:<id>:0`, dispatched by
// `channel_open_direct_tcpip`): proves the SSH-gateway consumer side (Phase
// 5.3) works transparently against an unmodified native provider.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_sec2_native_provider_ssh_consumer() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    let gw_port = start_gateway_server(host_key, dir.path().to_path_buf()).await?;
    let echo_port = spawn_echo_service().await?;

    let id = "sec2";
    let provider = Client::new_secret_provider(
        "localhost",
        echo_port,
        "localhost",
        id,
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0,
        1024,
        1,
        ProviderMeta::default(),
        None,
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(50)).await;

    let lp = free_port().await?;
    let mut ssh_child = Command::new("ssh")
        .args(ssh_local_forward_args(
            gw_port,
            &client_priv,
            lp,
            &format!("{id}:1"),
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh secret consumer (T-SSH-SEC2)")?;
    wait_port(lp, true).await;

    assert!(
        roundtrip(lp, b"hello from ssh consumer").await?,
        "ssh consumer -L forward must round-trip through the native secret provider"
    );

    ssh_child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-SEC3 — ssh on both sides: roundtrip + admin shows exactly one
// secret-provider row and one secret-consumer row (transport ssh); opening 3
// concurrent proxied connections through the `-L` consumer must NOT create
// extra admin rows — `active` increments to 3 on the SAME row (D11/BUG-S1
// parity: one row per (session, id), never one row per channel).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_sec3_ssh_both_sides_admin_rows() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    let gw_port = start_gateway_server(host_key, dir.path().to_path_buf()).await?;
    let svc_port = spawn_echo_service().await?;

    let id = "sec3";
    let provider_forward = format!("secret/{id}:0:127.0.0.1:{svc_port}");
    let mut provider_child = Command::new("ssh")
        .args(ssh_args_raw(
            gw_port,
            &client_priv,
            &[provider_forward],
            None,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh secret provider (T-SSH-SEC3)")?;

    wait_admin_data_contains(&format!("\"secret_id\":\"{id}\"")).await?;

    let lp = free_port().await?;
    let mut consumer_child = Command::new("ssh")
        .args(ssh_local_forward_args(
            gw_port,
            &client_priv,
            lp,
            &format!("{id}:1"),
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh secret consumer (T-SSH-SEC3)")?;
    wait_port(lp, true).await;

    assert!(
        roundtrip(lp, b"hello ssh-to-ssh").await?,
        "ssh consumer -L forward must round-trip through the ssh secret provider"
    );

    let admin = wait_admin_data_contains("\"role\":\"secret-consumer\"").await?;
    assert_eq!(
        admin.matches("\"transport\":\"ssh\"").count(),
        2,
        "expected exactly one secret-provider row and one secret-consumer row: {admin}"
    );
    let consumer_row = entry_json_containing(&admin, "\"role\":\"secret-consumer\"")
        .context("no secret-consumer admin row found")?;
    assert!(
        consumer_row.contains(&format!("\"secret_id\":\"{id}\"")),
        "consumer row must be scoped to '{id}': {consumer_row}"
    );

    // 3 concurrent proxied connections must bump `active` on the SAME
    // consumer row, never spawn a second row (BUG-S1 parity).
    let mut conns = Vec::new();
    for _ in 0..3 {
        conns.push(TcpStream::connect(("127.0.0.1", lp)).await?);
    }

    let mut admin = String::new();
    for _ in 0..800 {
        let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
        admin = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
        if entry_json_containing(&admin, "\"role\":\"secret-consumer\"")
            .is_some_and(|row| row.contains("\"active\":3"))
        {
            break;
        }
        time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        admin.matches("\"transport\":\"ssh\"").count(),
        2,
        "3 concurrent channels on one id must not create extra admin rows: {admin}"
    );
    let consumer_row = entry_json_containing(&admin, "\"role\":\"secret-consumer\"")
        .context("no secret-consumer admin row found")?;
    assert!(
        consumer_row.contains("\"active\":3"),
        "consumer active must be 3 with 3 open channels: {consumer_row}"
    );

    drop(conns);
    provider_child.kill().await.ok();
    consumer_child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-TAKE1 (`docs/plans/plan_SshGateway/phase_05.md`, D2/I-5) — a second
// SSH session authenticating with the SAME key as the incumbent takes over
// an already-registered vhost label: traffic switches to the new backend,
// and the evicted session (which had no other forwards) is disconnected by
// the gateway.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_take1_same_identity_vhost_takeover() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    // Both sessions authenticate with this SAME key, so both get the SAME
    // `KeyGrant::identity` — the exact scenario D2/I-5 evicts on.
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    let http_port = free_port().await?;
    let cfg = vhost_config("bore.sshtest", http_port, vec![]);
    let gw_port = start_gateway_server_vhost(host_key, dir.path().to_path_buf(), cfg).await?;

    let svc1 = spawn_http_stub("body one").await?;
    let svc2 = spawn_http_stub("body two").await?;

    let mut child_a = Command::new("ssh")
        .args(ssh_args_raw(
            gw_port,
            &client_priv,
            &[format!("vhost/take1:0:127.0.0.1:{svc1}")],
            None,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh session A (T-SSH-TAKE1)")?;

    wait_admin_data_contains("take1").await?;
    let first = send_http(http_port, "take1.bore.sshtest", "/").await?;
    assert!(
        first.contains("body one"),
        "expected session A's backend before takeover, got: {first}"
    );

    let mut child_b = Command::new("ssh")
        .args(ssh_args_raw(
            gw_port,
            &client_priv,
            &[format!("vhost/take1:0:127.0.0.1:{svc2}")],
            None,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh session B (T-SSH-TAKE1)")?;

    // Session A held exactly one forward, so evicting it leaves it with zero
    // remaining forwards: the gateway must disconnect it (D2 step 2).
    let a_exit = time::timeout(Duration::from_secs(20), child_a.wait())
        .await
        .context("evicted session A never exited")??;
    assert!(
        !a_exit.success(),
        "evicted session A should not exit cleanly, got {a_exit:?}"
    );

    // Traffic must switch to session B's backend (poll: the plan's 2s
    // switchover tolerance, given generous headroom for CI scheduling).
    let mut switched = false;
    for _ in 0..800 {
        if let Ok(resp) = send_http(http_port, "take1.bore.sshtest", "/").await {
            if resp.contains("body two") {
                switched = true;
                break;
            }
        }
        time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        switched,
        "traffic never switched to the takeover winner's backend"
    );

    child_b.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-TAKE2 (`docs/plans/plan_SshGateway/phase_05.md`, D2/I-5) — a third
// session authenticating with a DIFFERENT key can never take over a label:
// rejected outright, and the incumbent's tunnel keeps serving.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_take2_different_identity_rejected() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let bob_priv = gen_keypair_with_comment(dir.path(), "bob", "bob").await?;
    let carol_priv = gen_keypair_with_comment(dir.path(), "carol", "carol").await?;
    write_authorized_keys_named(dir.path(), "authorized_keys_bob", &bob_priv, None)?;
    write_authorized_keys_named(dir.path(), "authorized_keys_carol", &carol_priv, None)?;

    let http_port = free_port().await?;
    let cfg = vhost_config("bore.sshtest", http_port, vec![]);
    let gw_port = start_gateway_server_vhost(host_key, dir.path().to_path_buf(), cfg).await?;

    let svc_b = spawn_http_stub("bob's backend").await?;
    let svc_c = spawn_http_stub("carol's backend").await?;

    let mut child_b = Command::new("ssh")
        .args(ssh_args_raw(
            gw_port,
            &bob_priv,
            &[format!("vhost/take2:0:127.0.0.1:{svc_b}")],
            None,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh session B (T-SSH-TAKE2)")?;

    wait_admin_data_contains("take2").await?;
    let resp = send_http(http_port, "take2.bore.sshtest", "/").await?;
    assert!(
        resp.contains("bob's backend"),
        "expected bob's backend before carol's attempt, got: {resp}"
    );

    // `ExitOnForwardFailure=yes` makes a rejected `-R` exit the client
    // non-zero — the label must never be handed to a different identity.
    let child_c = Command::new("ssh")
        .args(ssh_args_raw(
            gw_port,
            &carol_priv,
            &[format!("vhost/take2:0:127.0.0.1:{svc_c}")],
            None,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawn ssh session C (T-SSH-TAKE2)")?;
    assert!(
        !child_c.status.success(),
        "a different identity's takeover attempt must be rejected, got {:?}",
        child_c.status
    );

    // Bob's tunnel must be completely unaffected by the rejected attempt.
    let after = send_http(http_port, "take2.bore.sshtest", "/").await?;
    assert!(
        after.contains("bob's backend"),
        "incumbent's tunnel must still serve after a rejected takeover attempt, got: {after}"
    );

    child_b.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-DMX1 (`docs/plans/plan_SshGateway/phase_06.md`, D8) — control-port
// demux: ONE port serves SSH, TLS, plain HTTP and native bore concurrently.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_ssh_dmx1_one_port_serves_ssh_tls_http_bore_concurrently() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    start_demux_server(host_key, dir.path().to_path_buf()).await?;

    // (a) native public tunnel over a TLS control connection.
    let echo_a = spawn_echo_service().await?;
    let client_a = Client::new(
        "localhost",
        echo_a,
        &format!("https://127.0.0.1:{CONTROL_PORT}"),
        0,
        None,
        true,
        Default::default(),
        None,
    )
    .await
    .context("native TLS client (T-SSH-DMX1)")?;
    let port_a = client_a.remote_port();
    tokio::spawn(client_a.listen());

    // (b) ssh -N -R public tunnel on the SAME control port (no dedicated
    // --ssh-port is configured — this only works if the demux is live).
    let echo_b = spawn_echo_service().await?;
    let bind_port_b = free_port().await?;
    let mut ssh_child = Command::new("ssh")
        .args(ssh_args(
            CONTROL_PORT,
            &client_priv,
            &[(bind_port_b, echo_b)],
            None,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh (T-SSH-DMX1)")?;
    wait_port(bind_port_b, true).await;

    // (d) plain bore client (no TLS scheme) on the same port.
    let echo_d = spawn_echo_service().await?;
    let client_d = Client::new(
        "localhost",
        echo_d,
        &format!("127.0.0.1:{CONTROL_PORT}"),
        0,
        None,
        false,
        Default::default(),
        None,
    )
    .await
    .context("native plain client (T-SSH-DMX1)")?;
    let port_d = client_d.remote_port();
    tokio::spawn(client_d.listen());

    // (c) plain-HTTP admin request on the same port, concurrently with (a),
    // (b) and (d) all still live.
    let admin = wait_admin_data_contains("\"role\"").await?;
    assert!(!admin.is_empty());

    // All four succeed simultaneously.
    assert!(
        roundtrip(port_a, b"via-tls-1").await?,
        "TLS-scheme native tunnel must round-trip"
    );
    assert!(
        roundtrip(bind_port_b, b"via-ssh-12").await?,
        "ssh -R tunnel must round-trip on the demuxed control port"
    );
    assert!(
        roundtrip(port_d, b"via-plain-1").await?,
        "plain (no-TLS-scheme) native tunnel must round-trip on a TLS-configured port"
    );

    ssh_child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-DMX2 — a silent client (sends nothing) gets the SSH banner spoken
// first, within the timeout fallback (sslh-style).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_dmx2_silent_client_gets_ssh_banner() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    start_demux_server(host_key, dir.path().to_path_buf()).await?;

    let mut conn = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let mut buf = [0u8; 8];
    time::timeout(Duration::from_millis(2500), conn.read_exact(&mut buf))
        .await
        .context("server never spoke first within the timeout fallback")??;
    assert_eq!(&buf, b"SSH-2.0-", "expected an SSH banner, got: {buf:?}");

    Ok(())
}

// ---------------------------------------------------------------------------
// T-SSH-TLS1 (D4) — SSH-over-TLS: an `openssl s_client` `ProxyCommand`
// tunnels a real ssh session through the control port's TLS listener.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_ssh_tls1_ssh_over_tls_via_proxycommand() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();
    skip_without_openssl!();
    wait_port(CONTROL_PORT, false).await;

    let dir = tempfile::tempdir()?;
    let host_key = gen_keypair(dir.path(), "host_key").await?;
    let client_priv = gen_keypair(dir.path(), "client").await?;
    write_authorized_keys(dir.path(), &client_priv, None)?;

    start_demux_server(host_key, dir.path().to_path_buf()).await?;

    let svc_port = spawn_echo_service().await?;
    let bind_port = free_port().await?;
    let mut ssh_child = Command::new("ssh")
        .args(ssh_proxycommand_args(
            &client_priv,
            &[format!("{bind_port}:127.0.0.1:{svc_port}")],
            None,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh over TLS ProxyCommand (T-SSH-TLS1)")?;
    wait_port(bind_port, true).await;

    assert!(
        roundtrip(bind_port, b"hello over ssh-over-tls").await?,
        "SSH-over-TLS tunnel must round-trip"
    );

    ssh_child.kill().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// T-DMX-OFF — gateway DISABLED, TLS configured: a client sending
// `SSH-2.0-...` gets no SSH banner back and the connection is not served as
// SSH (proves the demux branch is truly off; the existing TLS/plain/admin
// paths staying green is covered by `tests/tls_test.rs` + `tests/admin_test.rs`,
// run unmodified as part of the same `cargo test --all-features` gate).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t_dmx_off_gateway_disabled_ignores_ssh_looking_bytes() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;

    let (cert_pem, key_pem) = self_signed_cert()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, None);
    server.set_tls(acceptor);
    // No `set_ssh_gateway` call: the gateway is disabled.
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;

    let mut conn = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    conn.write_all(b"SSH-2.0-OpenSSH_9.0\r\n").await?;

    // Without the gateway, this port always TLS-accepts every connection
    // (I-1, the untouched legacy path) — plain bytes are not a valid TLS
    // ClientHello, so the handshake fails and the connection is dropped
    // (EOF), never an SSH banner.
    let mut buf = [0u8; 8];
    let result = time::timeout(Duration::from_secs(3), conn.read_exact(&mut buf)).await;
    match result {
        Ok(Ok(_)) => panic!("gateway disabled but still served an SSH-looking response: {buf:?}"),
        Ok(Err(_)) => {} // connection closed/reset: expected
        Err(_) => panic!("connection neither closed nor produced a banner within the timeout"),
    }

    Ok(())
}
