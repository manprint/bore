//! Owner lease client for `bore transfer web` rooms (Phase 1.4).
//!
//! Internal API exercised by `T-WEB-OWNER-LEASE`; the public `bore transfer
//! web` command arrives in Phase 3 and reuses exactly this loop. Generates
//! the room secrets once, holds the owner lease across reconnects (resume
//! only, never a silent replacement room) and destroys the room on clean
//! lifecycle events. Every log/error line carries the room ID and phase only
//! — never the URL, tokens or room key.

use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::auth::Authenticator;
use crate::client::{beat_once, CtrlBeat};
use crate::mux;
use crate::shared::{ClientMessage, ControlFrameSummary, Delimited, ServerMessage};
use crate::transport::{self, Endpoint};
use crate::web_transfer::{MemberToken, OwnerToken, RoomId, RoomKey};
use crate::web_transfer_protocol::PROTOCOL_VERSION;

/// Owner heartbeat period: the server reaps past its own (longer) deadline.
pub const OWNER_HEARTBEAT: Duration = Duration::from_secs(20);
/// Bound on one graceful-close write.
pub const OWNER_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// Resume backoff ladder in milliseconds (then the ceiling holds).
pub const RESUME_BACKOFF: &[u64] = &[250, 500, 1000, 2000, 4000];
/// Backoff ceiling once the ladder is exhausted.
pub const RESUME_BACKOFF_MAX_MS: u64 = 5000;

/// Lifecycle events driving the owner loop. Tests supply these; OS signal
/// wiring arrives with the public command in Phase 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerLifecycle {
    /// SIGINT equivalent: close cleanly.
    Interrupt,
    /// SIGTERM equivalent: close cleanly.
    Terminate,
    /// SIGHUP equivalent: close cleanly.
    Hangup,
    /// Unrecoverable owner fault: best-effort close, then fail.
    Fatal,
}

/// How `run_owner_lease` terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerShutdown {
    /// Clean lifecycle event closed and destroyed the room.
    CleanClose,
    /// Delivery to the (gone) creator closed and destroyed the room.
    DeliveryAborted,
    /// Reconnect never succeeded inside the owner grace.
    GraceExpired,
}

/// Owner client configuration. `owner_grace_secs` must not exceed the
/// server's `--web-transfer-owner-grace` (the client gives up first, which is
/// the safe direction); the native wire carries no grace field.
#[derive(Debug, Clone)]
pub struct OwnerClientConfig {
    /// Server endpoint (`--to` value).
    pub endpoint: String,
    /// Server authentication secret, when the server requires one.
    pub secret: Option<String>,
    /// Skip TLS verification (self-signed private deployments).
    pub insecure: bool,
    /// Open the room URL in the default browser after delivery.
    pub open_browser: bool,
    /// Resume deadline after the first loss (mirrors the server grace).
    pub owner_grace_secs: u64,
}

impl Default for OwnerClientConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            secret: None,
            insecure: false,
            open_browser: false,
            owner_grace_secs: 60,
        }
    }
}

/// A created room: the capability URL plus its public room ID. `Debug` shows
/// the room ID only so logs can carry this value safely.
pub struct CreatedRoom {
    /// Full capability URL (fragment holds member token and room key).
    pub display_url: String,
    /// Room ID (URL path; safe to log).
    pub room_id: RoomId,
}

impl fmt::Debug for CreatedRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CreatedRoom {{ room={} }}", self.room_id)
    }
}

/// Room secrets, generated once per `run_owner_lease` and retained across
/// reconnects. Only hashes cross the control stream.
struct OwnerSecrets {
    member: MemberToken,
    owner: OwnerToken,
    key: RoomKey,
}

impl OwnerSecrets {
    fn generate() -> Self {
        use ring::rand::{SecureRandom, SystemRandom};
        let random = SystemRandom::new();
        let mut bytes = [0u8; 32];
        let mut fill = || {
            random.fill(&mut bytes).expect("OS CSPRNG");
            bytes
        };
        Self {
            member: MemberToken::from_bytes(fill()),
            owner: OwnerToken::from_bytes(fill()),
            key: RoomKey::from_bytes(fill()),
        }
    }
}

/// Builds the capability URL: origin + `/transfer/<room>` + fragment secrets.
/// Pure, so the exact shape is unit-pinned without a network.
pub fn build_display_url(
    origin: &str,
    room_id: RoomId,
    member: &MemberToken,
    key: &RoomKey,
) -> String {
    format!("{origin}/transfer/{room_id}#m={member}&k={key}")
}

/// Resume delay for attempt `n` (0-based): ladder, then the ceiling holds.
pub fn resume_delay_ms(attempt: u32) -> u64 {
    RESUME_BACKOFF
        .get(attempt as usize)
        .copied()
        .unwrap_or(RESUME_BACKOFF_MAX_MS)
}

/// Maps one heartbeat write onto loop control: only `Sent` continues; a
/// closed or unread control reconnects (never silently disables beating).
pub(crate) fn beat_action(beat: CtrlBeat) -> bool {
    beat == CtrlBeat::Sent
}

/// Holds one owner control connection for its heartbeat/read/lifecycle loop.
struct OwnerSession<S> {
    control: Delimited<S>,
    room_id: RoomId,
    epoch: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> OwnerSession<S> {
    /// Sends a graceful close, bounded so a dead peer cannot pin shutdown.
    async fn close_bounded(&mut self) {
        let _ = tokio::time::timeout(
            OWNER_CLOSE_TIMEOUT,
            self.control.send(ClientMessage::CloseWebTransferRoom {
                room_id: self.room_id,
                owner_epoch: self.epoch,
            }),
        )
        .await;
    }
}

/// How the heartbeat phase ended.
enum HeartbeatEnd {
    /// Transport lost: enter the resume loop.
    Lost,
    /// Clean lifecycle event (or dropped event source): closed, done.
    Done,
}

/// Opens one owner control connection and runs create-or-resume on it:
/// transport, yamux client, control substream, first message, then the
/// optional server-secret handshake. Mirrors `client::open_carrier` order.
async fn owner_connect(
    endpoint: &Endpoint,
    insecure: bool,
    secret: Option<&str>,
    first: ClientMessage,
) -> Result<(Delimited<mux::Stream>, ServerMessage)> {
    let socket = transport::connect(endpoint, insecure)
        .await
        .with_context(|| format!("owner could not reach {}", endpoint.host))?;
    let (opener, _acceptor) = mux::client(socket);
    let mut control = Delimited::with_label(
        opener
            .open()
            .await
            .context("owner could not open control stream")?,
        "client/web-transfer-owner",
    );
    control
        .send(first)
        .await
        .context("owner could not send open")?;
    if let Some(secret) = secret {
        Authenticator::new(secret)
            .client_handshake(&mut control)
            .await?;
    }
    let reply = control
        .recv_timeout::<ServerMessage>()
        .await
        .context("owner got no open reply")?
        .context("server closed the owner control")?;
    Ok((control, reply))
}

/// Runs one heartbeat/read/lifecycle phase on a live session. Returns when
/// the transport is lost (resume next) or a clean event closed the room.
async fn heartbeat_phase<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut OwnerSession<S>,
    lifecycle_rx: &mut mpsc::Receiver<OwnerLifecycle>,
) -> Result<HeartbeatEnd> {
    let mut beat = tokio::time::interval(OWNER_HEARTBEAT);
    // The first tick fires immediately; skip it so beats wait a full period.
    beat.tick().await;
    loop {
        tokio::select! {
            _ = beat.tick() => {
                if !beat_action(beat_once(&mut session.control).await) {
                    tracing::warn!(room = %session.room_id, "owner control lost, resuming");
                    return Ok(HeartbeatEnd::Lost);
                }
            }
            msg = session.control.recv::<ServerMessage>() => {
                match msg {
                    Err(err) => {
                        tracing::warn!(room = %session.room_id, "owner control read failed, resuming: {err:#}");
                        return Ok(HeartbeatEnd::Lost);
                    }
                    Ok(None) => {
                        tracing::warn!(room = %session.room_id, "owner control closed, resuming");
                        return Ok(HeartbeatEnd::Lost);
                    }
                    Ok(Some(ServerMessage::Error(err))) => {
                        bail!("server error for room {}: {err}", session.room_id);
                    }
                    Ok(Some(_)) => {}
                }
            }
            event = lifecycle_rx.recv() => {
                match event {
                    Some(OwnerLifecycle::Fatal) => {
                        session.close_bounded().await;
                        bail!("owner fatal for room {}", session.room_id);
                    }
                    // `None` (senders dropped) closes cleanly: nobody can ask
                    // for anything else anymore.
                    _ => {
                        session.close_bounded().await;
                        return Ok(HeartbeatEnd::Done);
                    }
                }
            }
        }
    }
}

/// How the resume phase ended.
enum ResumeEnd<S> {
    /// Re-attached under a fresh epoch; heartbeat resumes.
    Resumed(OwnerSession<S>),
    /// A clean event fired mid-backoff: give up (server grace expires it).
    Clean,
    /// The grace ran out with no successful resume.
    Expired,
}

/// Reconnects with resume-only backoff until the grace expires. This path
/// sends `ResumeWebTransferRoom` and nothing else — a replacement room is
/// never created silently. `connect` is injected so unit tests drive the
/// same path over a duplex pair.
async fn resume_phase<S, C, F>(
    connect: &mut C,
    owner_token: OwnerToken,
    room_id: RoomId,
    first_loss: Instant,
    grace: Duration,
    lifecycle_rx: &mut mpsc::Receiver<OwnerLifecycle>,
) -> Result<ResumeEnd<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut(ClientMessage) -> F,
    F: Future<Output = Result<(Delimited<S>, ServerMessage)>>,
{
    let mut attempt: u32 = 0;
    loop {
        if first_loss.elapsed() >= grace {
            return Ok(ResumeEnd::Expired);
        }
        let delay = resume_delay_ms(attempt);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            event = lifecycle_rx.recv() => {
                match event {
                    Some(OwnerLifecycle::Fatal) => bail!("owner fatal for room {room_id}"),
                    _ => return Ok(ResumeEnd::Clean),
                }
            }
        }
        match connect(ClientMessage::ResumeWebTransferRoom {
            version: PROTOCOL_VERSION,
            room_id,
            owner_token,
        })
        .await
        {
            Ok((control, ServerMessage::WebTransferRoomResumed { owner_epoch, .. })) => {
                return Ok(ResumeEnd::Resumed(OwnerSession {
                    control,
                    room_id,
                    epoch: owner_epoch,
                }));
            }
            Ok((_, ServerMessage::Error(err))) => {
                tracing::warn!(room = %room_id, "resume refused, retrying: {err}");
            }
            Ok(_) => {
                tracing::warn!(room = %room_id, "unexpected resume reply, retrying");
            }
            Err(err) => {
                tracing::warn!(room = %room_id, "reconnect failed, retrying: {err:#}");
            }
        }
    }
}

/// Runs the owner lease over an injected connector (production passes the
/// real dialer; unit tests pass a duplex fake). See [`run_owner_lease`].
async fn run_owner_lease_with<S, C, F>(
    config: &OwnerClientConfig,
    created_tx: oneshot::Sender<CreatedRoom>,
    lifecycle_rx: &mut mpsc::Receiver<OwnerLifecycle>,
    connect: &mut C,
) -> Result<OwnerShutdown>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut(ClientMessage) -> F,
    F: Future<Output = Result<(Delimited<S>, ServerMessage)>>,
{
    let secrets = OwnerSecrets::generate();
    let member_hash = secrets.member.sha256_hash();
    let owner_hash = secrets.owner.sha256_hash();
    let grace = Duration::from_secs(config.owner_grace_secs);

    let (control, reply) = connect(ClientMessage::CreateWebTransferRoom {
        version: PROTOCOL_VERSION,
        member_token_hash: member_hash,
        owner_token_hash: owner_hash,
    })
    .await?;
    let (room_id, epoch, base_url) = match reply {
        ServerMessage::WebTransferRoomCreated {
            room_id,
            base_url,
            owner_epoch,
            ..
        } => (room_id, owner_epoch, base_url),
        ServerMessage::Error(err) => bail!("server refused room creation: {err}"),
        other => bail!("unexpected open reply: {}", other.control_frame_summary()),
    };
    let display_url = build_display_url(&base_url, room_id, &secrets.member, &secrets.key);
    if created_tx
        .send(CreatedRoom {
            display_url: display_url.clone(),
            room_id,
        })
        .is_err()
    {
        // The creator is gone: the room has no purpose left. Best-effort
        // close bounded to one second, then out.
        let mut session = OwnerSession {
            control,
            room_id,
            epoch,
        };
        session.close_bounded().await;
        return Ok(OwnerShutdown::DeliveryAborted);
    }
    if config.open_browser {
        // Delivery already succeeded; a browser failure must not fail the
        // lease, and the URL itself never reaches the log.
        if let Err(err) = webbrowser::open(&display_url) {
            tracing::warn!(room = %room_id, "could not open browser: {err}");
        }
    }

    let mut session = OwnerSession {
        control,
        room_id,
        epoch,
    };
    let mut first_loss: Option<Instant> = None;
    loop {
        match heartbeat_phase(&mut session, lifecycle_rx).await? {
            HeartbeatEnd::Done => return Ok(OwnerShutdown::CleanClose),
            HeartbeatEnd::Lost => {}
        }
        if first_loss.is_none() {
            first_loss = Some(Instant::now());
        }
        match resume_phase(
            connect,
            secrets.owner,
            session.room_id,
            first_loss.expect("loss is always armed here"),
            grace,
            lifecycle_rx,
        )
        .await?
        {
            ResumeEnd::Resumed(resumed) => {
                session = resumed;
                first_loss = None;
            }
            ResumeEnd::Clean => return Ok(OwnerShutdown::CleanClose),
            ResumeEnd::Expired => return Ok(OwnerShutdown::GraceExpired),
        }
    }
}

/// Runs the owner lease: create once, heartbeat, resume across resets within
/// the grace, destroy on clean lifecycle events. Returns how it terminated.
pub async fn run_owner_lease(
    config: OwnerClientConfig,
    created_tx: oneshot::Sender<CreatedRoom>,
    mut lifecycle_rx: mpsc::Receiver<OwnerLifecycle>,
) -> Result<OwnerShutdown> {
    let endpoint = Endpoint::parse(&config.endpoint);
    let endpoint_ref = &endpoint;
    let insecure = config.insecure;
    let secret = config.secret.clone();
    let mut connect = |first: ClientMessage| {
        let secret = secret.clone();
        async move { owner_connect(endpoint_ref, insecure, secret.as_deref(), first).await }
    };
    run_owner_lease_with(&config, created_tx, &mut lifecycle_rx, &mut connect).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_secrets_are_generated_once_and_only_hashes_are_created() {
        let first = OwnerSecrets::generate();
        let second = OwnerSecrets::generate();
        assert_ne!(first.member.sha256_hash(), second.member.sha256_hash());
        assert_ne!(first.owner.sha256_hash(), second.owner.sha256_hash());
        // The create message carries hashes, comparable server-side, while the
        // raw values stay in this scope.
        assert_eq!(first.member.sha256_hash().len(), 32);
        assert_eq!(first.owner.sha256_hash().len(), 32);
    }

    #[test]
    fn owner_url_has_exact_path_and_fragment() {
        let room = RoomId::from_bytes([0xabu8; 16]);
        let member = MemberToken::from_bytes([0x11u8; 32]);
        let key = RoomKey::from_bytes([0x33u8; 32]);
        assert_eq!(
            build_display_url("https://files.example.com", room, &member, &key),
            format!(
                "https://files.example.com/transfer/{}#m={}&k={}",
                "ab".repeat(16),
                "11".repeat(32),
                "33".repeat(32)
            )
        );
    }

    #[test]
    fn owner_logs_and_errors_redact_all_secrets() {
        let room = RoomId::from_bytes([0xabu8; 16]);
        let created = CreatedRoom {
            display_url: "https://h/transfer/ab#m=11&k=33".to_string(),
            room_id: room,
        };
        let debug = format!("{created:?}");
        assert!(!debug.contains("11"), "display URL leaks: {debug}");
        assert!(debug.contains(&"ab".repeat(16)), "room ID stays: {debug}");
    }

    #[tokio::test]
    async fn heartbeat_uses_bounded_beat_once() {
        assert_eq!(OWNER_HEARTBEAT, Duration::from_secs(20));
        assert_eq!(
            crate::secret::ctrl_heartbeat_send_timeout(),
            Duration::from_secs(10)
        );
        // The exact bounded primitive, wired: Sent over a live control.
        let (a, b) = tokio::io::duplex(65536);
        let mut client = Delimited::new(a);
        let mut server = Delimited::new(b);
        assert_eq!(beat_once(&mut client).await, CtrlBeat::Sent);
        assert!(matches!(
            server.recv::<ClientMessage>().await.unwrap(),
            Some(ClientMessage::Heartbeat)
        ));
    }

    #[test]
    fn closed_and_peer_not_reading_enter_resume_backoff() {
        assert!(beat_action(CtrlBeat::Sent));
        assert!(!beat_action(CtrlBeat::Closed));
        assert!(!beat_action(CtrlBeat::PeerNotReading));
    }

    #[test]
    fn resume_backoff_sequence_and_deadline_are_exact() {
        assert_eq!(
            (0..7).map(resume_delay_ms).collect::<Vec<_>>(),
            vec![250, 500, 1000, 2000, 4000, 5000, 5000]
        );
        let first_loss = Instant::now();
        let grace = Duration::from_secs(60);
        assert!(first_loss.elapsed() < grace);
    }

    #[test]
    fn resume_never_creates_a_new_room() {
        // The resume path builds exactly one message shape: Resume. The
        // create message is built once, at lease start, and never again —
        // pin both constructors so a refactor cannot swap them silently.
        let room = RoomId::from_bytes([1u8; 16]);
        let token = OwnerToken::from_bytes([2u8; 32]);
        let resume = ClientMessage::ResumeWebTransferRoom {
            version: 1,
            room_id: room,
            owner_token: token,
        };
        assert!(matches!(
            resume,
            ClientMessage::ResumeWebTransferRoom { .. }
        ));
        let create = ClientMessage::CreateWebTransferRoom {
            version: 1,
            member_token_hash: [3u8; 32],
            owner_token_hash: [4u8; 32],
        };
        assert!(matches!(
            create,
            ClientMessage::CreateWebTransferRoom { .. }
        ));
    }

    /// Fake connector over fresh duplex pairs: answers Create with a fixed
    /// room, records every Close. Drives `run_owner_lease_with` with zero
    /// sockets; the recording server task per connection counts closes.
    struct FakeConnector {
        room: RoomId,
        closes: std::sync::Arc<tokio::sync::Mutex<Vec<RoomId>>>,
    }

    impl FakeConnector {
        async fn call(
            self: &std::sync::Arc<Self>,
            first: ClientMessage,
        ) -> Result<(Delimited<tokio::io::DuplexStream>, ServerMessage)> {
            match first {
                ClientMessage::CreateWebTransferRoom { .. } => {
                    let (run_io, srv_io) = tokio::io::duplex(65536);
                    let mut srv = Delimited::new(srv_io);
                    let closes = std::sync::Arc::clone(&self.closes);
                    tokio::spawn(async move {
                        while let Ok(Some(msg)) = srv.recv::<ClientMessage>().await {
                            if let ClientMessage::CloseWebTransferRoom { room_id, .. } = msg {
                                closes.lock().await.push(room_id);
                            }
                        }
                    });
                    Ok((
                        Delimited::new(run_io),
                        ServerMessage::WebTransferRoomCreated {
                            version: PROTOCOL_VERSION,
                            room_id: self.room,
                            base_url: "http://127.0.0.1:8080".to_string(),
                            owner_epoch: 0,
                        },
                    ))
                }
                other => bail!(
                    "fake connector only opens rooms, got {}",
                    other.control_frame_summary()
                ),
            }
        }
    }

    fn fake_pair() -> (FakeConnector, OwnerClientConfig) {
        (
            FakeConnector {
                room: RoomId::from_bytes([0x77u8; 16]),
                closes: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
            },
            OwnerClientConfig::default(),
        )
    }

    #[tokio::test]
    async fn delivery_failure_closes_created_room() {
        let (fake, config) = fake_pair();
        let fake = std::sync::Arc::new(fake);
        let (created_tx, created_rx) = oneshot::channel();
        let (_lifecycle_tx, mut lifecycle_rx) = mpsc::channel(4);
        // Receiver dropped before the run: delivery fails, the room dies.
        drop(created_rx);
        let mut connect = {
            let fake = std::sync::Arc::clone(&fake);
            move |first: ClientMessage| {
                let fake = std::sync::Arc::clone(&fake);
                async move { fake.call(first).await }
            }
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            run_owner_lease_with(&config, created_tx, &mut lifecycle_rx, &mut connect),
        )
        .await
        .expect("run ends")
        .unwrap();
        assert_eq!(outcome, OwnerShutdown::DeliveryAborted);
        // Exactly one close, for the created room.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*fake.closes.lock().await, vec![fake.room]);
    }

    #[tokio::test]
    async fn clean_lifecycle_event_sends_close_once_with_one_second_bound() {
        let (fake, config) = fake_pair();
        let fake = std::sync::Arc::new(fake);
        let (created_tx, created_rx) = oneshot::channel();
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::channel(4);
        // Interrupt as soon as the room is delivered (separate task: the run
        // future below borrows the connector).
        let interrupter = tokio::spawn(async move {
            let created: CreatedRoom = created_rx.await.expect("created delivered");
            lifecycle_tx.send(OwnerLifecycle::Interrupt).await.unwrap();
            created.room_id
        });
        let mut connect = {
            let fake = std::sync::Arc::clone(&fake);
            move |first: ClientMessage| {
                let fake = std::sync::Arc::clone(&fake);
                async move { fake.call(first).await }
            }
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            run_owner_lease_with(&config, created_tx, &mut lifecycle_rx, &mut connect),
        )
        .await
        .expect("run ends")
        .unwrap();
        assert_eq!(outcome, OwnerShutdown::CleanClose);
        let room = interrupter.await.unwrap();
        assert_eq!(room, fake.room);
        // Exactly one close inside a fraction of the one-second bound.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*fake.closes.lock().await, vec![room]);
    }
}
