#![cfg(feature = "ssh-gateway")]
//! Phase 1.2 design-gating spike (`docs/plans/plan_SshGateway/phase_01.md`): embeds
//! a minimal russh server on an ephemeral port and drives it with the REAL OpenSSH
//! CLI to lock down the exact API surface (pubkey auth, `-R`/`-L` forwarding, exec+env,
//! keepalive) before phases 4-6 build the actual gateway on top of it. Findings are
//! written to `docs/plans/plan_SshGateway/SPIKE_FINDINGS.md`.
//!
//! Skips (prints a warning, passes) when `ssh`/`ssh-keygen` are not on `PATH` — CI
//! installs them in phase 7.3.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use lazy_static::lazy_static;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{run_stream, Auth, Config, Handler, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::time;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

/// `true` if both `ssh` and `ssh-keygen` are invocable on `PATH`.
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

/// Returns `true` (and prints a warning) when the test should skip for lack of an
/// OpenSSH CLI on `PATH`.
macro_rules! skip_without_ssh_cli {
    () => {
        if !has_ssh_cli().await {
            eprintln!(
                "WARNING: `ssh`/`ssh-keygen` not found on PATH — skipping {}",
                module_path!()
            );
            return;
        }
    };
}

/// Generates a throwaway ed25519 keypair via the real `ssh-keygen` CLI and returns
/// the private key path (the matching `<path>.pub` is created alongside it).
async fn gen_keypair(dir: &Path, name: &str) -> Result<PathBuf> {
    let priv_path = dir.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&priv_path)
        .args(["-C", "spike"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("spawn ssh-keygen")?;
    if !status.success() {
        return Err(anyhow!("ssh-keygen exited with {status}"));
    }
    Ok(priv_path)
}

/// Shared `ssh` CLI options: no host-key persistence/prompts, pubkey-only, no
/// interactive fallback. `ConnectTimeout` covers TCP connect AND the full SSH
/// handshake/KEX (ssh_config(5)), not just the socket connect — 30s gives a
/// loaded CI runner headroom for that whole phase (see `ssh_gateway_test.rs`'s
/// `ssh_base_args` for the CI failures this was tuned against).
fn ssh_base_args(port: u16, identity: &Path) -> Vec<String> {
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
        "ConnectTimeout=30".into(),
        "-i".into(),
        identity.display().to_string(),
        "-p".into(),
        port.to_string(),
        "spike@127.0.0.1".into(),
    ]
}

fn base_config(host_key: PrivateKey) -> Arc<Config> {
    Arc::new(Config {
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(20)),
        auth_rejection_time: Duration::from_millis(50),
        ..Default::default()
    })
}

/// Accepts connections on `listener` forever, running `make_handler()` (fresh
/// per-connection handler) through `russh::server::run_stream`.
fn spawn_accept_loop<H, F>(listener: TcpListener, config: Arc<Config>, make_handler: F)
where
    H: Handler + Send + 'static,
    H::Error: std::fmt::Debug,
    F: Fn() -> H + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let (socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let config = config.clone();
            let handler = make_handler();
            tokio::spawn(async move {
                match run_stream(config, socket, handler).await {
                    Ok(running) => {
                        let _ = running.await;
                    }
                    Err(e) => eprintln!("run_stream error: {e:?}"),
                }
            });
        }
    });
}

async fn load_pubkey(priv_path: &Path) -> Result<PublicKey> {
    let key = PrivateKey::read_openssh_file(priv_path)
        .map_err(|e| anyhow!("read_openssh_file({}): {e}", priv_path.display()))?;
    Ok(key.public_key().clone())
}

// ---------------------------------------------------------------------------
// T-SSH-SPIKE1 — pubkey auth: accepted key succeeds, a different key is rejected.
// ---------------------------------------------------------------------------

struct AuthOnlyHandler {
    accepted: PublicKey,
}

impl Handler for AuthOnlyHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth> {
        if public_key.key_data() == self.accepted.key_data() {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        session.channel_success(channel)?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

#[tokio::test]
async fn t_ssh_spike1_pubkey_auth() {
    let _guard = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();

    let dir = tempfile::tempdir().expect("tempdir");
    let host_key_path = gen_keypair(dir.path(), "host").await.expect("host key");
    let host_key = PrivateKey::read_openssh_file(&host_key_path).expect("load host key");
    let accepted_priv = gen_keypair(dir.path(), "accepted")
        .await
        .expect("accepted key");
    let other_priv = gen_keypair(dir.path(), "other").await.expect("other key");
    let accepted_pub = load_pubkey(&accepted_priv).await.expect("accepted pubkey");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let config = base_config(host_key);
    spawn_accept_loop(listener, config, move || AuthOnlyHandler {
        accepted: accepted_pub.clone(),
    });

    let ok = Command::new("ssh")
        .args(ssh_base_args(port, &accepted_priv))
        .arg("true")
        .stdin(Stdio::null())
        .status()
        .await
        .expect("spawn ssh (accepted key)");
    assert!(ok.success(), "accepted key must authenticate: {ok}");

    let rejected = Command::new("ssh")
        .args(ssh_base_args(port, &other_priv))
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("spawn ssh (other key)");
    assert!(!rejected.success(), "a different key must NOT authenticate");
}

// ---------------------------------------------------------------------------
// T-SSH-SPIKE2 — tcpip-forward + forwarded-tcpip (`-R`): full server->client data path.
// ---------------------------------------------------------------------------

/// Local TCP echo listener the `ssh -R` client relays into. Returns its port.
async fn spawn_echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let port = listener.local_addr().expect("local_addr").port();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if socket.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    port
}

struct ForwardHandler {
    accepted: PublicKey,
    result_tx: mpsc::UnboundedSender<Result<(), String>>,
}

impl Handler for ForwardHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth> {
        Ok(if public_key.key_data() == self.accepted.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool> {
        if address != "testname" || *port != 0 {
            let _ = self
                .result_tx
                .send(Err(format!("unexpected tcpip_forward({address}, {port})")));
            return Ok(false);
        }
        *port = 45001;
        let handle = session.handle();
        let tx = self.result_tx.clone();
        let granted_port = *port;
        tokio::spawn(async move {
            // give the client time to receive REQUEST_SUCCESS before we push data.
            time::sleep(Duration::from_millis(300)).await;
            let outcome = async {
                let channel = handle
                    .channel_open_forwarded_tcpip("testname", granted_port, "203.0.113.1", 12345)
                    .await
                    .map_err(|e| format!("channel_open_forwarded_tcpip: {e}"))?;
                let mut stream = channel.into_stream();
                stream
                    .write_all(b"ping")
                    .await
                    .map_err(|e| format!("write_all: {e}"))?;
                let mut buf = [0u8; 4];
                stream
                    .read_exact(&mut buf)
                    .await
                    .map_err(|e| format!("read_exact: {e}"))?;
                if &buf != b"ping" {
                    return Err(format!("echo mismatch: {buf:?}"));
                }
                Ok(())
            }
            .await;
            let _ = tx.send(outcome);
        });
        Ok(true)
    }
}

#[tokio::test]
async fn t_ssh_spike2_forwarded_tcpip() {
    let _guard = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();

    let dir = tempfile::tempdir().expect("tempdir");
    let host_key_path = gen_keypair(dir.path(), "host").await.expect("host key");
    let host_key = PrivateKey::read_openssh_file(&host_key_path).expect("load host key");
    let client_priv = gen_keypair(dir.path(), "client").await.expect("client key");
    let client_pub = load_pubkey(&client_priv).await.expect("client pubkey");

    let echo_port = spawn_echo_server().await;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = base_config(host_key);
    spawn_accept_loop(listener, config, move || ForwardHandler {
        accepted: client_pub.clone(),
        result_tx: tx.clone(),
    });

    let mut args = ssh_base_args(port, &client_priv);
    args.insert(0, "-N".into());
    args.push("-R".into());
    args.push(format!("testname:0:127.0.0.1:{echo_port}"));
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ssh -R");

    let outcome = time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for forwarded-tcpip round trip")
        .expect("channel closed without a result");
    let _ = child.kill().await;
    outcome.expect("forwarded-tcpip ping/pong");
}

// ---------------------------------------------------------------------------
// T-SSH-SPIKE3 — direct-tcpip (`-L`): client-initiated channel open, server echoes.
// ---------------------------------------------------------------------------

struct DirectTcpipHandler {
    accepted: PublicKey,
    result_tx: mpsc::UnboundedSender<Result<(), String>>,
}

impl Handler for DirectTcpipHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth> {
        Ok(if public_key.key_data() == self.accepted.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        if host_to_connect != "testname" || port_to_connect != 1 {
            let _ = self.result_tx.send(Err(format!(
                "unexpected direct-tcpip target {host_to_connect}:{port_to_connect}"
            )));
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        reply.accept().await;
        let tx = self.result_tx.clone();
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let outcome = async {
                let mut buf = [0u8; 4];
                stream
                    .read_exact(&mut buf)
                    .await
                    .map_err(|e| format!("read_exact: {e}"))?;
                stream
                    .write_all(&buf)
                    .await
                    .map_err(|e| format!("write_all: {e}"))?;
                Ok(())
            }
            .await;
            let _ = tx.send(outcome);
        });
        Ok(())
    }
}

#[tokio::test]
async fn t_ssh_spike3_direct_tcpip() {
    let _guard = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();

    let dir = tempfile::tempdir().expect("tempdir");
    let host_key_path = gen_keypair(dir.path(), "host").await.expect("host key");
    let host_key = PrivateKey::read_openssh_file(&host_key_path).expect("load host key");
    let client_priv = gen_keypair(dir.path(), "client").await.expect("client key");
    let client_pub = load_pubkey(&client_priv).await.expect("client pubkey");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let local_fwd_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind lfwd");
    let lport = local_fwd_listener.local_addr().expect("local_addr").port();
    drop(local_fwd_listener); // free the port for ssh to bind; small TOCTOU is fine in a test

    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = base_config(host_key);
    spawn_accept_loop(listener, config, move || DirectTcpipHandler {
        accepted: client_pub.clone(),
        result_tx: tx.clone(),
    });

    let mut args = ssh_base_args(port, &client_priv);
    args.insert(0, "-N".into());
    args.push("-L".into());
    // Unlike `-R` (where a `:0` REMOTE listen port asks the server to allocate one),
    // OpenSSH's `-L` client-side parser rejects a `:0` remote TARGET port outright
    // ("Bad local forwarding specification") — found via this spike, see
    // SPIKE_FINDINGS.md. Use an arbitrary nonzero placeholder instead.
    args.push(format!("{lport}:testname:1"));
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ssh -L");

    // wait for ssh to bind the local forward port.
    let mut connected = None;
    for _ in 0..200 {
        match tokio::net::TcpStream::connect(("127.0.0.1", lport)).await {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(_) => time::sleep(Duration::from_millis(25)).await,
        }
    }
    let connected = match connected {
        Some(s) => s,
        None => {
            let _ = child.kill().await;
            let output = child.wait_with_output().await.expect("wait_with_output");
            panic!(
                "ssh -L local port never came up; stdout={:?} stderr={:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    let mut socket = connected;
    socket.write_all(b"ping").await.expect("write ping");
    let mut buf = [0u8; 4];
    socket.read_exact(&mut buf).await.expect("read pong");
    assert_eq!(&buf, b"ping");

    let outcome = time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for direct-tcpip result")
        .expect("channel closed without a result");
    let _ = child.kill().await;
    outcome.expect("direct-tcpip echo");
}

// ---------------------------------------------------------------------------
// T-SSH-SPIKE4 — exec + env: env var observed, exec command observed, server->client data.
// ---------------------------------------------------------------------------

struct ExecEnvHandler {
    accepted: PublicKey,
    result_tx: mpsc::UnboundedSender<String>,
}

impl Handler for ExecEnvHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth> {
        Ok(if public_key.key_data() == self.accepted.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<()> {
        let _ = self
            .result_tx
            .send(format!("env:{variable_name}={variable_value}"));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let _ = self
            .result_tx
            .send(format!("exec:{}", String::from_utf8_lossy(data)));
        session.channel_success(channel)?;
        session.data(channel, "hello-from-spike\n")?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

#[tokio::test]
async fn t_ssh_spike4_exec_env() {
    let _guard = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();

    let dir = tempfile::tempdir().expect("tempdir");
    let host_key_path = gen_keypair(dir.path(), "host").await.expect("host key");
    let host_key = PrivateKey::read_openssh_file(&host_key_path).expect("load host key");
    let client_priv = gen_keypair(dir.path(), "client").await.expect("client key");
    let client_pub = load_pubkey(&client_priv).await.expect("client pubkey");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = base_config(host_key);
    spawn_accept_loop(listener, config, move || ExecEnvHandler {
        accepted: client_pub.clone(),
        result_tx: tx.clone(),
    });

    let mut args = ssh_base_args(port, &client_priv);
    args.insert(0, "SetEnv=BORE_NOTES=spike".into());
    args.insert(0, "-o".into());
    args.push("notes=cli".into());
    let output = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .await
        .expect("spawn ssh exec");
    assert!(
        output.status.success(),
        "exec must succeed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("hello-from-spike"),
        "stdout must contain server-written line, got: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(
        events.iter().any(|e| e == "exec:notes=cli"),
        "handler must observe the exec command, got: {events:?}"
    );
    // OpenSSH only sends `env` for a `SetEnv`/`SendEnv`-matched variable; record
    // whatever actually arrived (or didn't) in SPIKE_FINDINGS.md rather than
    // asserting blindly, per the phase-1.2 spec ("record actual behavior").
    eprintln!("T-SSH-SPIKE4 observed events: {events:?}");
}

// ---------------------------------------------------------------------------
// T-SSH-SPIKE5 — keepalive: client-side ServerAliveInterval must not kill the
// session; record how the SERVER can proactively probe the client (I-3).
// ---------------------------------------------------------------------------

struct KeepaliveHandler {
    accepted: PublicKey,
}

impl Handler for KeepaliveHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth> {
        Ok(if public_key.key_data() == self.accepted.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn tcpip_forward(
        &mut self,
        _address: &str,
        port: &mut u32,
        _session: &mut Session,
    ) -> Result<bool> {
        *port = 45002;
        Ok(true)
    }
}

#[tokio::test]
async fn t_ssh_spike5_keepalive() {
    let _guard = SERIAL_GUARD.lock().await;
    skip_without_ssh_cli!();

    let dir = tempfile::tempdir().expect("tempdir");
    let host_key_path = gen_keypair(dir.path(), "host").await.expect("host key");
    let host_key = PrivateKey::read_openssh_file(&host_key_path).expect("load host key");
    let client_priv = gen_keypair(dir.path(), "client").await.expect("client key");
    let client_pub = load_pubkey(&client_priv).await.expect("client pubkey");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    // Server-initiated keepalive under test too (I-3): russh's own Config-driven
    // probe, independent of the client's ServerAliveInterval below.
    let config = Arc::new(Config {
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(20)),
        auth_rejection_time: Duration::from_millis(50),
        keepalive_interval: Some(Duration::from_millis(500)),
        keepalive_max: 4,
        ..Default::default()
    });
    spawn_accept_loop(listener, config, move || KeepaliveHandler {
        accepted: client_pub.clone(),
    });

    let mut args = ssh_base_args(port, &client_priv);
    args.insert(0, "ServerAliveCountMax=2".into());
    args.insert(0, "-o".into());
    args.insert(0, "ServerAliveInterval=1".into());
    args.insert(0, "-o".into());
    args.insert(0, "-N".into());
    args.push("-R".into());
    args.push("testname:0:127.0.0.1:1".into());
    let mut child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ssh keepalive");

    time::sleep(Duration::from_secs(5)).await;
    let status = child.try_wait().expect("try_wait");
    let _ = child.kill().await;
    assert!(
        status.is_none(),
        "ssh session must survive 5s of ServerAliveInterval=1 probing, got exit: {status:?}"
    );
}
