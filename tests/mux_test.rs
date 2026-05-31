use std::time::Duration;

use bore_cli::mux;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// Isolates the multiplexer: a half-closed write on one side must surface as EOF
// on the peer, while the opposite direction stays usable.
#[tokio::test]
async fn mux_half_close_propagates_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (_opener, mut acceptor) = mux::server(sock);
        let mut stream = acceptor.accept().await.unwrap();
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
        // Expect EOF after the client half-closes its write half.
        let n = stream.read(&mut [0u8; 8]).await.unwrap();
        assert_eq!(n, 0, "expected EOF after peer shutdown");
        // Reverse direction must still work.
        stream.write_all(b"yo").await.unwrap();
        stream.shutdown().await.unwrap();
    });

    let sock = TcpStream::connect(addr).await.unwrap();
    let (opener, _acceptor) = mux::client(sock);
    let mut stream = opener.open().await.unwrap();
    stream.write_all(b"hi").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, b"yo");

    server.await.unwrap();
}

// Reproduces bore's data path: two TCP ends bridged through one substream with
// copy_bidirectional on each side. A half-close on one end must reach the other.
#[tokio::test]
async fn mux_copy_bidirectional_half_close() {
    let mux_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mux_addr = mux_listener.local_addr().unwrap();
    let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b_addr = b_listener.local_addr().unwrap();

    // "client" side of the mux: accept inbound substreams, bridge to endpoint B.
    tokio::spawn(async move {
        let sock = TcpStream::connect(mux_addr).await.unwrap();
        let (_opener, mut acceptor) = mux::client(sock);
        let mut substream = acceptor.accept().await.unwrap();
        let mut b = TcpStream::connect(b_addr).await.unwrap();
        tokio::io::copy_bidirectional(&mut b, &mut substream)
            .await
            .ok();
    });

    // "server" side of the mux: open a substream, bridge to endpoint A.
    let (a_sock, _) = mux_listener.accept().await.unwrap();
    let (opener, _acceptor) = mux::server(a_sock);
    let mut a = {
        let a_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a_listener.local_addr().unwrap();
        let connect = tokio::spawn(async move { TcpStream::connect(a_addr).await.unwrap() });
        let (mut a_srv, _) = a_listener.accept().await.unwrap();
        let a_cli = connect.await.unwrap();
        let mut substream = opener.open().await.unwrap();
        tokio::spawn(async move {
            tokio::io::copy_bidirectional(&mut a_srv, &mut substream)
                .await
                .ok();
        });
        a_cli
    };

    // Endpoint B echoes once then half-closes its write. Runs in its own task so
    // the data flow that establishes the substream is not blocked on it.
    tokio::spawn(async move {
        let (mut b, _) = b_listener.accept().await.unwrap();
        let mut buf = [0u8; 5];
        b.read_exact(&mut buf).await.unwrap();
        b.write_all(&buf).await.unwrap();
        b.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await; // keep B alive past the assertion
    });

    a.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    a.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
    // B half-closed, so A must observe EOF.
    let n = a.read(&mut [0u8; 8]).await.unwrap();
    assert_eq!(n, 0, "expected EOF to propagate through copy_bidirectional");
}
