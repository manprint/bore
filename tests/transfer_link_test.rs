use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(unix)]
use bore_cli::transfer_link::prepare_exec;
use bore_cli::transfer_link::{
    content_disposition, encode_path_segment, filename_from_path, prepare_selection,
    spawn_file_producer_with_options, validate_filename, FileProducerOptions, LinkConfigError,
    LinkLimits, LinkOptions, ManifestEntryKind, PreparedFile, PreparedSource, SourceMessage,
    TransferLinkHttp, ValidationGate, MAX_DOWNLOADS, MAX_FILENAME_BYTES,
};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("bore-transfer-link-{label}-{}", Uuid::new_v4()))
}

async fn prepared(path: &Path) -> PreparedFile {
    PreparedFile::from_path(path.to_path_buf(), None)
        .await
        .unwrap()
}

async fn collect(mut producer: bore_cli::transfer_link::FileProducerHandle) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut complete = false;
    while let Some(message) = producer.recv().await {
        match message {
            SourceMessage::Data(bytes) => payload.extend_from_slice(&bytes),
            SourceMessage::Complete(summary) => {
                assert_eq!(summary.bytes_read, payload.len() as u64);
                complete = true;
            }
            SourceMessage::Failed(failure) => panic!("unexpected producer failure: {failure:?}"),
        }
    }
    producer.join().await.unwrap();
    assert!(complete, "producer closed without Complete");
    payload
}

#[test]
fn filename_rejects_controls_and_separators() {
    for invalid in ["", ".", "..", "a/b", "a\\b", "line\nfeed", "tab\tname"] {
        assert!(validate_filename(invalid).is_err(), "accepted {invalid:?}");
    }
    let too_long = "x".repeat(MAX_FILENAME_BYTES + 1);
    assert!(validate_filename(&too_long).is_err());
}

#[test]
fn filename_spaces_unicode_percent_roundtrip() {
    let filename = "résumé final #1.bin";
    validate_filename(filename).unwrap();
    assert_eq!(
        encode_path_segment(filename),
        "r%C3%A9sum%C3%A9%20final%20%231.bin"
    );
    assert_eq!(
        content_disposition(filename).unwrap(),
        "attachment; filename=\"r_sum_ final #1.bin\"; filename*=UTF-8''r%C3%A9sum%C3%A9%20final%20%231.bin"
    );
}

#[test]
fn os_path_is_not_lossily_rewritten() {
    let path = Path::new("/var/backups/backup.tar");
    assert_eq!(filename_from_path(path, None).unwrap(), "backup.tar");
    assert_eq!(
        filename_from_path(Path::new("/tmp/ignored-name"), Some("backup.tar")).unwrap(),
        "backup.tar"
    );
}

#[test]
fn limits_reject_zero_and_overflow() {
    assert!(matches!(
        LinkLimits::new(0, Duration::from_secs(1)),
        Err(LinkConfigError::MaxDownloads { value: 0 })
    ));
    assert!(matches!(
        LinkLimits::new(MAX_DOWNLOADS + 1, Duration::from_secs(1)),
        Err(LinkConfigError::MaxDownloads { value }) if value == MAX_DOWNLOADS + 1
    ));
    assert!(LinkLimits::new(1, Duration::ZERO).is_err());
    assert!(LinkLimits::new(1, Duration::from_secs(61)).is_err());
    assert!(LinkLimits::new(1, Duration::from_millis(1_001)).is_err());
}

#[tokio::test]
async fn file_tail_waits_for_final_validation() {
    let path = temp_path("tail");
    fs::write(&path, b"abcdefgh").await.unwrap();
    let gate = ValidationGate::new();
    let mut options = FileProducerOptions::new(4, 2).unwrap();
    options.validation_gate = Some(gate.clone());
    let mut producer =
        spawn_file_producer_with_options(prepared(&path).await, CancellationToken::new(), options)
            .unwrap();
    gate.wait_until_reached().await;
    assert_eq!(
        producer.try_recv().unwrap(),
        SourceMessage::Data(bytes::Bytes::from_static(b"abcd"))
    );
    assert!(matches!(
        producer.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    gate.release();
    let mut tail = Vec::new();
    let mut complete = false;
    while let Some(message) = producer.recv().await {
        match message {
            SourceMessage::Data(bytes) => tail.extend_from_slice(&bytes),
            SourceMessage::Complete(summary) => {
                assert_eq!(summary.bytes_read, 8);
                complete = true;
            }
            SourceMessage::Failed(failure) => panic!("unexpected producer failure: {failure:?}"),
        }
    }
    producer.join().await.unwrap();
    assert!(complete);
    assert_eq!(tail, b"efgh");
    fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn zero_byte_file_validates_before_success() {
    let path = temp_path("empty");
    fs::write(&path, []).await.unwrap();
    let gate = ValidationGate::new();
    let mut options = FileProducerOptions::new(4, 2).unwrap();
    options.validation_gate = Some(gate.clone());
    let mut producer =
        spawn_file_producer_with_options(prepared(&path).await, CancellationToken::new(), options)
            .unwrap();
    gate.wait_until_reached().await;
    assert!(matches!(
        producer.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    gate.release();
    assert_eq!(collect(producer).await, b"");
    fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn growth_shrink_replacement_are_errors() {
    let path = temp_path("mutate");
    fs::write(&path, b"1234").await.unwrap();
    let gate = ValidationGate::new();
    let mut options = FileProducerOptions::new(4, 2).unwrap();
    options.validation_gate = Some(gate.clone());
    let producer =
        spawn_file_producer_with_options(prepared(&path).await, CancellationToken::new(), options)
            .unwrap();
    gate.wait_until_reached().await;
    let mut append = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await
        .unwrap();
    append.write_all(b"x").await.unwrap();
    append.sync_all().await.unwrap();
    gate.release();
    assert_failed(producer).await;
    fs::remove_file(&path).await.unwrap();

    fs::write(&path, b"1234").await.unwrap();
    let gate = ValidationGate::new();
    let mut options = FileProducerOptions::new(4, 2).unwrap();
    options.validation_gate = Some(gate.clone());
    let producer =
        spawn_file_producer_with_options(prepared(&path).await, CancellationToken::new(), options)
            .unwrap();
    gate.wait_until_reached().await;
    fs::write(&path, b"1").await.unwrap();
    gate.release();
    assert_failed(producer).await;

    #[cfg(unix)]
    {
        fs::write(&path, b"1234").await.unwrap();
        let gate = ValidationGate::new();
        let mut options = FileProducerOptions::new(4, 2).unwrap();
        options.validation_gate = Some(gate.clone());
        let producer = spawn_file_producer_with_options(
            prepared(&path).await,
            CancellationToken::new(),
            options,
        )
        .unwrap();
        gate.wait_until_reached().await;
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, b"abcd").await.unwrap();
        fs::rename(&replacement, &path).await.unwrap();
        gate.release();
        assert_failed(producer).await;
    }
    fs::remove_file(path).await.unwrap();
}

async fn assert_failed(mut producer: bore_cli::transfer_link::FileProducerHandle) {
    let mut failed = false;
    while let Some(message) = producer.recv().await {
        if let SourceMessage::Failed(failure) = message {
            assert!(failure.error.contains("changed"));
            failed = true;
        }
    }
    assert!(failed, "producer closed without Failed");
    producer.join().await.unwrap();
}

#[tokio::test]
async fn receiver_drop_unblocks_full_queue() {
    let path = temp_path("drop");
    fs::write(&path, vec![b'x'; 4 * 32]).await.unwrap();
    let mut options = FileProducerOptions::new(4, 2).unwrap();
    options.validation_gate = None;
    let producer =
        spawn_file_producer_with_options(prepared(&path).await, CancellationToken::new(), options)
            .unwrap();
    let (mut receiver, task) = producer.into_parts();
    let _ = timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    drop(receiver);
    timeout(Duration::from_secs(1), task)
        .await
        .expect("producer remained blocked after receiver drop")
        .unwrap();
    fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn large_reader_never_queues_more_than_two_chunks() {
    let path = temp_path("queue");
    fs::write(&path, b"abcdefghijkl").await.unwrap();
    let gate = ValidationGate::new();
    let mut options = FileProducerOptions::new(4, 2).unwrap();
    options.validation_gate = Some(gate.clone());
    let mut producer =
        spawn_file_producer_with_options(prepared(&path).await, CancellationToken::new(), options)
            .unwrap();
    gate.wait_until_reached().await;
    let mut queued = 0;
    let mut prefix = Vec::new();
    while let Ok(message) = producer.try_recv() {
        if let SourceMessage::Data(bytes) = message {
            queued += 1;
            prefix.extend_from_slice(&bytes);
        }
    }
    assert!(queued <= 2, "queued {queued} chunks");
    gate.release();
    let mut complete = false;
    while let Some(message) = producer.recv().await {
        match message {
            SourceMessage::Data(bytes) => prefix.extend_from_slice(&bytes),
            SourceMessage::Complete(summary) => {
                assert_eq!(summary.bytes_read, 12);
                complete = true;
            }
            SourceMessage::Failed(failure) => panic!("unexpected producer failure: {failure:?}"),
        }
    }
    producer.join().await.unwrap();
    assert!(complete);
    assert_eq!(prefix, b"abcdefghijkl");
    fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn sha256_matches_exact_payload() {
    let path = temp_path("hash");
    let data = b"payload with exact bytes\0\xff";
    fs::write(&path, data).await.unwrap();
    let mut producer = spawn_file_producer_with_options(
        prepared(&path).await,
        CancellationToken::new(),
        FileProducerOptions::new(4, 2).unwrap(),
    )
    .unwrap();
    let mut payload = Vec::new();
    let mut digest = None;
    while let Some(message) = producer.recv().await {
        match message {
            SourceMessage::Data(bytes) => payload.extend_from_slice(&bytes),
            SourceMessage::Complete(summary) => digest = Some(summary.sha256),
            SourceMessage::Failed(failure) => panic!("unexpected producer failure: {failure:?}"),
        }
    }
    producer.join().await.unwrap();
    let expected: [u8; 32] = Sha256::digest(data).into();
    assert_eq!(payload, data);
    assert_eq!(digest, Some(expected));
    fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn mixed_roots_have_deterministic_archive_manifest() {
    let directory = temp_path("manifest-dir");
    let second = temp_path("manifest-file.txt");
    fs::create_dir_all(directory.join("empty")).await.unwrap();
    fs::write(directory.join("z.txt"), b"z").await.unwrap();
    fs::write(directory.join("a.txt"), b"a").await.unwrap();
    fs::write(&second, b"second").await.unwrap();

    let prepared = prepare_selection(vec![directory.clone(), second.clone()], None)
        .await
        .unwrap();
    let PreparedSource::Archive(archive) = prepared else {
        panic!("directory selection unexpectedly used raw-file mode");
    };
    let names: Vec<_> = archive
        .manifest
        .entries
        .iter()
        .map(|entry| (entry.zip_name.clone(), entry.kind))
        .collect();
    assert!(names.contains(&(
        format!("{}/", directory.file_name().unwrap().to_string_lossy()),
        ManifestEntryKind::Directory
    )));
    assert!(names
        .iter()
        .any(|(name, kind)| name.ends_with("/empty/") && *kind == ManifestEntryKind::Directory));
    assert!(names
        .iter()
        .any(|(name, kind)| name.ends_with("/a.txt") && *kind == ManifestEntryKind::File));
    assert_eq!(names, {
        let mut sorted = names.clone();
        sorted.sort_by(|left, right| left.0.cmp(&right.0));
        sorted
    });
    archive.manifest.validate().await.unwrap();
    fs::remove_dir_all(directory).await.unwrap();
    fs::remove_file(second).await.unwrap();
}

#[tokio::test]
async fn manifest_rejects_collisions_symlinks_and_mutations() {
    let first_dir = temp_path("collision-a");
    let second_dir = temp_path("collision-b");
    fs::create_dir_all(&first_dir).await.unwrap();
    fs::create_dir_all(&second_dir).await.unwrap();
    let first = first_dir.join("same.bin");
    let second = second_dir.join("same.bin");
    fs::write(&first, b"a").await.unwrap();
    fs::write(&second, b"b").await.unwrap();
    let error = prepare_selection(vec![first, second], None)
        .await
        .expect_err("duplicate archive basenames were accepted");
    assert!(error.to_string().contains("collision"));

    #[cfg(unix)]
    {
        let link = first_dir.join("link");
        std::os::unix::fs::symlink(second_dir.join("same.bin"), &link).unwrap();
        let error = prepare_selection(vec![link], None)
            .await
            .expect_err("symlink root was accepted");
        assert!(error.to_string().contains("symbolic link"));
        std::fs::remove_file(first_dir.join("link")).unwrap();
    }

    let prepared = prepare_selection(vec![first_dir.clone()], None)
        .await
        .unwrap();
    let PreparedSource::Archive(archive) = prepared else {
        panic!("directory selection unexpectedly used raw-file mode");
    };
    fs::write(first_dir.join("new.bin"), b"new").await.unwrap();
    assert!(archive.manifest.validate().await.is_err());
    assert!(archive.manifest.is_invalidated());
    assert!(archive.manifest.validate().await.is_err());

    fs::remove_dir_all(first_dir).await.unwrap();
    fs::remove_dir_all(second_dir).await.unwrap();
}

async fn start_http(
    path: &Path,
    filename: &str,
    max_downloads: usize,
) -> (
    SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), bore_cli::transfer_link::HttpServerError>>,
) {
    let prepared = prepared(path).await;
    let options = LinkOptions::new(filename, max_downloads, Duration::from_secs(1)).unwrap();
    let server = TransferLinkHttp::bind(prepared, options).await.unwrap();
    let address = server.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(server.run(cancellation.clone()));
    (address, cancellation, task)
}

async fn stop_http(
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), bore_cli::transfer_link::HttpServerError>>,
) {
    cancellation.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .expect("HTTP server did not stop")
        .expect("HTTP server task panicked")
        .expect("HTTP server failed")
}

async fn raw_http(address: SocketAddr, request: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("HTTP response timed out")
        .unwrap();
    response
}

fn split_response(response: &[u8]) -> (&str, &[u8], &[u8]) {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response has no header terminator");
    let headers = std::str::from_utf8(&response[..separator]).unwrap();
    let body = &response[separator + 4..];
    let mut lines = headers.split("\r\n");
    let status = lines.next().unwrap();
    (status, &response[..separator], body)
}

fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut cursor = 0;
    let mut decoded = Vec::new();
    loop {
        let line_end = body[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("chunk size line");
        let size_text = std::str::from_utf8(&body[cursor..cursor + line_end]).unwrap();
        let size = usize::from_str_radix(size_text.trim(), 16).unwrap();
        cursor += line_end + 2;
        if size == 0 {
            break;
        }
        decoded.extend_from_slice(&body[cursor..cursor + size]);
        cursor += size + 2;
    }
    decoded
}

#[tokio::test]
async fn head_does_not_read_source() {
    let path = temp_path("head");
    fs::write(&path, b"head payload").await.unwrap();
    let (address, cancellation, task) = start_http(&path, "head.bin", 1).await;
    fs::remove_file(&path).await.unwrap();

    let response = raw_http(
        address,
        "HEAD /head.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, headers, body) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(headers
        .windows(b"Content-Length: 12".len())
        .any(|window| window.eq_ignore_ascii_case(b"Content-Length: 12")));
    assert!(body.is_empty());

    let response = raw_http(
        address,
        "GET /head.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, _) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 500"), "{status}");
    stop_http(cancellation, task).await;
}

#[tokio::test]
async fn archive_get_is_stored_zip_stream() {
    let directory = temp_path("zip-http");
    fs::create_dir_all(directory.join("empty")).await.unwrap();
    fs::write(directory.join("file.txt"), b"archive payload")
        .await
        .unwrap();
    let prepared = prepare_selection(vec![directory.clone()], None)
        .await
        .unwrap();
    let options = LinkOptions::new("download.zip", 1, Duration::from_secs(1)).unwrap();
    let server = TransferLinkHttp::bind_source(prepared, options)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(server.run(cancellation.clone()));
    let response = raw_http(
        address,
        "GET /download.zip HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, headers, body) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(headers
        .windows(b"Content-Type: application/zip".len())
        .any(|window| { window.eq_ignore_ascii_case(b"Content-Type: application/zip") }));
    assert!(headers
        .windows(b"Transfer-Encoding: chunked".len())
        .any(|window| window.eq_ignore_ascii_case(b"Transfer-Encoding: chunked")));
    let archive = decode_chunked(body);
    assert!(archive.starts_with(b"PK\x03\x04"));
    assert!(archive
        .windows(b"file.txt".len())
        .any(|window| window == b"file.txt"));
    assert!(archive
        .windows(b"empty/".len())
        .any(|window| window == b"empty/"));
    assert!(
        archive.windows(4).any(|window| window == b"PK\x05\x06")
            || archive.windows(4).any(|window| window == b"PK\x06\x06")
    );
    stop_http(cancellation, task).await;
    fs::remove_dir_all(directory).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn exec_stream_starts_on_first_get_and_is_one_shot() {
    use std::ffi::OsString;

    let stream = prepare_exec(
        "backup.tar".to_owned(),
        vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("printf 'exec\\000payload'"),
        ],
    )
    .unwrap();
    let options = LinkOptions::new("backup.tar", 1, Duration::from_secs(1)).unwrap();
    let server = TransferLinkHttp::bind_source(PreparedSource::OneShot(stream), options)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(server.run(cancellation.clone()));

    let head = raw_http(
        address,
        "HEAD /backup.tar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, headers, body) = split_response(&head);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(headers
        .windows(b"Content-Length:".len())
        .all(|window| !window.eq_ignore_ascii_case(b"Content-Length:")));
    assert!(body.is_empty());

    let response = raw_http(
        address,
        "GET /backup.tar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, headers, body) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(headers
        .windows(b"Content-Type: application/octet-stream".len())
        .any(|window| { window.eq_ignore_ascii_case(b"Content-Type: application/octet-stream") }));
    assert!(headers
        .windows(b"Transfer-Encoding: chunked".len())
        .any(|window| window.eq_ignore_ascii_case(b"Transfer-Encoding: chunked")));
    assert_eq!(decode_chunked(body), b"exec\0payload");

    let second = raw_http(
        address,
        "GET /backup.tar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, _) = split_response(&second);
    assert!(status.starts_with("HTTP/1.1 410"), "{status}");
    stop_http(cancellation, task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn exec_empty_success_emits_a_valid_empty_response() {
    use std::ffi::OsString;

    let stream = prepare_exec(
        "empty.tar".to_owned(),
        vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("exit 0"),
        ],
    )
    .unwrap();
    let options = LinkOptions::new("empty.tar", 1, Duration::from_secs(1)).unwrap();
    let server = TransferLinkHttp::bind_source(PreparedSource::OneShot(stream), options)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(server.run(cancellation.clone()));

    let response = raw_http(
        address,
        "GET /empty.tar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, headers, body) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(headers
        .windows(b"Transfer-Encoding: chunked".len())
        .any(|window| window.eq_ignore_ascii_case(b"Transfer-Encoding: chunked")));
    assert!(decode_chunked(body).is_empty());

    let second = raw_http(
        address,
        "GET /empty.tar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(split_response(&second).0.starts_with("HTTP/1.1 410"));
    stop_http(cancellation, task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn one_shot_rejects_concurrent_get_and_preserves_single_claim() {
    use std::ffi::OsString;

    let stream = prepare_exec(
        "slow.bin".to_owned(),
        vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("sleep 1; printf slow"),
        ],
    )
    .unwrap();
    let options = LinkOptions::new("slow.bin", 2, Duration::from_secs(1)).unwrap();
    let server = TransferLinkHttp::bind_source(PreparedSource::OneShot(stream), options)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(server.run(cancellation.clone()));
    let request = "GET /slow.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (first, second) = tokio::join!(raw_http(address, request), raw_http(address, request));
    let first_status = split_response(&first).0.to_owned();
    let second_status = split_response(&second).0.to_owned();
    let mut statuses = [first_status, second_status];
    statuses.sort();
    assert!(statuses[0].starts_with("HTTP/1.1 200"));
    assert!(statuses[1].starts_with("HTTP/1.1 409"));
    stop_http(cancellation, task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn exec_failure_never_emits_success_terminator() {
    use std::ffi::OsString;

    let stream = prepare_exec(
        "failed.tar".to_owned(),
        vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("printf partial; exit 7"),
        ],
    )
    .unwrap();
    let options = LinkOptions::new("failed.tar", 1, Duration::from_secs(1)).unwrap();
    let server = TransferLinkHttp::bind_source(PreparedSource::OneShot(stream), options)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(server.run(cancellation.clone()));
    let response = raw_http(
        address,
        "GET /failed.tar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, body) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.windows(7).any(|window| window == b"partial"));
    assert!(!body.windows(5).any(|window| window == b"0\r\n\r\n"));
    let second = raw_http(
        address,
        "GET /failed.tar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(split_response(&second).0.starts_with("HTTP/1.1 410"));
    stop_http(cancellation, task).await;
}

#[tokio::test]
async fn unknown_stream_rejects_http10_before_claim() {
    let stream = bore_cli::transfer_link::prepare_stdin("stream.bin".to_owned()).unwrap();
    let options = LinkOptions::new("stream.bin", 1, Duration::from_secs(1)).unwrap();
    let server = TransferLinkHttp::bind_source(PreparedSource::OneShot(stream), options)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(server.run(cancellation.clone()));
    let response = raw_http(
        address,
        "GET /stream.bin HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, _) = split_response(&response);
    assert!(
        status.ends_with("505 HTTP Version Not Supported"),
        "{status}"
    );
    let head = raw_http(
        address,
        "HEAD /stream.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, _) = split_response(&head);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    stop_http(cancellation, task).await;
}

#[tokio::test]
async fn range_returns_full_200() {
    let path = temp_path("range");
    fs::write(&path, b"range payload").await.unwrap();
    let (address, cancellation, task) = start_http(&path, "range.bin", 1).await;
    let response = raw_http(
        address,
        "GET /range.bin HTTP/1.1\r\nHost: localhost\r\nRange: bytes=0-1\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, headers, body) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(headers
        .windows(b"Accept-Ranges: none".len())
        .any(|window| window.eq_ignore_ascii_case(b"Accept-Ranges: none")));
    assert_eq!(body, b"range payload");
    stop_http(cancellation, task).await;
    fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn bad_path_and_methods_do_not_start_source() {
    let path = temp_path("routing");
    fs::write(&path, b"routing payload").await.unwrap();
    let (address, cancellation, task) = start_http(&path, "route.bin", 1).await;

    let response = raw_http(
        address,
        "GET /other.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, _) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 404"), "{status}");

    let response = raw_http(
        address,
        "POST /route.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, headers, _) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 405"), "{status}");
    assert!(headers
        .windows(b"Allow: GET, HEAD".len())
        .any(|window| window.eq_ignore_ascii_case(b"Allow: GET, HEAD")));

    let response = raw_http(
        address,
        "GET /route.bin HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, _) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 400"), "{status}");

    fs::remove_file(&path).await.unwrap();
    let response = raw_http(
        address,
        "GET /route.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (status, _, _) = split_response(&response);
    assert!(status.starts_with("HTTP/1.1 500"), "{status}");
    stop_http(cancellation, task).await;
}

#[tokio::test]
async fn header_deadline_has_timer() {
    let path = temp_path("timeout");
    fs::write(&path, b"timeout payload").await.unwrap();
    let (address, cancellation, task) = start_http(&path, "timeout.bin", 1).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /timeout.bin HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("header timeout did not close the socket")
        .unwrap();
    assert!(response.is_empty() || response.starts_with(b"HTTP/1.1 408"));
    stop_http(cancellation, task).await;
    fs::remove_file(path).await.unwrap();
}
