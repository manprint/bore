//! Room owner for the web-transfer browser e2e suite (`T-WEB-BROWSER-ROOM`).
//!
//! Not user-facing: Phase 2 exposes no public `bore transfer web` command,
//! so Playwright cannot create rooms by itself. This helper holds one owner
//! lease against a running server and prints the capability URL, staying
//! alive until killed:
//!
//! ```sh
//! cargo build --all-features --example web_transfer_e2e_owner
//! ./target/debug/examples/web_transfer_e2e_owner 127.0.0.1:7835
//! # stdout: WEB_TRANSFER_ROOM_URL=https://…/transfer/#YVCYDRDYkjNIIyoIIHIH_w
//! ```
//!
//! Only the `WEB_TRANSFER_ROOM_URL=` line is machine-read (by
//! `web/transfer/tests/e2e/room.spec.mjs`); anything else on stdout would
//! break that parser, so diagnostics go to stderr.

use bore_cli::web_transfer_cli::{run_owner_lease, OwnerClientConfig};
use std::io::Write;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let endpoint = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: web_transfer_e2e_owner <host:port>");
        std::process::exit(2);
    });
    // A second argument of `relay-only` opens the room with the operator's
    // `--relay-only` policy, which is what lets a BROWSER gate prove the
    // server never starts a direct attempt in such a room — the Rust units
    // prove the registry's half, and a page that never constructs an
    // `RTCPeerConnection` is the half a user can see.
    let relay_only = std::env::args().nth(2).as_deref() == Some("relay-only");
    let (created_tx, created_rx) = tokio::sync::oneshot::channel();
    let (_lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::channel(4);
    let run = tokio::spawn(run_owner_lease(
        OwnerClientConfig {
            endpoint,
            open_browser: false,
            relay_only,
            ..OwnerClientConfig::default()
        },
        created_tx,
        lifecycle_rx,
    ));
    let created = created_rx.await?;
    println!("WEB_TRANSFER_ROOM_URL={}", created.display_url);
    std::io::stdout().flush()?;
    // Hold the lease until killed; the room dies with this process unless a
    // lifecycle event arrives (none is ever sent here).
    let _ = run.await;
    Ok(())
}
