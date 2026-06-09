#![allow(dead_code)]

use anyhow::{bail, ensure, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_HTTP_HEAD: usize = 16 * 1024;
const CLOSE_OPCODE: u8 = 0x8;
const PING_OPCODE: u8 = 0x9;
const PONG_OPCODE: u8 = 0xA;

pub fn spawn_websocket_echo_listener(listener: TcpListener, expected_header: Option<&'static str>) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = handle_websocket_echo(stream, expected_header).await;
            });
        }
    });
}

pub async fn spawn_websocket_echo_service(expected_header: Option<&'static str>) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    spawn_websocket_echo_listener(listener, expected_header);
    Ok(port)
}

pub async fn assert_websocket_round_trip<S>(stream: &mut S, host: &str, path: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    client_handshake(stream, host, path).await?;

    let text = b"hello websocket through bore";
    write_frame(stream, 0x1, text, true).await?;
    let (opcode, payload) = read_frame(stream).await?;
    ensure!(opcode == 0x1, "expected text echo, got opcode {opcode:#x}");
    ensure!(payload == text, "text frame payload mismatch");

    let binary: Vec<u8> = (0..200u16).map(|n| (n % 251) as u8).collect();
    write_frame(stream, 0x2, &binary, true).await?;
    let (opcode, payload) = read_frame(stream).await?;
    ensure!(opcode == 0x2, "expected binary echo, got opcode {opcode:#x}");
    ensure!(payload == binary, "binary frame payload mismatch");

    write_frame(stream, PING_OPCODE, b"ping", true).await?;
    let (opcode, payload) = read_frame(stream).await?;
    ensure!(opcode == PONG_OPCODE, "expected pong, got opcode {opcode:#x}");
    ensure!(payload == b"ping", "pong payload mismatch");

    write_frame(stream, CLOSE_OPCODE, &[], true).await?;
    let (opcode, _) = read_frame(stream).await?;
    ensure!(opcode == CLOSE_OPCODE, "expected close echo, got opcode {opcode:#x}");
    Ok(())
}

async fn client_handshake<S>(stream: &mut S, host: &str, path: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGVzdC1ib3JlLXdz\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let response = read_http_head(stream).await?;
    let response_text = String::from_utf8_lossy(&response).to_lowercase();
    ensure!(
        response_text.starts_with("http/1.1 101"),
        "expected 101 Switching Protocols, got: {response_text}"
    );
    ensure!(
        response_text.contains("upgrade: websocket"),
        "missing Upgrade: websocket in response: {response_text}"
    );
    ensure!(
        response_text.contains("connection: upgrade"),
        "missing Connection: Upgrade in response: {response_text}"
    );
    Ok(())
}

async fn handle_websocket_echo(mut stream: TcpStream, expected_header: Option<&str>) -> Result<()> {
    let request = read_http_head(&mut stream).await?;
    let request_text = String::from_utf8_lossy(&request).to_lowercase();
    ensure!(
        request_text.contains("upgrade: websocket"),
        "missing Upgrade: websocket in request: {request_text}"
    );
    ensure!(
        request_text.contains("connection: upgrade"),
        "missing Connection: Upgrade in request: {request_text}"
    );
    if let Some(expected_header) = expected_header {
        ensure!(
            request_text.contains(&expected_header.to_ascii_lowercase()),
            "missing expected injected header `{expected_header}` in request: {request_text}"
        );
    }

    stream
        .write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ignored-by-tests\r\n\r\n",
        )
        .await?;

    loop {
        let (opcode, payload) = read_frame(&mut stream).await?;
        match opcode {
            0x1 | 0x2 => write_frame(&mut stream, opcode, &payload, false).await?,
            PING_OPCODE => write_frame(&mut stream, PONG_OPCODE, &payload, false).await?,
            CLOSE_OPCODE => {
                write_frame(&mut stream, CLOSE_OPCODE, &[], false).await?;
                stream.shutdown().await?;
                return Ok(());
            }
            other => bail!("unsupported websocket opcode {other:#x}"),
        }
    }
}

async fn read_http_head<S>(stream: &mut S) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if buf[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        ensure!(buf.len() < MAX_HTTP_HEAD, "websocket HTTP head exceeded {MAX_HTTP_HEAD} bytes");
    }
    bail!("unexpected EOF while reading websocket HTTP head")
}

async fn read_frame<S>(stream: &mut S) -> Result<(u8, Vec<u8>)>
where
    S: AsyncRead + Unpin,
{
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;
    ensure!(header[0] & 0x70 == 0, "RSV bits must be clear");
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let len = read_payload_len(stream, header[1] & 0x7F).await?;
    let mut mask = [0u8; 4];
    if masked {
        stream.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    if masked {
        for (idx, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[idx % mask.len()];
        }
    }
    Ok((opcode, payload))
}

async fn read_payload_len<S>(stream: &mut S, short_len: u8) -> Result<usize>
where
    S: AsyncRead + Unpin,
{
    match short_len {
        len @ 0..=125 => Ok(len as usize),
        126 => {
            let mut buf = [0u8; 2];
            stream.read_exact(&mut buf).await?;
            Ok(u16::from_be_bytes(buf) as usize)
        }
        127 => {
            let mut buf = [0u8; 8];
            stream.read_exact(&mut buf).await?;
            let len = u64::from_be_bytes(buf);
            ensure!(len <= usize::MAX as u64, "frame too large for this platform");
            Ok(len as usize)
        }
        _ => unreachable!("payload length is masked to 7 bits"),
    }
}

async fn write_frame<S>(stream: &mut S, opcode: u8, payload: &[u8], masked: bool) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.push(0x80 | (opcode & 0x0F));

    if payload.len() <= 125 {
        frame.push((if masked { 0x80 } else { 0 }) | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push((if masked { 0x80 } else { 0 }) | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push((if masked { 0x80 } else { 0 }) | 127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }

    if masked {
        let mask = [0x12, 0x34, 0x56, 0x78];
        frame.extend_from_slice(&mask);
        for (idx, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[idx % mask.len()]);
        }
    } else {
        frame.extend_from_slice(payload);
    }

    stream.write_all(&frame).await?;
    Ok(())
}