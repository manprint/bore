use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use anyhow::{bail, Context, Result};
use bore_cli::{server::Server, shared::CONTROL_PORT};
use lazy_static::lazy_static;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time;
use uuid::Uuid;

lazy_static! {
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

async fn spawn_server(udp: bool) {
    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, Some("transfer-secret"));
    server.set_udp(udp);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
}

fn bore_binary() -> Result<PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_bore")
        .map(PathBuf::from)
        .context("CARGO_BIN_EXE_bore is not available for subprocess transfer tests")
}

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bore-transfer-stdin-test-{label}-{}",
        Uuid::new_v4()
    ))
}

async fn read_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    Ok(buf)
}

fn listener_child(
    dest_path: &Path,
    transfer_id: &str,
    relay_only: bool,
    overwrite: bool,
    rename: bool,
    stun_server: Option<&str>,
) -> Result<Child> {
    let mut cmd = Command::new(bore_binary()?);
    cmd.arg("transfer")
        .arg("listener")
        .arg("--dest-path")
        .arg(dest_path)
        .arg("--to")
        .arg("localhost")
        .arg("--secret")
        .arg("transfer-secret")
        .arg("--transfer-id")
        .arg(transfer_id);
    if relay_only {
        cmd.arg("--relay-only");
    }
    if overwrite {
        cmd.arg("--overwrite");
    }
    if rename {
        cmd.arg("--rename");
    }
    if let Some(stun_server) = stun_server {
        cmd.arg("--stun-server").arg(stun_server);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
        .context("failed to spawn bore transfer listener subprocess")
}

fn sender_stdin_child(
    transfer_id: &str,
    output_name: Option<&Path>,
    relay_only: bool,
    stun_server: Option<&str>,
) -> Result<Child> {
    let mut cmd = Command::new(bore_binary()?);
    cmd.arg("transfer")
        .arg("sender")
        .arg("--source")
        .arg("stdin")
        .arg("--to")
        .arg("localhost")
        .arg("--secret")
        .arg("transfer-secret")
        .arg("--transfer-id")
        .arg(transfer_id);
    if let Some(output_name) = output_name {
        cmd.arg("--output").arg(output_name);
    }
    if relay_only {
        cmd.arg("--relay-only");
    }
    if let Some(stun_server) = stun_server {
        cmd.arg("--stun-server").arg(stun_server);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
        .context("failed to spawn bore transfer sender subprocess")
}

async fn wait_child_output(child: Child) -> Result<Output> {
    tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .context("failed to join subprocess wait task")?
        .context("failed to wait on subprocess")
}

fn output_text(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("stdout:\n{stdout}\nstderr:\n{stderr}")
}

fn expect_success(output: Output, label: &str) -> Result<Output> {
    if output.status.success() {
        Ok(output)
    } else {
        bail!(
            "{label} failed with status {}\n{}",
            output.status,
            output_text(&output)
        );
    }
}

fn patterned_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|index| ((index * 17) % 251) as u8).collect()
}

#[derive(Clone, Copy)]
enum ListenerCollisionMode {
    Fail,
    Overwrite,
    Rename,
}

async fn run_stdin_transfer(
    dest_root: &Path,
    transfer_id: &str,
    output_name: &Path,
    data: &[u8],
    relay_only: bool,
    collision: ListenerCollisionMode,
    stun_server: Option<&str>,
) -> Result<()> {
    let (overwrite, rename) = match collision {
        ListenerCollisionMode::Fail => (false, false),
        ListenerCollisionMode::Overwrite => (true, false),
        ListenerCollisionMode::Rename => (false, true),
    };
    let listener = listener_child(
        dest_root,
        transfer_id,
        relay_only,
        overwrite,
        rename,
        stun_server,
    )?;
    time::sleep(Duration::from_millis(200)).await;

    let mut sender = sender_stdin_child(transfer_id, Some(output_name), relay_only, stun_server)?;
    {
        let stdin = sender
            .stdin
            .take()
            .context("sender subprocess is missing stdin pipe")?;
        let payload = data.to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut stdin = stdin;
            std::io::Write::write_all(&mut stdin, &payload)
                .context("failed to write stdin payload to sender subprocess")?;
            Ok(())
        })
        .await
        .context("failed to join sender stdin writer task")??;
    }

    let sender_output = expect_success(
        wait_child_output(sender).await?,
        "transfer sender subprocess",
    )?;
    let listener_output = expect_success(
        wait_child_output(listener).await?,
        "transfer listener subprocess",
    )?;
    if !sender_output.stderr.is_empty() || !listener_output.stderr.is_empty() {
        let _ = (sender_output, listener_output);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_small_text_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-small-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-relay-small-{}", Uuid::new_v4());
    let payload = b"stdin relay small payload";

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new("small.txt"),
        payload,
        true,
        ListenerCollisionMode::Fail,
        None,
    )
    .await?;

    assert_eq!(read_file(&dest_root.join("small.txt")).await?, payload);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_empty_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-empty-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-relay-empty-{}", Uuid::new_v4());

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new("empty.bin"),
        b"",
        true,
        ListenerCollisionMode::Fail,
        None,
    )
    .await?;

    let metadata = fs::metadata(dest_root.join("empty.bin")).await?;
    assert_eq!(metadata.len(), 0);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_large_binary_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-large-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-relay-large-{}", Uuid::new_v4());
    let payload = patterned_bytes(900_123);

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new("large.bin"),
        &payload,
        true,
        ListenerCollisionMode::Fail,
        None,
    )
    .await?;

    assert_eq!(read_file(&dest_root.join("large.bin")).await?, payload);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_overwrite_existing_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-overwrite-dest");
    fs::create_dir_all(&dest_root).await?;
    fs::write(dest_root.join("overwrite.txt"), b"old data").await?;
    let transfer_id = format!("stdin-relay-overwrite-{}", Uuid::new_v4());
    let payload = b"new overwrite payload";

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new("overwrite.txt"),
        payload,
        true,
        ListenerCollisionMode::Overwrite,
        None,
    )
    .await?;

    assert_eq!(read_file(&dest_root.join("overwrite.txt")).await?, payload);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_rename_existing_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-rename-dest");
    fs::create_dir_all(&dest_root).await?;
    fs::write(dest_root.join("rename.txt"), b"old data").await?;
    let transfer_id = format!("stdin-relay-rename-{}", Uuid::new_v4());
    let payload = b"renamed payload";

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new("rename.txt"),
        payload,
        true,
        ListenerCollisionMode::Rename,
        None,
    )
    .await?;

    assert_eq!(read_file(&dest_root.join("rename.txt")).await?, b"old data");
    assert_eq!(read_file(&dest_root.join("rename (1).txt")).await?, payload);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_requires_output_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    let transfer_id = format!("stdin-missing-output-{}", Uuid::new_v4());
    let mut sender = sender_stdin_child(&transfer_id, None, true, None)?;
    {
        let stdin = sender
            .stdin
            .take()
            .context("sender subprocess is missing stdin pipe")?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut stdin = stdin;
            std::io::Write::write_all(&mut stdin, b"ignored")
                .context("failed to write stdin payload to sender subprocess")?;
            Ok(())
        })
        .await
        .context("failed to join sender stdin writer task")??;
    }
    let output = wait_child_output(sender).await?;
    if output.status.success() {
        bail!(
            "sender without --output unexpectedly succeeded\n{}",
            output_text(&output)
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.contains("--output is required") {
        bail!(
            "sender without --output failed for the wrong reason\n{}",
            output_text(&output)
        );
    }
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_small_text_over_direct_udp_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let dest_root = temp_path("stdin-udp-small-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-udp-small-{}", Uuid::new_v4());
    let payload = b"stdin udp payload";
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new("udp.txt"),
        payload,
        false,
        ListenerCollisionMode::Fail,
        Some(&stun),
    )
    .await?;

    assert_eq!(read_file(&dest_root.join("udp.txt")).await?, payload);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_non_utf8_output_name_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-nonutf8-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-relay-nonutf8-{}", Uuid::new_v4());
    let output_name = OsString::from_vec(vec![
        b's', b't', b'd', b'i', b'n', b'-', 0xff, b'.', b'b', b'i', b'n',
    ]);
    let payload = b"stdin non utf8 output";

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new(&output_name),
        payload,
        true,
        ListenerCollisionMode::Fail,
        None,
    )
    .await?;

    assert_eq!(read_file(&dest_root.join(&output_name)).await?, payload);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}
