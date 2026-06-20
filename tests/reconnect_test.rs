use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::{Client, ProviderMeta},
    reconnect,
    secret::Proxy,
    server::Server,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// Multi-connection echo service; returns its local port.
async fn echo_service() -> Result<u16> {
    let listener = TcpListener::bind("localhost:0").await?;
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

/// Attempt a single request/response through `addr`; returns whether it worked.
async fn try_roundtrip(addr: (&str, u16)) -> bool {
    let attempt = async {
        let mut conn = TcpStream::connect(addr).await?;
        conn.write_all(b"ping").await?;
        let mut buf = [0u8; 4];
        conn.read_exact(&mut buf).await?;
        anyhow::Ok(buf == *b"ping")
    };
    matches!(
        time::timeout(Duration::from_secs(1), attempt).await,
        Ok(Ok(true))
    )
}

/// Poll a round-trip through `addr` until it succeeds, within a deadline.
async fn await_roundtrip(addr: (&str, u16)) -> bool {
    for _ in 0..60 {
        if try_roundtrip(addr).await {
            return true;
        }
        time::sleep(Duration::from_millis(200)).await;
    }
    false
}

#[tokio::test]
async fn client_reconnects_when_server_appears() -> Result<()> {
    const CONTROL: u16 = 17910;
    const REMOTE: u16 = 16100;
    wait_port(CONTROL, false).await;

    let echo = echo_service().await?;

    // Start the auto-reconnecting client while the server is still down.
    tokio::spawn(async move {
        let connect = move || async move {
            let to = format!("localhost:{CONTROL}");
            Client::new(
                "localhost",
                echo,
                &to,
                REMOTE,
                None,
                false,
                Default::default(),
                None,
            )
            .await
        };
        let serve = |client: Client| async move { client.listen().await };
        let _ = reconnect::run(true, connect, serve).await;
    });

    // Bring the server up shortly after; the client must reconnect on its own.
    time::sleep(Duration::from_millis(300)).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CONTROL);
    tokio::spawn(server.listen());

    assert!(
        await_roundtrip(("127.0.0.1", REMOTE)).await,
        "client should reconnect and serve once the server is up"
    );

    Ok(())
}

#[tokio::test]
async fn proxy_reconnects_when_server_appears() -> Result<()> {
    const CONTROL: u16 = 17911;
    const LOCAL_PROXY: u16 = 16101;
    wait_port(CONTROL, false).await;

    let echo = echo_service().await?;

    // Start the auto-reconnecting proxy while the server is still down.
    tokio::spawn(async move {
        let connect = move || async move {
            let to = format!("localhost:{CONTROL}");
            let bind = format!("127.0.0.1:{LOCAL_PROXY}").parse().unwrap();
            Proxy::new(
                &to, bind, "svc", None, false, false, None, false, false, 0, 0, 1, None, false,
            )
            .await
        };
        let serve = |proxy: Proxy| async move { proxy.listen().await };
        let _ = reconnect::run(true, connect, serve).await;
    });

    // Bring up the server and the provider after a delay.
    time::sleep(Duration::from_millis(300)).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CONTROL);
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;

    let to = format!("localhost:{CONTROL}");
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        &to,
        "svc",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
        None,
    )
    .await?;
    tokio::spawn(provider.listen());

    assert!(
        await_roundtrip(("127.0.0.1", LOCAL_PROXY)).await,
        "proxy should reconnect and serve once the server and provider are up"
    );

    Ok(())
}
