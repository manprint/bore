use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use anyhow::{bail, Context, Result};
use bore_cli::{
    server::Server,
    shared::CONTROL_PORT,
    transfer::{CollisionPolicy, DeviceMode, ListenerOptions, SenderOptions, SymlinkMode},
    transport,
};
use lazy_static::lazy_static;
use rcgen::generate_simple_self_signed;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time;
use uuid::Uuid;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

/// RAII guard that removes an env var on drop (panic-safe cleanup).
struct EnvVarGuard(&'static str);
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

const TEST_CHUNK_SIZE: usize = 1024 * 1024;
const TEST_MANIFEST_CHUNK: usize = 128;
const TRANSFER_TLS_CONTROL_PORT: u16 = 17910;

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

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", port)).await.is_ok() == listening {
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

fn self_signed() -> Result<(String, String)> {
    let key = generate_simple_self_signed(["localhost".to_string()])?;
    Ok((key.cert.pem(), key.signing_key.serialize_pem()))
}

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("bore-transfer-test-{label}-{}", Uuid::new_v4()))
}

async fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, data).await?;
    Ok(())
}

async fn read_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    Ok(buf)
}

fn listener_options(
    transfer_id: String,
    dest_path: PathBuf,
    relay_only: bool,
    carriers: u16,
    stun_server: Option<String>,
) -> ListenerOptions {
    listener_options_with_collision(
        transfer_id,
        dest_path,
        relay_only,
        carriers,
        stun_server,
        CollisionPolicy::Fail,
    )
}

fn listener_options_with_collision(
    transfer_id: String,
    dest_path: PathBuf,
    relay_only: bool,
    carriers: u16,
    stun_server: Option<String>,
    collision: CollisionPolicy,
) -> ListenerOptions {
    ListenerOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        dest_path,
        relay_only,
        stun_server,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers,
        collision,
        persistent: false,
        ask_confirm: false,
        confirm_timeout: 120,
        stall_timeout: 0, // disabled in tests to avoid false timeouts
    }
}

fn sender_options(
    transfer_id: String,
    source: PathBuf,
    output: Option<PathBuf>,
    relay_only: bool,
    carriers: u16,
    parallel: u16,
    stun_server: Option<String>,
) -> SenderOptions {
    SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        sources: vec![source],
        source_files: vec![],
        ask_confirm: false,
        output,
        relay_only,
        stun_server,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers,
        parallel,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0, // disabled in tests to avoid false timeouts
    }
}

fn patterned_bytes(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn reserve_udp_port(exclude: Option<u16>) -> Result<u16> {
    for _ in 0..32 {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let port = socket.local_addr()?.port();
        if Some(port) != exclude {
            return Ok(port);
        }
    }
    bail!("failed to reserve a distinct UDP port")
}

#[cfg(unix)]
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|uid| uid.trim() == "0")
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_single_file_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("single-source");
    let dest_root = temp_path("single-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"hello transfer over relay").await?;

    let transfer_id = format!("relay-file-{}", Uuid::new_v4());
    let listener_dest = dest_root.clone();
    let listener_id = transfer_id.clone();
    let listener = tokio::spawn(async move {
        bore_cli::transfer::run_listener(ListenerOptions {
            to: "localhost".to_string(),
            secret: Some("transfer-secret".to_string()),
            insecure: false,
            transfer_id: Some(listener_id),
            dest_path: listener_dest,
            relay_only: true,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            nat_udp_preferred_port: 0,
            nat_udp_release_timeout: 0,
            carriers: 1,
            collision: CollisionPolicy::Fail,
            persistent: false,
            ask_confirm: false,
            confirm_timeout: 120,
            stall_timeout: 0,
        })
        .await
    });

    time::sleep(Duration::from_millis(200)).await;
    let sender = bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        sources: vec![source_file.clone()],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(!sender.transport.direct_udp);
    assert!(!listener.transport.direct_udp);
    let received = read_file(&dest_root.join("payload.txt")).await?;
    assert_eq!(received, b"hello transfer over relay");

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_zero_byte_file_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("zero-source");
    let dest_root = temp_path("zero-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("empty.bin");
    write_file(&source_file, b"").await?;

    let transfer_id = format!("zero-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;
    listener.await.context("listener task join failed")??;

    assert_eq!(
        read_file(&dest_root.join("empty.bin")).await?,
        Vec::<u8>::new()
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_file_size_boundaries_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    for (label, size) in [
        ("one-byte", 1usize),
        ("chunk-minus-one", TEST_CHUNK_SIZE - 1),
        ("chunk-exact", TEST_CHUNK_SIZE),
        ("chunk-plus-one", TEST_CHUNK_SIZE + 1),
        ("chunk-multiple-exact", TEST_CHUNK_SIZE * 2),
    ] {
        let source_root = temp_path(&format!("{label}-source"));
        let dest_root = temp_path(&format!("{label}-dest"));
        fs::create_dir_all(&source_root).await?;
        fs::create_dir_all(&dest_root).await?;
        let source_file = source_root.join("payload.bin");
        let payload = patterned_bytes(size);
        write_file(&source_file, &payload).await?;

        let transfer_id = format!("{label}-{}", Uuid::new_v4());
        let listener = tokio::spawn({
            let transfer_id = transfer_id.clone();
            let dest_root = dest_root.clone();
            async move {
                bore_cli::transfer::run_listener(listener_options(
                    transfer_id,
                    dest_root,
                    true,
                    1,
                    None,
                ))
                .await
            }
        });

        time::sleep(Duration::from_millis(200)).await;
        let sender = bore_cli::transfer::run_sender(sender_options(
            transfer_id,
            source_file,
            None,
            true,
            1,
            4,
            None,
        ))
        .await?;
        let listener = listener.await.context("listener task join failed")??;

        assert!(
            !sender.transport.direct_udp,
            "unexpected direct UDP for {label}"
        );
        assert!(
            !listener.transport.direct_udp,
            "unexpected direct UDP for {label}"
        );
        assert_eq!(
            read_file(&dest_root.join("payload.bin")).await?,
            payload,
            "payload mismatch for {label}"
        );

        let _ = fs::remove_dir_all(&source_root).await;
        let _ = fs::remove_dir_all(&dest_root).await;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_manifest_spans_multiple_frames_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_parent = temp_path("manifest-parent");
    let source_root = source_parent.join("bundle");
    let dest_root = temp_path("manifest-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    for index in 0..=(TEST_MANIFEST_CHUNK + 1) {
        let name = format!("file-{index:03}.bin");
        write_file(&source_root.join(&name), &[index as u8]).await?;
    }

    let transfer_id = format!("manifest-dir-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_root.clone(),
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;
    listener.await.context("listener task join failed")??;

    for index in 0..=(TEST_MANIFEST_CHUNK + 1) {
        let name = format!("file-{index:03}.bin");
        assert_eq!(
            read_file(&dest_root.join("bundle").join(&name)).await?,
            vec![index as u8],
            "manifest-chunked file missing or corrupted: {name}"
        );
    }

    let _ = fs::remove_dir_all(&source_parent).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_fail_existing_file_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("fail-existing-source");
    let dest_root = temp_path("fail-existing-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"new payload").await?;
    write_file(&dest_root.join("payload.txt"), b"old payload").await?;

    let transfer_id = format!("fail-existing-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    let sender_err = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await
    .expect_err("transfer should fail when destination already exists");
    assert!(
        sender_err
            .to_string()
            .contains("destination already exists"),
        "unexpected sender error: {sender_err}"
    );
    let listener_err = listener
        .await
        .context("listener task join failed")?
        .expect_err("listener should reject an existing destination");
    assert!(
        listener_err
            .to_string()
            .contains("destination already exists"),
        "unexpected listener error: {listener_err}"
    );
    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"old payload"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_overwrite_existing_file_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("overwrite-file-source");
    let dest_root = temp_path("overwrite-file-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"new transfer payload").await?;
    write_file(&dest_root.join("payload.txt"), b"old destination payload").await?;

    let transfer_id = format!("overwrite-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options_with_collision(
                transfer_id,
                dest_root,
                true,
                1,
                None,
                CollisionPolicy::Overwrite,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;
    listener.await.context("listener task join failed")??;

    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"new transfer payload"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_rename_existing_file_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("rename-file-source");
    let dest_root = temp_path("rename-file-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"new renamed payload").await?;
    write_file(&dest_root.join("payload.txt"), b"old destination payload").await?;

    let transfer_id = format!("rename-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options_with_collision(
                transfer_id,
                dest_root,
                true,
                1,
                None,
                CollisionPolicy::Rename,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;
    listener.await.context("listener task join failed")??;

    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"old destination payload"
    );
    assert_eq!(
        read_file(&dest_root.join("payload (1).txt")).await?,
        b"new renamed payload"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_directory_preserves_structure() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_parent = temp_path("dir-parent");
    let source_root = source_parent.join("tree");
    let dest_root = temp_path("dir-dest");
    fs::create_dir_all(source_root.join("nested/deeper")).await?;
    fs::create_dir_all(&dest_root).await?;
    write_file(&source_root.join("root.txt"), b"root-content").await?;
    write_file(
        &source_root.join("nested/deeper/data.bin"),
        b"nested-content",
    )
    .await?;

    #[cfg(unix)]
    std::os::unix::fs::symlink("nested/deeper/data.bin", source_root.join("link.bin"))?;

    let transfer_id = format!("relay-dir-{}", Uuid::new_v4());
    let listener_dest = dest_root.clone();
    let listener_id = transfer_id.clone();
    let listener = tokio::spawn(async move {
        bore_cli::transfer::run_listener(ListenerOptions {
            to: "localhost".to_string(),
            secret: Some("transfer-secret".to_string()),
            insecure: false,
            transfer_id: Some(listener_id),
            dest_path: listener_dest,
            relay_only: true,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            nat_udp_preferred_port: 0,
            nat_udp_release_timeout: 0,
            carriers: 1,
            collision: CollisionPolicy::Fail,
            persistent: false,
            ask_confirm: false,
            confirm_timeout: 120,
            stall_timeout: 0,
        })
        .await
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        sources: vec![source_root.clone()],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Include,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    listener.await.context("listener task join failed")??;

    assert_eq!(
        read_file(&dest_root.join("tree/root.txt")).await?,
        b"root-content"
    );
    assert_eq!(
        read_file(&dest_root.join("tree/nested/deeper/data.bin")).await?,
        b"nested-content"
    );

    #[cfg(unix)]
    {
        let target = fs::read_link(dest_root.join("tree/link.bin")).await?;
        assert_eq!(target, PathBuf::from("nested/deeper/data.bin"));
    }

    let _ = fs::remove_dir_all(&source_parent).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_directory_excludes_symlinks_when_requested() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_parent = temp_path("dir-exclude-parent");
    let source_root = source_parent.join("tree");
    let dest_root = temp_path("dir-exclude-dest");
    fs::create_dir_all(source_root.join("nested")).await?;
    fs::create_dir_all(&dest_root).await?;
    write_file(&source_root.join("nested/data.bin"), b"nested-content").await?;
    std::os::unix::fs::symlink("nested/data.bin", source_root.join("link.bin"))?;

    let transfer_id = format!("relay-dir-exclude-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    let mut options = sender_options(transfer_id, source_root.clone(), None, true, 1, 0, None);
    options.symlinks = SymlinkMode::Exclude;
    bore_cli::transfer::run_sender(options).await?;
    listener.await.context("listener task join failed")??;

    assert_eq!(
        read_file(&dest_root.join("tree/nested/data.bin")).await?,
        b"nested-content"
    );
    assert!(!fs::try_exists(dest_root.join("tree/link.bin")).await?);

    let _ = fs::remove_dir_all(&source_parent).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_root_symlink_is_rejected_when_symlinks_are_excluded() -> Result<()> {
    let source_root = temp_path("root-symlink-source");
    fs::create_dir_all(&source_root).await?;
    write_file(&source_root.join("target.txt"), b"target").await?;
    let source_link = source_root.join("target-link.txt");
    std::os::unix::fs::symlink("target.txt", &source_link)?;

    let transfer_id = format!("root-symlink-{}", Uuid::new_v4());
    let sender_err = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_link,
        None,
        true,
        1,
        0,
        None,
    ))
    .await
    .expect_err("root symlink should be rejected when symlinks are excluded");
    assert!(
        sender_err
            .to_string()
            .contains("symlink but --symlinks=exclude"),
        "unexpected sender error: {sender_err}"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_root_device_is_rejected_when_devices_are_excluded() -> Result<()> {
    let transfer_id = format!("root-device-exclude-{}", Uuid::new_v4());
    let sender_err = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        PathBuf::from("/dev/null"),
        None,
        true,
        1,
        0,
        None,
    ))
    .await
    .expect_err("root device should be rejected when devices are excluded");
    assert!(
        sender_err
            .to_string()
            .contains("device but --devices=exclude"),
        "unexpected sender error: {sender_err}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_root_device_honors_devices_include_over_relay() -> Result<()> {
    use std::os::unix::fs::FileTypeExt;

    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("device-include-dest");
    fs::create_dir_all(&dest_root).await?;
    let transfer_id = format!("device-include-{}", Uuid::new_v4());

    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    let mut options = sender_options(
        transfer_id,
        PathBuf::from("/dev/null"),
        None,
        true,
        1,
        0,
        None,
    );
    options.devices = DeviceMode::Include;
    let sender = bore_cli::transfer::run_sender(options).await;
    let listener_result = listener.await.context("listener task join failed")?;

    if running_as_root() {
        let sender = sender?;
        let listener = listener_result?;
        assert!(!sender.transport.direct_udp);
        assert!(!listener.transport.direct_udp);

        let metadata = fs::metadata(dest_root.join("null")).await?;
        assert!(
            metadata.file_type().is_char_device(),
            "expected a character device at the destination"
        );
    } else {
        let sender_err = sender.expect_err("device include should fail without privileges");
        let listener_err = listener_result.expect_err("listener should fail without privileges");
        let combined = format!("{sender_err}\n{listener_err}");
        assert!(
            combined.contains("failed to create device")
                || combined.contains("Operation not permitted")
                || combined.contains("permission denied"),
            "unexpected device-include failure: {combined}"
        );
        assert!(!fs::try_exists(dest_root.join("null")).await?);
    }

    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_large_file_parallel_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("parallel-source");
    let dest_root = temp_path("parallel-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("large.bin");
    let payload = patterned_bytes(1_500_321);
    write_file(&source_file, &payload).await?;

    let transfer_id = format!("parallel-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    let sender = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        4,
        4,
        None,
    ))
    .await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(!sender.transport.direct_udp);
    assert!(!listener.transport.direct_udp);
    assert_eq!(read_file(&dest_root.join("large.bin")).await?, payload);

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

/// Fix A: `--carriers 0` (auto) must transparently scale the relay carrier pool to the
/// worker parallelism and still deliver the file intact over the multi-carrier relay path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_auto_carriers_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("auto-carriers-source");
    let dest_root = temp_path("auto-carriers-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("auto.bin");
    // Several chunks so multiple workers (and thus multiple auto-allocated carriers) are used.
    let payload = patterned_bytes(5_000_000);
    write_file(&source_file, &payload).await?;

    let transfer_id = format!("auto-carriers-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            // carriers = 0 → auto on the listener.
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                0,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    // carriers = 0 → auto on the sender; parallel = 6 workers.
    let sender = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        0,
        6,
        None,
    ))
    .await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(!sender.transport.direct_udp);
    assert!(!listener.transport.direct_udp);
    assert_eq!(read_file(&dest_root.join("auto.bin")).await?, payload);

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_small_file_parallel_over_relay_when_workers_exceed_chunks() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("parallel-small-source");
    let dest_root = temp_path("parallel-small-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("single-chunk.bin");
    let payload = patterned_bytes(73_531);
    write_file(&source_file, &payload).await?;

    let transfer_id = format!("parallel-small-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    let sender = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        4,
        4,
        None,
    ))
    .await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(!sender.transport.direct_udp);
    assert!(!listener.transport.direct_udp);
    assert_eq!(
        read_file(&dest_root.join("single-chunk.bin")).await?,
        payload
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_resume_large_file_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("resume-source");
    let dest_root = temp_path("resume-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("resume.bin");
    let payload = patterned_bytes(2_100_777);
    write_file(&source_file, &payload).await?;

    let transfer_id = format!("resume-file-{}", Uuid::new_v4());
    let first_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    std::env::set_var("BORE_TRANSFER_TEST_MAX_CHUNKS", "2");
    let interrupted = bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        source_file.clone(),
        None,
        true,
        4,
        4,
        None,
    ))
    .await;
    std::env::remove_var("BORE_TRANSFER_TEST_MAX_CHUNKS");
    assert!(
        interrupted.is_err(),
        "first sender run should be interrupted"
    );
    assert!(
        first_listener
            .await
            .context("listener task join failed")?
            .is_err(),
        "first listener run should observe the interrupted transfer"
    );

    let second_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        4,
        4,
        None,
    ))
    .await?;
    second_listener
        .await
        .context("listener task join failed")??;

    assert_eq!(read_file(&dest_root.join("resume.bin")).await?, payload);

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_resume_rejects_changed_manifest_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("resume-mismatch-source");
    let dest_root = temp_path("resume-mismatch-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("resume.bin");
    write_file(&source_file, &patterned_bytes(1_400_333)).await?;

    let transfer_id = format!("resume-mismatch-{}", Uuid::new_v4());
    let first_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    std::env::set_var("BORE_TRANSFER_TEST_MAX_CHUNKS", "2");
    let interrupted = bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        source_file.clone(),
        None,
        true,
        4,
        4,
        None,
    ))
    .await;
    std::env::remove_var("BORE_TRANSFER_TEST_MAX_CHUNKS");
    assert!(
        interrupted.is_err(),
        "first sender run should be interrupted"
    );
    assert!(
        first_listener
            .await
            .context("listener task join failed")?
            .is_err(),
        "first listener run should observe the interrupted transfer"
    );

    write_file(&source_file, &patterned_bytes(1_410_777)).await?;

    let second_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    let retry = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        4,
        4,
        None,
    ))
    .await;
    let retry_err = retry.expect_err("resume with a changed manifest should fail");
    assert!(
        retry_err
            .to_string()
            .contains("does not match the current manifest"),
        "unexpected retry error: {retry_err}"
    );
    assert!(
        second_listener
            .await
            .context("listener task join failed")?
            .is_err(),
        "second listener run should reject the changed manifest"
    );
    assert!(!fs::try_exists(dest_root.join("resume.bin")).await?);

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_non_utf8_file_name_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("nonutf8-file-source");
    let dest_root = temp_path("nonutf8-file-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file_name = OsString::from_vec(vec![
        b'p', b'a', b'y', b'l', b'o', b'a', b'd', b'-', 0xff, b'.', b'b', b'i', b'n',
    ]);
    let source_file = source_root.join(&file_name);
    write_file(&source_file, b"non utf8 file name").await?;

    let transfer_id = format!("nonutf8-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;
    listener.await.context("listener task join failed")??;

    assert_eq!(
        read_file(&dest_root.join(&file_name)).await?,
        b"non utf8 file name"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_non_utf8_nested_directory_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_parent = temp_path("nonutf8-dir-parent");
    let source_root = source_parent.join("tree");
    let dest_root = temp_path("nonutf8-dir-dest");
    let nested = OsString::from_vec(vec![b'd', b'i', b'r', b'-', 0xfe]);
    let nested_file = OsString::from_vec(vec![b'f', b'i', b'l', b'e', b'-', 0xfd]);
    fs::create_dir_all(source_root.join(&nested)).await?;
    fs::create_dir_all(&dest_root).await?;
    write_file(
        &source_root.join(&nested).join(&nested_file),
        b"nested non utf8",
    )
    .await?;

    let transfer_id = format!("nonutf8-dir-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_root.clone(),
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;
    listener.await.context("listener task join failed")??;

    assert_eq!(
        read_file(&dest_root.join("tree").join(&nested).join(&nested_file)).await?,
        b"nested non utf8"
    );

    let _ = fs::remove_dir_all(&source_parent).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_single_file_over_direct_udp() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let source_root = temp_path("udp-source");
    let dest_root = temp_path("udp-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"hello transfer over udp").await?;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let transfer_id = format!("udp-file-{}", Uuid::new_v4());
    let listener_dest = dest_root.clone();
    let listener_id = transfer_id.clone();
    let stun_listener = stun.clone();
    let listener = tokio::spawn(async move {
        bore_cli::transfer::run_listener(ListenerOptions {
            to: "localhost".to_string(),
            secret: Some("transfer-secret".to_string()),
            insecure: false,
            transfer_id: Some(listener_id),
            dest_path: listener_dest,
            relay_only: false,
            stun_server: Some(stun_listener),
            upnp: false,
            try_port_prediction: false,
            nat_udp_preferred_port: 0,
            nat_udp_release_timeout: 0,
            carriers: 1,
            collision: CollisionPolicy::Fail,
            persistent: false,
            ask_confirm: false,
            confirm_timeout: 120,
            stall_timeout: 0,
        })
        .await
    });

    time::sleep(Duration::from_millis(300)).await;
    let sender = bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        sources: vec![source_file.clone()],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: false,
        stun_server: Some(stun),
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(
        sender.transport.direct_udp,
        "sender should negotiate direct UDP"
    );
    assert!(
        listener.transport.direct_udp,
        "listener should report direct UDP"
    );
    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"hello transfer over udp"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_single_file_over_direct_udp_with_nat_flags_enabled() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let listener_udp_port = reserve_udp_port(None)?;
    let sender_udp_port = reserve_udp_port(Some(listener_udp_port))?;
    let source_root = temp_path("udp-nat-flags-source");
    let dest_root = temp_path("udp-nat-flags-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"hello transfer with nat flags").await?;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let transfer_id = format!("udp-nat-flags-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        let stun_listener = stun.clone();
        async move {
            let mut options =
                listener_options(transfer_id, dest_root, false, 1, Some(stun_listener));
            options.upnp = true;
            options.try_port_prediction = true;
            options.nat_udp_preferred_port = listener_udp_port;
            options.nat_udp_release_timeout = 1;
            bore_cli::transfer::run_listener(options).await
        }
    });

    time::sleep(Duration::from_millis(300)).await;
    let mut options = sender_options(transfer_id, source_file, None, false, 1, 0, Some(stun));
    options.upnp = true;
    options.try_port_prediction = true;
    options.nat_udp_preferred_port = sender_udp_port;
    options.nat_udp_release_timeout = 1;
    let sender = bore_cli::transfer::run_sender(options).await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(sender.transport.direct_udp);
    assert!(listener.transport.direct_udp);
    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"hello transfer with nat flags"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_single_file_over_tls_control_with_insecure() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    wait_port(TRANSFER_TLS_CONTROL_PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("transfer-secret"));
    server.set_control_port(TRANSFER_TLS_CONTROL_PORT);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(TRANSFER_TLS_CONTROL_PORT, true).await;

    let source_root = temp_path("tls-source");
    let dest_root = temp_path("tls-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"hello transfer over tls control").await?;
    let to = format!("https://localhost:{TRANSFER_TLS_CONTROL_PORT}");

    let transfer_id = format!("tls-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        let to = to.clone();
        async move {
            let mut options = listener_options(transfer_id, dest_root, true, 1, None);
            options.to = to;
            options.insecure = true;
            bore_cli::transfer::run_listener(options).await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    let mut options = sender_options(transfer_id, source_file, None, true, 1, 0, None);
    options.to = to;
    options.insecure = true;
    let sender = bore_cli::transfer::run_sender(options).await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(!sender.transport.direct_udp);
    assert!(sender.transport.relay_tls);
    assert!(!listener.transport.direct_udp);
    assert!(listener.transport.relay_tls);
    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"hello transfer over tls control"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_single_file_falls_back_to_relay_when_listener_disables_direct_udp() -> Result<()>
{
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let source_root = temp_path("udp-fallback-source");
    let dest_root = temp_path("udp-fallback-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"hello transfer fallback").await?;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let transfer_id = format!("udp-fallback-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(300)).await;
    let sender = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        false,
        1,
        0,
        Some(stun),
    ))
    .await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(
        !sender.transport.direct_udp,
        "sender should fall back to relay when UDP is unavailable"
    );
    assert!(
        !listener.transport.direct_udp,
        "listener should report relay fallback when UDP is unavailable"
    );
    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"hello transfer fallback"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_single_file_falls_back_to_relay_with_nat_flags_enabled() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let sender_udp_port = reserve_udp_port(None)?;
    let source_root = temp_path("udp-nat-fallback-source");
    let dest_root = temp_path("udp-nat-fallback-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("payload.txt");
    write_file(&source_file, b"hello transfer nat fallback").await?;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let transfer_id = format!("udp-nat-fallback-file-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(300)).await;
    let mut options = sender_options(transfer_id, source_file, None, false, 1, 0, Some(stun));
    options.upnp = true;
    options.try_port_prediction = true;
    options.nat_udp_preferred_port = sender_udp_port;
    options.nat_udp_release_timeout = 1;
    let sender = bore_cli::transfer::run_sender(options).await?;
    let listener = listener.await.context("listener task join failed")??;

    assert!(!sender.transport.direct_udp);
    assert!(!listener.transport.direct_udp);
    assert_eq!(
        read_file(&dest_root.join("payload.txt")).await?,
        b"hello transfer nat fallback"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_resume_large_file_over_direct_udp() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let source_root = temp_path("resume-udp-source");
    let dest_root = temp_path("resume-udp-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("resume.bin");
    let payload = patterned_bytes(2_100_777);
    write_file(&source_file, &payload).await?;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let transfer_id = format!("resume-udp-file-{}", Uuid::new_v4());
    let first_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        let stun_listener = stun.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                false,
                1,
                Some(stun_listener),
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(300)).await;
    std::env::set_var("BORE_TRANSFER_TEST_MAX_CHUNKS", "2");
    let interrupted = bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        source_file.clone(),
        None,
        false,
        1,
        4,
        Some(stun.clone()),
    ))
    .await;
    std::env::remove_var("BORE_TRANSFER_TEST_MAX_CHUNKS");
    assert!(
        interrupted.is_err(),
        "first sender run should be interrupted"
    );
    assert!(
        first_listener
            .await
            .context("listener task join failed")?
            .is_err(),
        "first listener run should observe the interrupted transfer"
    );

    let second_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        let stun_listener = stun.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                false,
                1,
                Some(stun_listener),
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(300)).await;
    let sender = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        false,
        1,
        4,
        Some(stun),
    ))
    .await?;
    let listener = second_listener
        .await
        .context("listener task join failed")??;

    assert!(sender.transport.direct_udp);
    assert!(listener.transport.direct_udp);
    assert_eq!(read_file(&dest_root.join("resume.bin")).await?, payload);

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_resume_large_file_over_udp_request_fallback_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;

    let source_root = temp_path("resume-fallback-source");
    let dest_root = temp_path("resume-fallback-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let source_file = source_root.join("resume.bin");
    let payload = patterned_bytes(2_100_777);
    write_file(&source_file, &payload).await?;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let transfer_id = format!("resume-fallback-file-{}", Uuid::new_v4());
    let first_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(300)).await;
    std::env::set_var("BORE_TRANSFER_TEST_MAX_CHUNKS", "2");
    let interrupted = bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        source_file.clone(),
        None,
        false,
        4,
        4,
        Some(stun.clone()),
    ))
    .await;
    std::env::remove_var("BORE_TRANSFER_TEST_MAX_CHUNKS");
    assert!(
        interrupted.is_err(),
        "first sender run should be interrupted"
    );
    assert!(
        first_listener
            .await
            .context("listener task join failed")?
            .is_err(),
        "first listener run should observe the interrupted transfer"
    );

    let second_listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                4,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(300)).await;
    let sender = bore_cli::transfer::run_sender(sender_options(
        transfer_id,
        source_file,
        None,
        false,
        4,
        4,
        Some(stun),
    ))
    .await?;
    let listener = second_listener
        .await
        .context("listener task join failed")??;

    assert!(
        !sender.transport.direct_udp,
        "sender should resume over relay fallback when direct UDP is unavailable"
    );
    assert!(
        !listener.transport.direct_udp,
        "listener should report relay fallback during resumed transfer"
    );
    assert_eq!(read_file(&dest_root.join("resume.bin")).await?, payload);

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_multi_source_files_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("multi-source");
    let dest_root = temp_path("multi-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file1 = source_root.join("alpha.txt");
    let file2 = source_root.join("beta.txt");
    write_file(&file1, b"alpha content").await?;
    write_file(&file2, b"beta content").await?;

    let transfer_id = format!("multi-relay-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        sources: vec![file1, file2],
        source_files: vec![],
        ask_confirm: false,
        output: Some(PathBuf::from("bundle")),
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    listener.await.context("listener task join failed")??;

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
async fn transfer_persistent_listener_two_sequential_transfers() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("persist-source");
    let dest_root = temp_path("persist-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file1 = source_root.join("first.txt");
    let file2 = source_root.join("second.txt");
    write_file(&file1, b"first transfer").await?;
    write_file(&file2, b"second transfer").await?;

    let transfer_id = format!("persist-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(ListenerOptions {
                to: "localhost".to_string(),
                secret: Some("transfer-secret".to_string()),
                insecure: false,
                transfer_id: Some(transfer_id),
                dest_path: dest_root,
                relay_only: true,
                stun_server: None,
                upnp: false,
                try_port_prediction: false,
                nat_udp_preferred_port: 0,
                nat_udp_release_timeout: 0,
                carriers: 1,
                collision: CollisionPolicy::Fail,
                persistent: true,
                ask_confirm: false,
                confirm_timeout: 120,
                stall_timeout: 0,
            })
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;

    // First transfer
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        sources: vec![file1],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    assert_eq!(
        read_file(&dest_root.join("first.txt")).await?,
        b"first transfer"
    );

    // Give listener time to reset
    time::sleep(Duration::from_millis(300)).await;

    // Second transfer
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        sources: vec![file2],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    assert_eq!(
        read_file(&dest_root.join("second.txt")).await?,
        b"second transfer"
    );

    // Kill listener (it's persistent, never exits on its own)
    listener.abort();
    let _ = listener.await;

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_source_files_flag() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("srcfiles-source");
    let dest_root = temp_path("srcfiles-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file1 = source_root.join("hello.txt");
    let file2 = source_root.join("world.txt");
    write_file(&file1, b"hello").await?;
    write_file(&file2, b"world").await?;

    let list_file = source_root.join("list.txt");
    let list_content = format!(
        "# this is a comment\n{}\n# another comment\n{}\n   \n",
        file1.display(),
        file2.display()
    );
    fs::write(&list_file, list_content.as_bytes()).await?;

    let transfer_id = format!("srcfiles-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        sources: vec![],
        source_files: vec![list_file],
        ask_confirm: false,
        output: Some(PathBuf::from("bundle")),
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    listener.await.context("listener task join failed")??;

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

/// A re-run of the same sender (same transfer_id + same content) must succeed idempotently
/// rather than failing with "destination already exists" (F3 committed-marker fix).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_idempotent_recompletion_after_commit() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("idemp-source");
    let dest_root = temp_path("idemp-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file = source_root.join("data.txt");
    write_file(&file, b"hello idempotent").await?;

    let transfer_id = format!("idemp-{}", Uuid::new_v4());

    // Persistent listener so the second sender can connect after the first completes.
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            let mut opts = listener_options(transfer_id, dest_root, true, 1, None);
            opts.persistent = true;
            bore_cli::transfer::run_listener(opts).await
        }
    });
    time::sleep(Duration::from_millis(200)).await;

    // First send succeeds normally.
    bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        file.clone(),
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;

    time::sleep(Duration::from_millis(300)).await;

    // Second send with identical content + same transfer_id must succeed (idempotent
    // re-completion via the committed marker — not a false collision).
    bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        file.clone(),
        None,
        true,
        1,
        0,
        None,
    ))
    .await
    .expect("idempotent re-run must succeed, not fail with collision");

    // File in dest must still contain the original content.
    let content = read_file(&dest_root.join("data.txt")).await?;
    assert_eq!(
        content, b"hello idempotent",
        "file content must be unchanged"
    );

    listener.abort();
    let _ = listener.await;
    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

/// A committed marker with a different manifest_hash (stale marker) must be ignored
/// and the transfer must proceed normally — here triggering a real collision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_committed_marker_mismatch_starts_fresh() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("idemp-stale-source");
    let dest_root = temp_path("idemp-stale-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file = source_root.join("data.txt");
    write_file(&file, b"original content").await?;

    let transfer_id = format!("idemp-stale-{}", Uuid::new_v4());

    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            let mut opts = listener_options(transfer_id, dest_root, true, 1, None);
            opts.persistent = true;
            bore_cli::transfer::run_listener(opts).await
        }
    });
    time::sleep(Duration::from_millis(200)).await;

    // First send succeeds — committed marker is written with hash H1.
    bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        file.clone(),
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;

    time::sleep(Duration::from_millis(300)).await;

    // Change the file so the manifest hash differs from the committed marker (stale).
    write_file(&file, b"different content").await?;

    // Second send has a different manifest → stale marker removed → normal collision check
    // → data.txt already exists in dest_root → CollisionPolicy::Fail → error.
    let result = bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        file.clone(),
        None,
        true,
        1,
        0,
        None,
    ))
    .await;
    assert!(
        result.is_err(),
        "stale committed marker must not suppress a real collision"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("destination already exists") || err.contains("collision"),
        "error must mention destination conflict, got: {err}"
    );

    // Original file content is preserved.
    let content = read_file(&dest_root.join("data.txt")).await?;
    assert_eq!(
        content, b"original content",
        "original committed content must be preserved"
    );

    listener.abort();
    let _ = listener.await;
    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_persistent_listener_collision_continues() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("persist-coll-source");
    let dest_root = temp_path("persist-coll-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;
    let file = source_root.join("data.txt");
    write_file(&file, b"content").await?;

    let transfer_id = format!("persist-coll-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(ListenerOptions {
                to: "localhost".to_string(),
                secret: Some("transfer-secret".to_string()),
                insecure: false,
                transfer_id: Some(transfer_id),
                dest_path: dest_root,
                relay_only: true,
                stun_server: None,
                upnp: false,
                try_port_prediction: false,
                nat_udp_preferred_port: 0,
                nat_udp_release_timeout: 0,
                carriers: 1,
                collision: CollisionPolicy::Fail,
                persistent: true,
                ask_confirm: false,
                confirm_timeout: 120,
                stall_timeout: 0,
            })
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;

    // First transfer succeeds
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        sources: vec![file.clone()],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;

    time::sleep(Duration::from_millis(300)).await;

    // Mutate the file so its manifest hash differs from the committed marker written by
    // the first transfer. This triggers the stale-marker removal path, and the normal
    // CollisionPolicy::Fail check fires because data.txt already exists in dest_root.
    write_file(&file, b"different content").await?;

    // Second transfer fails (collision) — sender gets error, listener stays up
    let second_result = bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        sources: vec![file.clone()],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await;
    assert!(
        second_result.is_err(),
        "second transfer should fail due to collision"
    );

    // Wait longer than the persistent listener's error-drain window (500ms)
    time::sleep(Duration::from_millis(700)).await;

    // Listener should still be alive — verify by sending a different file
    let file3 = source_root.join("new_data.txt");
    write_file(&file3, b"new content").await?;
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        sources: vec![file3],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;

    listener.abort();
    let _ = listener.await;

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

// ── Bug regressions ──────────────────────────────────────────────────────────

/// Bug 002: multi-source without --output must place each source directly in
/// dest_root, not inside a wrapper directory named after the first source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_multi_source_flat_no_output_over_relay() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("multi-flat-source");
    let dest_root = temp_path("multi-flat-dest");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    let file1 = source_root.join("alpha.txt");
    write_file(&file1, b"alpha content").await?;

    let dir1 = source_root.join("sub_dir");
    fs::create_dir_all(&dir1).await?;
    write_file(&dir1.join("nested.txt"), b"nested content").await?;

    let file2 = source_root.join("beta.txt");
    write_file(&file2, b"beta content").await?;

    let transfer_id = format!("multi-flat-{}", Uuid::new_v4());
    let listener = tokio::spawn({
        let transfer_id = transfer_id.clone();
        let dest_root = dest_root.clone();
        async move {
            bore_cli::transfer::run_listener(listener_options(
                transfer_id,
                dest_root,
                true,
                1,
                None,
            ))
            .await
        }
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        sources: vec![file1, dir1, file2],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await?;
    listener.await.context("listener task join failed")??;

    // Each source must land directly in dest_root, not inside a wrapper dir.
    assert_eq!(
        read_file(&dest_root.join("alpha.txt")).await?,
        b"alpha content",
        "alpha.txt must be at dest_root level"
    );
    assert_eq!(
        read_file(&dest_root.join("beta.txt")).await?,
        b"beta content",
        "beta.txt must be at dest_root level"
    );
    assert_eq!(
        read_file(&dest_root.join("sub_dir/nested.txt")).await?,
        b"nested content",
        "sub_dir/nested.txt must be at dest_root/sub_dir level"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

/// Bug 003: when no listener is running, the sender must exit with a helpful
/// error message rather than the generic "unexpected EOF on transfer stream".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_sender_fails_with_helpful_message_when_no_listener() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let source_root = temp_path("no-listener-source");
    fs::create_dir_all(&source_root).await?;
    let source_file = source_root.join("data.txt");
    write_file(&source_file, b"payload").await?;

    let transfer_id = format!("no-listener-{}", Uuid::new_v4());
    // Intentionally do NOT start a listener.
    let err = bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await
    .expect_err("sender must fail when no listener is running");

    let err_str = format!("{err}");
    assert!(
        err_str.contains("listener did not respond") || err_str.contains("transfer listener"),
        "error should mention the listener, got: {err_str}"
    );
    assert!(
        !err_str.contains("unexpected EOF on transfer stream"),
        "must not expose the raw EOF error to the user, got: {err_str}"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    Ok(())
}

/// Bug 001: --ask-confirm must not silently cancel when stdin is not a tty.
/// In non-interactive test environments it must return a clear error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_ask_confirm_returns_err_when_no_tty_available() -> Result<()> {
    // Skip when stdin IS a real terminal: we cannot test the non-tty path then.
    if std::io::stdin().is_terminal() {
        return Ok(());
    }

    let source_root = temp_path("ask-confirm-src");
    fs::create_dir_all(&source_root).await?;
    let source_file = source_root.join("file.txt");
    write_file(&source_file, b"data").await?;

    let result = bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: None,
        insecure: true,
        transfer_id: Some("ask-confirm-test".to_string()),
        sources: vec![source_file],
        source_files: vec![],
        ask_confirm: true,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 0,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await;

    let err = result.expect_err("must fail when no tty is available");
    let err_str = format!("{err:#}");
    // After the fix: error explains the tty requirement.
    // Before the fix: Ok(false) → "transfer cancelled by user" (misleading).
    assert!(
        err_str.contains("terminal") || err_str.contains("tty"),
        "error must explain the tty requirement, got: {err_str}"
    );
    assert!(
        !err_str.contains("transfer cancelled by user"),
        "must not silently cancel, got: {err_str}"
    );

    let _ = fs::remove_dir_all(&source_root).await;
    Ok(())
}

/// Feature 002: receiver --ask-confirm=true, user types "y" → transfer succeeds.
/// Uses BORE_TEST_CONFIRM_RESPONSE env var to inject the answer without a real tty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_receiver_ask_confirm_accepts() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    // Inject "y" so the receiver confirmation returns true.
    std::env::set_var("BORE_TEST_CONFIRM_RESPONSE", "y");
    let _cleanup_env = EnvVarGuard("BORE_TEST_CONFIRM_RESPONSE");

    let source_root = temp_path("rx-confirm-accept-src");
    let dest_root = temp_path("rx-confirm-accept-dst");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    let source_file = source_root.join("hello.txt");
    write_file(&source_file, b"receiver accepted").await?;

    let transfer_id = format!("rx-confirm-accept-{}", Uuid::new_v4());

    let mut listener_opts = listener_options(transfer_id.clone(), dest_root.clone(), true, 1, None);
    listener_opts.ask_confirm = true;

    let listener_task = tokio::spawn(bore_cli::transfer::run_listener(listener_opts));
    tokio::time::sleep(Duration::from_millis(200)).await;

    bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await?;

    let outcome = listener_task.await??;
    // final_path for a single file is the file itself, not the dest directory
    let received = read_file(&outcome.final_path).await?;
    assert_eq!(received, b"receiver accepted");

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

/// Feature 002: receiver --ask-confirm=true, user types "n" → transfer rejected.
/// Sender should receive "peer reported an error: transfer rejected by receiver".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_receiver_ask_confirm_rejects() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    // Inject "n" so the receiver confirmation returns false.
    std::env::set_var("BORE_TEST_CONFIRM_RESPONSE", "n");
    let _cleanup_env = EnvVarGuard("BORE_TEST_CONFIRM_RESPONSE");

    let source_root = temp_path("rx-confirm-reject-src");
    let dest_root = temp_path("rx-confirm-reject-dst");
    fs::create_dir_all(&source_root).await?;
    fs::create_dir_all(&dest_root).await?;

    let source_file = source_root.join("data.txt");
    write_file(&source_file, b"should not arrive").await?;

    let transfer_id = format!("rx-confirm-reject-{}", Uuid::new_v4());

    let mut listener_opts = listener_options(transfer_id.clone(), dest_root.clone(), true, 1, None);
    listener_opts.ask_confirm = true;

    let listener_task = tokio::spawn(bore_cli::transfer::run_listener(listener_opts));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let sender_err = bore_cli::transfer::run_sender(sender_options(
        transfer_id.clone(),
        source_file,
        None,
        true,
        1,
        0,
        None,
    ))
    .await
    .expect_err("sender must fail when receiver rejects");

    let err_str = format!("{sender_err}");
    assert!(
        err_str.contains("rejected by receiver") || err_str.contains("peer reported an error"),
        "error must mention receiver rejection, got: {err_str}"
    );
    assert!(
        !err_str.contains("unexpected EOF"),
        "must not expose raw EOF, got: {err_str}"
    );

    // Listener exits with an error (rejection); we just discard it.
    let _ = listener_task.await;

    let _ = fs::remove_dir_all(&source_root).await;
    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}

/// Feature 002: when sender uses stdin, receiver --ask-confirm is silently ignored.
/// The unit-level assertion is in transfer::tests::receiver_ask_confirm_ignored_for_stdin.
/// This integration test verifies the listener starts up cleanly with ask_confirm=true
/// and doesn't crash before any sender connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_receiver_ask_confirm_listener_starts_cleanly() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(false).await;

    let dest_root = temp_path("rx-confirm-clean-dst");
    fs::create_dir_all(&dest_root).await?;

    let transfer_id = format!("rx-confirm-clean-{}", Uuid::new_v4());

    let mut listener_opts = listener_options(transfer_id.clone(), dest_root.clone(), true, 1, None);
    listener_opts.ask_confirm = true;

    let listener_task = tokio::spawn(bore_cli::transfer::run_listener(listener_opts));
    // Give the listener time to start; then abort it — we just verify it started.
    tokio::time::sleep(Duration::from_millis(300)).await;
    listener_task.abort();
    // JoinError::is_cancelled() means task was aborted, not panicked — that's fine.
    match listener_task.await {
        Err(e) if e.is_cancelled() => {}
        other => {
            // An actual error (not cancellation) would be unexpected.
            if let Ok(Err(e)) = other {
                let err_str = format!("{e}");
                // The only acceptable listener errors at this stage are transport-level
                // (e.g., "transport ended before a sender connected").
                assert!(
                    !err_str.contains("rejected by receiver"),
                    "listener must not self-reject before any sender connects: {err_str}"
                );
            }
        }
    }

    let _ = fs::remove_dir_all(&dest_root).await;
    Ok(())
}
