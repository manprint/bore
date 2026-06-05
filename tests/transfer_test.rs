use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use anyhow::{Context, Result};
use bore_cli::{
    server::Server,
    shared::CONTROL_PORT,
    transfer::{CollisionPolicy, DeviceMode, ListenerOptions, SenderOptions, SymlinkMode},
};
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
        collision: CollisionPolicy::Fail,
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
        source,
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
    }
}

fn patterned_bytes(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
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
        })
        .await
    });

    time::sleep(Duration::from_millis(200)).await;
    let sender = bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        source: source_file.clone(),
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
        })
        .await
    });

    time::sleep(Duration::from_millis(200)).await;
    bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        source: source_root.clone(),
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
    std::env::set_var("BORE_TRANSFER_TEST_MAX_ACKED_CHUNKS", "2");
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
    std::env::remove_var("BORE_TRANSFER_TEST_MAX_ACKED_CHUNKS");
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
        })
        .await
    });

    time::sleep(Duration::from_millis(300)).await;
    let sender = bore_cli::transfer::run_sender(SenderOptions {
        to: "localhost".to_string(),
        secret: Some("transfer-secret".to_string()),
        insecure: false,
        transfer_id: Some(transfer_id),
        source: source_file.clone(),
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
