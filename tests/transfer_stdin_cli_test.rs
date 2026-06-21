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
        .arg("--sources")
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

fn sender_filesystem_child(
    transfer_id: &str,
    source: &Path,
    relay_only: bool,
    stun_server: Option<&str>,
    parallel: Option<u16>,
) -> Result<Child> {
    let mut cmd = Command::new(bore_binary()?);
    cmd.arg("transfer")
        .arg("sender")
        .arg("--sources")
        .arg(source)
        .arg("--to")
        .arg("localhost")
        .arg("--secret")
        .arg("transfer-secret")
        .arg("--transfer-id")
        .arg(transfer_id);
    if relay_only {
        cmd.arg("--relay-only");
    }
    if let Some(stun_server) = stun_server {
        cmd.arg("--stun-server").arg(stun_server);
    }
    if let Some(parallel) = parallel {
        cmd.arg("--parallel").arg(parallel.to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
        .context("failed to spawn bore transfer filesystem sender subprocess")
}

async fn wait_child_output(child: Child) -> Result<Output> {
    tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .context("failed to join subprocess wait task")?
        .context("failed to wait on subprocess")
}

async fn kill_child_output(child: Child) -> Result<Output> {
    tokio::task::spawn_blocking(move || {
        let mut child = child;
        let _ = child.kill();
        child.wait_with_output()
    })
    .await
    .context("failed to join subprocess kill task")?
    .context("failed to kill/wait on subprocess")
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

#[cfg(windows)]
fn windows_sanitized_name(name: &str) -> PathBuf {
    PathBuf::from(format!("_bore_utf8_{}", hex::encode(name.as_bytes())))
}

#[derive(Clone, Copy)]
enum ListenerCollisionMode {
    Fail,
    Overwrite,
    Rename,
}

#[derive(Clone, Copy)]
struct TransferModes<'a> {
    listener_relay_only: bool,
    sender_relay_only: bool,
    listener_stun_server: Option<&'a str>,
    sender_stun_server: Option<&'a str>,
}

impl<'a> TransferModes<'a> {
    fn mirrored(relay_only: bool, stun_server: Option<&'a str>) -> Self {
        Self {
            listener_relay_only: relay_only,
            sender_relay_only: relay_only,
            listener_stun_server: stun_server,
            sender_stun_server: stun_server,
        }
    }
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
    run_stdin_transfer_with_modes(
        dest_root,
        transfer_id,
        output_name,
        data,
        collision,
        TransferModes::mirrored(relay_only, stun_server),
    )
    .await
}

async fn run_stdin_transfer_with_modes(
    dest_root: &Path,
    transfer_id: &str,
    output_name: &Path,
    data: &[u8],
    collision: ListenerCollisionMode,
    modes: TransferModes<'_>,
) -> Result<()> {
    let (overwrite, rename) = match collision {
        ListenerCollisionMode::Fail => (false, false),
        ListenerCollisionMode::Overwrite => (true, false),
        ListenerCollisionMode::Rename => (false, true),
    };
    let listener = listener_child(
        dest_root,
        transfer_id,
        modes.listener_relay_only,
        overwrite,
        rename,
        modes.listener_stun_server,
    )?;
    time::sleep(Duration::from_millis(200)).await;

    let mut sender = sender_stdin_child(
        transfer_id,
        Some(output_name),
        modes.sender_relay_only,
        modes.sender_stun_server,
    )?;
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

fn resume_state_dir(dest_root: &Path, transfer_id: &str) -> PathBuf {
    let digest = blake3::hash(transfer_id.as_bytes()).to_hex().to_string();
    dest_root.join(format!(".bore-transfer-state-{digest}"))
}

async fn completed_chunks_in_state(path: &Path) -> Result<usize> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).await?)?;
    let files = value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .context("resume state is missing files")?;
    Ok(files
        .iter()
        .filter_map(|file| file.get("completed"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter(|done| done.as_bool().unwrap_or(false))
        .count())
}

async fn wait_for_completed_chunks(
    state_file: &Path,
    final_path: &Path,
    minimum: usize,
) -> Result<()> {
    for _ in 0..1000 {
        if fs::try_exists(state_file).await?
            && completed_chunks_in_state(state_file).await? >= minimum
            && !fs::try_exists(final_path).await?
        {
            return Ok(());
        }
        time::sleep(Duration::from_millis(10)).await;
    }
    bail!(
        "timed out waiting for resume progress in {}",
        state_file.display()
    )
}

async fn resume_filesystem_transfer_with_retries(
    dest_root: &Path,
    transfer_id: &str,
    source_file: &Path,
) -> Result<()> {
    let mut last_failure = None;
    for attempt in 0..3 {
        let listener = listener_child(dest_root, transfer_id, true, false, false, None)?;
        time::sleep(Duration::from_millis(300)).await;
        let sender = sender_filesystem_child(transfer_id, source_file, true, None, Some(1))?;

        let sender_output = wait_child_output(sender).await?;
        let listener_output = wait_child_output(listener).await?;
        if sender_output.status.success() && listener_output.status.success() {
            return Ok(());
        }

        last_failure = Some(format!(
            "resume attempt {} failed\nsender:\n{}\nlistener:\n{}",
            attempt + 1,
            output_text(&sender_output),
            output_text(&listener_output)
        ));
        time::sleep(Duration::from_millis(200)).await;
    }

    bail!(
        "filesystem resume subprocesses did not recover after restart\n{}",
        last_failure.unwrap_or_else(|| "no subprocess output captured".to_string())
    )
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
async fn transfer_stdin_fail_existing_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-fail-dest");
    fs::create_dir_all(&dest_root).await?;
    fs::write(dest_root.join("fail.txt"), b"old data").await?;
    let transfer_id = format!("stdin-relay-fail-{}", Uuid::new_v4());
    let payload = b"should not overwrite";

    let listener = listener_child(&dest_root, &transfer_id, true, false, false, None)?;
    time::sleep(Duration::from_millis(200)).await;
    let mut sender = sender_stdin_child(&transfer_id, Some(Path::new("fail.txt")), true, None)?;
    {
        let stdin = sender
            .stdin
            .take()
            .context("sender subprocess is missing stdin pipe")?;
        let payload = payload.to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut stdin = stdin;
            std::io::Write::write_all(&mut stdin, &payload)
                .context("failed to write stdin payload to sender subprocess")?;
            Ok(())
        })
        .await
        .context("failed to join sender stdin writer task")??;
    }

    let sender_output = wait_child_output(sender).await?;
    let listener_output = wait_child_output(listener).await?;
    assert!(
        !sender_output.status.success(),
        "stdin sender should fail when destination exists\n{}",
        output_text(&sender_output)
    );
    assert!(
        !listener_output.status.success(),
        "stdin listener should fail when destination exists\n{}",
        output_text(&listener_output)
    );
    let combined = format!(
        "{}\n{}",
        output_text(&sender_output),
        output_text(&listener_output)
    );
    assert!(
        combined.contains("destination already exists"),
        "unexpected collision-fail output\n{combined}"
    );
    assert_eq!(read_file(&dest_root.join("fail.txt")).await?, b"old data");

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

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_small_text_falls_back_to_relay_when_listener_disables_direct_udp_cli(
) -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let dest_root = temp_path("stdin-udp-fallback-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-udp-fallback-{}", Uuid::new_v4());
    let payload = b"stdin udp fallback payload";
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    run_stdin_transfer_with_modes(
        &dest_root,
        &transfer_id,
        Path::new("fallback.txt"),
        payload,
        ListenerCollisionMode::Fail,
        TransferModes {
            listener_relay_only: true,
            sender_relay_only: false,
            listener_stun_server: None,
            sender_stun_server: Some(&stun),
        },
    )
    .await?;

    assert_eq!(read_file(&dest_root.join("fallback.txt")).await?, payload);
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_filesystem_listener_kill_resumes_and_cleans_up_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("filesystem-kill-source");
    let dest_root = temp_path("filesystem-kill-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    let source_file = source_root.join("resume.bin");
    let payload = patterned_bytes((256 * 1024 * 64) + 17);
    fs::write(&source_file, &payload).await?;
    let transfer_id = format!("filesystem-kill-{}", Uuid::new_v4());
    let final_path = dest_root.join("resume.bin");
    let state_dir = resume_state_dir(&dest_root, &transfer_id);
    let state_file = state_dir.join("state.json");

    let listener = listener_child(&dest_root, &transfer_id, true, false, false, None)?;
    time::sleep(Duration::from_millis(200)).await;
    let sender = sender_filesystem_child(&transfer_id, &source_file, true, None, Some(1))?;

    wait_for_completed_chunks(&state_file, &final_path, 2).await?;
    let killed_listener = kill_child_output(listener).await?;
    assert!(
        !killed_listener.status.success(),
        "listener kill should not look like a clean exit\n{}",
        output_text(&killed_listener)
    );

    let sender_output = wait_child_output(sender).await?;
    assert!(
        !sender_output.status.success(),
        "filesystem sender should fail after listener kill\n{}",
        output_text(&sender_output)
    );
    assert!(fs::try_exists(&state_file).await?);
    assert!(completed_chunks_in_state(&state_file).await? >= 2);
    assert!(!fs::try_exists(&final_path).await?);

    resume_filesystem_transfer_with_retries(&dest_root, &transfer_id, &source_file).await?;

    assert_eq!(read_file(&final_path).await?, payload);
    assert!(!fs::try_exists(&state_dir).await?);

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

// Linux-only: macOS (APFS/HFS+) rejects non-UTF-8 byte sequences in file
// names at the syscall layer (EILSEQ, os error 92) — "failed to create
// destination file stdin-<bytes>.bin". Byte-preserving names are a Linux
// filesystem capability.
#[cfg(target_os = "linux")]
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

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_reserved_windows_output_name_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-windows-reserved-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-relay-windows-reserved-{}", Uuid::new_v4());
    let payload = b"stdin reserved windows output";
    let requested = "CON.txt";

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new(requested),
        payload,
        true,
        ListenerCollisionMode::Fail,
        None,
    )
    .await?;

    assert_eq!(
        read_file(&dest_root.join(windows_sanitized_name(requested))).await?,
        payload
    );

    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_stdin_invalid_windows_char_output_name_over_relay_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("stdin-relay-windows-invalid-char-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("stdin-relay-windows-invalid-char-{}", Uuid::new_v4());
    let payload = b"stdin invalid windows char output";
    let requested = "bad:name.txt";

    run_stdin_transfer(
        &dest_root,
        &transfer_id,
        Path::new(requested),
        payload,
        true,
        ListenerCollisionMode::Fail,
        None,
    )
    .await?;

    assert_eq!(
        read_file(&dest_root.join(windows_sanitized_name(requested))).await?,
        payload
    );

    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

fn sender_multi_source_child(
    transfer_id: &str,
    sources: &[&Path],
    output: Option<&Path>,
    relay_only: bool,
) -> Result<Child> {
    let mut cmd = Command::new(bore_binary()?);
    cmd.arg("transfer").arg("sender");
    cmd.arg("--sources");
    for s in sources {
        cmd.arg(s);
    }
    cmd.arg("--to")
        .arg("localhost")
        .arg("--secret")
        .arg("transfer-secret")
        .arg("--transfer-id")
        .arg(transfer_id);
    if let Some(out) = output {
        cmd.arg("--output").arg(out);
    }
    if relay_only {
        cmd.arg("--relay-only");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn().context("failed to spawn sender subprocess")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_source_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("multi-source-cli-src");
    let dest_root = temp_path("multi-source-cli-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file1 = source_root.join("alpha.txt");
    let file2 = source_root.join("beta.txt");
    fs::write(&file1, b"alpha content").await?;
    fs::write(&file2, b"beta content").await?;

    let transfer_id = format!("multi-source-cli-{}", Uuid::new_v4());

    let listener = listener_child(&dest_root, &transfer_id, true, false, false, None)?;
    time::sleep(Duration::from_millis(200)).await;

    let sender = sender_multi_source_child(
        &transfer_id,
        &[file1.as_path(), file2.as_path()],
        Some(Path::new("bundle")),
        true,
    )?;

    expect_success(wait_child_output(sender).await?, "multi-source sender")?;
    expect_success(wait_child_output(listener).await?, "multi-source listener")?;

    assert_eq!(
        read_file(&dest_root.join("bundle/alpha.txt")).await?,
        b"alpha content"
    );
    assert_eq!(
        read_file(&dest_root.join("bundle/beta.txt")).await?,
        b"beta content"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_files_cli() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("source-files-cli-src");
    let dest_root = temp_path("source-files-cli-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file1 = source_root.join("hello.txt");
    let file2 = source_root.join("world.txt");
    fs::write(&file1, b"hello").await?;
    fs::write(&file2, b"world").await?;

    let list_content = format!(
        "# comment\n{}\n# another comment\n{}\n   \n",
        file1.display(),
        file2.display()
    );
    let list_file = source_root.join("list.txt");
    fs::write(&list_file, list_content.as_bytes()).await?;

    let transfer_id = format!("source-files-cli-{}", Uuid::new_v4());

    let listener = listener_child(&dest_root, &transfer_id, true, false, false, None)?;
    time::sleep(Duration::from_millis(200)).await;

    let mut cmd = Command::new(bore_binary()?);
    cmd.arg("transfer")
        .arg("sender")
        .arg("--source-files")
        .arg(&list_file)
        .arg("--output")
        .arg("bundle")
        .arg("--to")
        .arg("localhost")
        .arg("--secret")
        .arg("transfer-secret")
        .arg("--transfer-id")
        .arg(&transfer_id)
        .arg("--relay-only");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let sender = cmd.spawn().context("failed to spawn source-files sender")?;

    expect_success(wait_child_output(sender).await?, "source-files sender")?;
    expect_success(wait_child_output(listener).await?, "source-files listener")?;

    assert_eq!(
        read_file(&dest_root.join("bundle/hello.txt")).await?,
        b"hello"
    );
    assert_eq!(
        read_file(&dest_root.join("bundle/world.txt")).await?,
        b"world"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}
