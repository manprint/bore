//! End-to-end regression coverage for `bore local`'s PORT positional accepting
//! `HOST:PORT` (mirroring `bore vhost`/`bore proxy`'s target syntax), not just a
//! bare port. Before the fix, `bore local -p 9005 --https 10.10.16.138:5000
//! --udp --auto-reconnect -s mysecret` failed at the clap layer with a confusing
//! "invalid digit found in string" error instead of targeting the remote host,
//! because the PORT positional was a bare `u16`. These tests drive the real
//! compiled `bore` binary (not just the library) to prove the full pipeline —
//! CLI parsing, host/port resolution, and actual tunneled data flow — works for
//! both the historical bare-port syntax and the new HOST:PORT syntax, and that
//! conflicting/malformed input now surfaces a clear error instead of the old
//! confusing one.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use bore_cli::{server::Server, shared::CONTROL_PORT};
use lazy_static::lazy_static;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    /// Serialize tests sharing the fixed `CONTROL_PORT`.
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

async fn wait_for_control_port(listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", CONTROL_PORT))
            .await
            .is_ok()
            == listening
        {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_server() {
    wait_for_control_port(false).await;
    tokio::spawn(Server::new(1024..=65535, Some("local-host-port-secret")).listen());
    wait_for_control_port(true).await;
}

fn bore_binary() -> Result<PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_bore")
        .map(PathBuf::from)
        .context("CARGO_BIN_EXE_bore is not available for subprocess tests")
}

/// Local echo service bound explicitly to `127.0.0.1`.
async fn spawn_echo_service() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                if let Ok(n) = stream.read(&mut buf).await {
                    if n > 0 {
                        let _ = stream.write_all(&buf[..n]).await;
                    }
                }
            });
        }
    });
    Ok(port)
}

fn local_child(args: &[&str]) -> Result<Child> {
    let mut cmd = Command::new(bore_binary()?);
    cmd.arg("local")
        .args(args)
        .arg("--to")
        .arg("localhost")
        .arg("--secret")
        .arg("local-host-port-secret");
    cmd.env("RUST_LOG", "debug");
    cmd.kill_on_drop(true);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn().context("failed to spawn bore local subprocess")
}

/// Take the subprocess's stderr (piped, so tracing runs with ANSI off) and
/// spawn a background reader that tees every line to this process's stderr
/// (visible with `--nocapture`) and resolves once it sees the `listening at
/// HOST:PORT` announcement, returning the allocated remote port. Keeps
/// draining for the child's lifetime so later log lines aren't lost.
async fn wait_for_remote_port(child: &mut Child) -> Result<u16> {
    let stderr = child.stderr.take().context("subprocess stderr not piped")?;
    let mut reader = BufReader::new(stderr);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u16>();
    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            eprint!("[child] {line}");
            if let Some(idx) = line.find("listening at ") {
                let rest = line[idx + "listening at ".len()..].trim();
                if let Some(port) = rest.rsplit(':').next().and_then(|p| p.parse().ok()) {
                    let _ = tx.send(port);
                }
            }
        }
    });
    time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .context("timed out waiting for remote port announcement")?
        .context("subprocess exited before announcing a remote port")
}

async fn assert_echoes(remote_port: u16) -> Result<()> {
    let mut conn = TcpStream::connect(("127.0.0.1", remote_port)).await?;
    conn.write_all(b"ping").await?;
    let mut buf = [0u8; 4];
    time::timeout(Duration::from_secs(3), conn.read_exact(&mut buf)).await??;
    assert_eq!(&buf, b"ping");
    Ok(())
}

/// Wait (bounded) for a subprocess to exit, returning its captured output.
async fn wait_exit(child: Child) -> Result<std::process::Output> {
    time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .context("subprocess did not exit in time")?
        .context("failed to wait on subprocess")
}

#[tokio::test]
async fn local_cli_bare_port_still_defaults_to_localhost() -> Result<()> {
    // Regression: the historical `bore local <PORT>` syntax (no host at all)
    // must keep working byte-for-byte after PORT stopped being a bare `u16`.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo_port = spawn_echo_service().await?;

    let mut child = local_child(&[&echo_port.to_string()])?;
    let remote_port = wait_for_remote_port(&mut child).await?;
    assert_echoes(remote_port).await?;
    child.kill().await?;
    Ok(())
}

#[tokio::test]
async fn local_cli_explicit_local_host_flag_still_works() -> Result<()> {
    // Regression: the standalone `--local-host` flag (no embedded host in PORT)
    // must keep working after it changed from `String` to `Option<String>`.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo_port = spawn_echo_service().await?;

    let mut child = local_child(&[&echo_port.to_string(), "--local-host", "127.0.0.1"])?;
    let remote_port = wait_for_remote_port(&mut child).await?;
    assert_echoes(remote_port).await?;
    child.kill().await?;
    Ok(())
}

#[tokio::test]
async fn local_cli_host_colon_port_reaches_target_end_to_end() -> Result<()> {
    // The actual bug: `bore local HOST:PORT` (e.g. `10.10.16.138:5000`) must
    // parse and tunnel to that host, not fail with "invalid digit found in
    // string" on the PORT positional.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo_port = spawn_echo_service().await?;

    let target = format!("127.0.0.1:{echo_port}");
    let mut child = local_child(&[&target])?;
    let remote_port = wait_for_remote_port(&mut child).await?;
    assert_echoes(remote_port).await?;
    child.kill().await?;
    Ok(())
}

#[tokio::test]
async fn local_cli_host_colon_port_matching_local_host_flag_ok() -> Result<()> {
    // An embedded host that agrees with an explicit --local-host is not a
    // conflict and must still work.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo_port = spawn_echo_service().await?;

    let target = format!("127.0.0.1:{echo_port}");
    let mut child = local_child(&[&target, "--local-host", "127.0.0.1"])?;
    let remote_port = wait_for_remote_port(&mut child).await?;
    assert_echoes(remote_port).await?;
    child.kill().await?;
    Ok(())
}

#[tokio::test]
async fn local_cli_conflicting_host_rejected_with_clear_error() -> Result<()> {
    // An embedded host that disagrees with an explicit --local-host must be
    // rejected with an actionable message, not silently pick one.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;

    let child = local_child(&["127.0.0.1:9", "--local-host", "192.0.2.1"])?;
    let output = wait_exit(child).await?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("conflicting host"),
        "expected a conflicting-host error, got:\n{stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn local_cli_malformed_target_rejected_with_clear_error() -> Result<()> {
    // Malformed PORT input (neither a bare port nor HOST:PORT) must fail with
    // a message that explains the accepted syntax, not the old opaque clap
    // "invalid digit found in string".
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;

    let child = local_child(&["not-a-port-or-target"])?;
    let output = wait_exit(child).await?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("expected a port number or host:port"),
        "expected a helpful PORT-syntax error, got:\n{stderr}"
    );
    Ok(())
}
