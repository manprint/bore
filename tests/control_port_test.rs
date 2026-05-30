use std::time::Duration;

use anyhow::Result;
use bore_cli::{client::Client, server::Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

/// A non-default control port, distinct from the hardcoded 7835 used elsewhere.
const CTRL: u16 = 17835;

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn custom_control_port_round_trip() -> Result<()> {
    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(CTRL);
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;

    // Local echo service.
    let local = TcpListener::bind("localhost:0").await?;
    let local_port = local.local_addr()?.port();
    tokio::spawn(async move {
        let (mut stream, _) = local.accept().await?;
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).await?;
        stream.write_all(&buf).await?;
        anyhow::Ok(())
    });

    // Client reaches the server via "host:port" using the custom control port.
    let to = format!("localhost:{CTRL}");
    let client = Client::new("localhost", local_port, &to, 0, Some("sec")).await?;
    let tunnel_port = client.remote_port();
    tokio::spawn(client.listen());

    let mut conn = TcpStream::connect(("127.0.0.1", tunnel_port)).await?;
    conn.write_all(b"abcde").await?;
    let mut buf = [0u8; 5];
    conn.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"abcde");

    Ok(())
}
