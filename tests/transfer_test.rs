use std::path::{Path, PathBuf};
use std::time::Duration;

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
        source: source_file.display().to_string(),
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
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
        source: source_root.display().to_string(),
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
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
        source: source_file.display().to_string(),
        output: None,
        relay_only: false,
        stun_server: Some(stun),
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
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
