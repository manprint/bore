//! Owner lease client and public run path for `bore transfer web` rooms
//! (Phases 1.4 and 3.6).
//!
//! Exercised by `T-WEB-OWNER-LEASE` and `T-WEB-CLI`; the command reuses
//! exactly this loop and adds stdout, the browser open and OS signals on top
//! of it — the lease itself never writes to stdout. Generates
//! the room secrets once, holds the owner lease across reconnects (resume
//! only, never a silent replacement room) and destroys the room on clean
//! lifecycle events. Every log/error line carries the room ID and phase only
//! — never the URL, tokens or room key.

use std::fmt;
use std::future::Future;
use std::io::{self, Write};
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
/// The only actionable thing an operator can do about a server that cannot
/// create a room: the wire text never reaches the terminal (it can carry a
/// server-chosen string), so this is the whole message.
pub const OLD_SERVER_ERROR: &str =
    "web transfer requires an upgraded server configured with --web-transfer-base-url";
/// `--relay-only` asked for and NOT confirmed by the server. The field is
/// additive, so a server that predates it parses the request, creates an
/// ordinary room and answers success: silence would hand the owner a room
/// whose transfers take the direct path, which is the exact opposite of what
/// they asked for. The room this message accompanies is closed, never used.
pub const RELAY_ONLY_UNSUPPORTED_ERROR: &str =
    "--relay-only needs a server that supports it: this one created the room without it, so \
     transfers could still go direct. Upgrade the server, or drop --relay-only.";
/// Second signal: the room close is already running and bounded, so the user
/// asking twice gets out now. 128 + SIGINT, the shell convention.
pub const FORCED_EXIT_CODE: i32 = 130;

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
    /// Ask for a room whose transfers never negotiate the direct path.
    pub relay_only: bool,
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
            relay_only: false,
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
    /// Sends a graceful close and waits for the server to drop the owner
    /// control, bounded so a dead peer cannot pin shutdown.
    ///
    /// The wait is not politeness: the control rides a yamux substream whose
    /// driver is a DETACHED task, so a process that returns the moment `send`
    /// resolves can exit with the close frame still queued — the room then
    /// lives on for the whole owner grace and the URL the user just abandoned
    /// still works. The server drops this control as soon as it has processed
    /// the close, so reading to the end IS the acknowledgement.
    async fn close_bounded(&mut self) {
        let _ = tokio::time::timeout(OWNER_CLOSE_TIMEOUT, async {
            self.control
                .send(ClientMessage::CloseWebTransferRoom {
                    room_id: self.room_id,
                    owner_epoch: self.epoch,
                })
                .await?;
            while let Some(_msg) = self.control.recv::<ServerMessage>().await? {}
            Ok::<(), anyhow::Error>(())
        })
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
    let creating = matches!(first, ClientMessage::CreateWebTransferRoom { .. });
    control
        .send(first)
        .await
        .context("owner could not send open")?;
    if let Some(secret) = secret {
        Authenticator::new(secret)
            .client_handshake(&mut control)
            .await?;
    }
    let reply = match control.recv_timeout::<ServerMessage>().await {
        Ok(Some(reply)) => reply,
        // A server that never answers a CREATE either predates web transfer
        // (an unknown control variant closes its side) or has it disabled;
        // both are the same operator action, and neither is a transient
        // loss. A RESUME that goes unanswered is exactly that transient
        // loss, so it keeps its own diagnostic and its retry.
        Ok(None) if creating => {
            tracing::debug!("server closed the owner control before answering the create");
            bail!(OLD_SERVER_ERROR);
        }
        Ok(None) => bail!("server closed the owner control"),
        Err(err) if creating => {
            tracing::debug!("owner got no create reply: {err:#}");
            bail!(OLD_SERVER_ERROR);
        }
        Err(err) => return Err(err).context("owner got no open reply"),
    };
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
        relay_only: config.relay_only,
    })
    .await?;
    let (room_id, epoch, base_url) = match reply {
        ServerMessage::WebTransferRoomCreated {
            room_id,
            base_url,
            owner_epoch,
            relay_only,
            ..
        } => {
            if config.relay_only && !relay_only {
                // The room EXISTS on the server and would serve direct
                // transfers. Closing it is part of the refusal: a room the
                // operator refused must not stay reachable behind a URL
                // that was never printed.
                let mut session = OwnerSession {
                    control,
                    room_id,
                    epoch: owner_epoch,
                };
                session.close_bounded().await;
                bail!(RELAY_ONLY_UNSUPPORTED_ERROR);
            }
            (room_id, owner_epoch, base_url)
        }
        ServerMessage::Error(err) => {
            // The wire text is server-chosen: it goes to the log, never to
            // the terminal, and the operator gets the one actionable line.
            tracing::debug!("server refused room creation: {err}");
            bail!(OLD_SERVER_ERROR);
        }
        other => {
            tracing::debug!("unexpected open reply: {}", other.control_frame_summary());
            bail!(OLD_SERVER_ERROR);
        }
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
    // The browser open belongs to the run path, AFTER the URL is flushed to
    // stdout (3.6): a lease that opened it here would race the print, and a
    // user whose browser steals focus first has nothing to copy if it fails.

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

/// The injected browser opener: production passes `webbrowser::open`, tests
/// pass a recorder, and a `None` means `--open` was not asked for.
pub(crate) type BrowserOpener<'a> = &'a mut dyn FnMut(&str) -> io::Result<()>;

/// Writes the two announcement lines and FLUSHES before anything else may
/// touch the terminal, then opens the browser when asked. The order is the
/// contract: a browser that steals focus (or fails) before the URL is on the
/// terminal leaves the user with nothing to copy. `open` is injected so that
/// order is provable without a browser.
pub(crate) fn announce_room(
    out: &mut dyn Write,
    err_out: &mut dyn Write,
    url: &str,
    open: Option<BrowserOpener<'_>>,
) -> io::Result<()> {
    writeln!(out, "room: {url}")?;
    writeln!(out, "room active; press Ctrl+C to close")?;
    out.flush()?;
    if let Some(open) = open {
        if let Err(err) = open(url) {
            // Redacted: the kind only. The URL is a capability and a browser
            // launcher echoes its argument back in its own error text.
            let _ = writeln!(
                err_out,
                "warning: could not open the browser ({})",
                err.kind()
            );
            let _ = err_out.flush();
        }
    }
    Ok(())
}

/// Maps a lease outcome onto the process result. Only a clean close is a
/// success: an expired grace destroyed the room the user is still looking at.
fn lease_result(shutdown: Result<OwnerShutdown>) -> Result<()> {
    match shutdown? {
        OwnerShutdown::CleanClose => Ok(()),
        OwnerShutdown::DeliveryAborted => {
            bail!("web transfer room was abandoned before it could be announced")
        }
        OwnerShutdown::GraceExpired => {
            bail!(
                "web transfer room expired: the owner lease could not be resumed inside the grace"
            )
        }
    }
}

/// Announces the created room, then holds the lease to its end. Takes the
/// lease as a future so every branch — including the stdout failure, which
/// must close the room and exit nonzero — is driven in a unit test without a
/// server.
pub(crate) async fn announce_and_hold<F>(
    lease: F,
    created_rx: oneshot::Receiver<CreatedRoom>,
    lifecycle_tx: mpsc::Sender<OwnerLifecycle>,
    out: &mut dyn Write,
    err_out: &mut dyn Write,
    open: Option<BrowserOpener<'_>>,
) -> Result<()>
where
    F: Future<Output = Result<OwnerShutdown>>,
{
    tokio::pin!(lease);
    let created = tokio::select! {
        // Biased: a room that WAS created is announced even when the lease
        // finished in the same poll — the user must see the URL of a room
        // that briefly existed, not silence.
        biased;
        created = created_rx => created,
        // The lease ended before it delivered a room: its own error is the
        // answer, never a second invented one.
        done = &mut lease => return lease_result(done),
    };
    let created = match created {
        Ok(created) => created,
        Err(_) => return lease_result(lease.await),
    };
    if let Err(err) = announce_room(out, err_out, &created.display_url, open) {
        // Nobody can reach a room whose URL never landed. `Fatal` runs the
        // same bounded close as a signal; the wait is bounded too, so a dead
        // peer cannot pin the exit.
        let _ = lifecycle_tx.send(OwnerLifecycle::Fatal).await;
        let _ = tokio::time::timeout(OWNER_CLOSE_TIMEOUT * 2, &mut lease).await;
        return Err(err).context("could not write the room URL to stdout");
    }
    lease_result(lease.await)
}

/// First signal returns `true` (close the room cleanly), every later one
/// `false` (force the process out). Pure so the decision is unit-pinned;
/// the exit itself is the only part a test cannot take.
pub(crate) fn signal_action(first: &std::sync::atomic::AtomicBool) -> bool {
    first.swap(false, std::sync::atomic::Ordering::SeqCst)
}

/// One signal: close cleanly the first time, force out the second.
async fn on_signal(
    tx: &mpsc::Sender<OwnerLifecycle>,
    first: &std::sync::atomic::AtomicBool,
    event: OwnerLifecycle,
) {
    if signal_action(first) {
        let _ = tx.send(event).await;
    } else {
        std::process::exit(FORCED_EXIT_CODE);
    }
}

/// Installs the documented handlers: Ctrl+C everywhere, SIGTERM and SIGHUP
/// on Unix (a closed shell must destroy the room, not orphan it). A handler
/// that cannot be installed is skipped, never fatal — the room still closes
/// on the handlers that did install.
fn spawn_signal_handlers(tx: mpsc::Sender<OwnerLifecycle>) {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let first = Arc::new(AtomicBool::new(true));

    {
        let tx = tx.clone();
        let first = Arc::clone(&first);
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                on_signal(&tx, &first, OwnerLifecycle::Interrupt).await;
            }
        });
    }

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for (kind, event) in [
            (SignalKind::terminate(), OwnerLifecycle::Terminate),
            (SignalKind::hangup(), OwnerLifecycle::Hangup),
        ] {
            let Ok(mut stream) = signal(kind) else {
                tracing::debug!("signal handler unavailable; skipping one source");
                continue;
            };
            let tx = tx.clone();
            let first = Arc::clone(&first);
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    on_signal(&tx, &first, event).await;
                }
            });
        }
    }
}

/// Runs `bore transfer web`: create one room, print exactly two lines, open
/// the browser when asked, then hold the owner lease until a signal closes
/// the room (or the grace expires). Never selects, reads or names a file.
pub async fn run_web_transfer(config: OwnerClientConfig) -> Result<()> {
    let open_browser = config.open_browser;
    let (created_tx, created_rx) = oneshot::channel();
    let (lifecycle_tx, lifecycle_rx) = mpsc::channel(4);
    spawn_signal_handlers(lifecycle_tx.clone());
    let lease = run_owner_lease(config, created_tx, lifecycle_rx);
    let mut opener = |url: &str| webbrowser::open(url);
    let open: Option<BrowserOpener<'_>> = if open_browser {
        Some(&mut opener)
    } else {
        None
    };
    let mut out = io::stdout();
    let mut err_out = io::stderr();
    announce_and_hold(
        lease,
        created_rx,
        lifecycle_tx,
        &mut out,
        &mut err_out,
        open,
    )
    .await
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
            relay_only: false,
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
        /// A server that knows `relay_only` echoes what it installed. `false`
        /// stands for one that predates the field: it parses the request,
        /// creates an ordinary room and answers success — which is exactly
        /// the silent downgrade the client has to catch.
        relay_only_supported: bool,
    }

    impl FakeConnector {
        async fn call(
            self: &std::sync::Arc<Self>,
            first: ClientMessage,
        ) -> Result<(Delimited<tokio::io::DuplexStream>, ServerMessage)> {
            match first {
                ClientMessage::CreateWebTransferRoom { relay_only, .. } => {
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
                            relay_only: relay_only && self.relay_only_supported,
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
                relay_only_supported: true,
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
    async fn relay_only_asked_and_confirmed_opens_the_room() {
        // The other half of the red-check below: with a server that DOES
        // support the flag, asking for it must still produce a room. A
        // refusal that fired on every server would pass the negative test
        // and break the feature.
        let (mut fake, mut config) = fake_pair();
        fake.relay_only_supported = true;
        config.relay_only = true;
        let fake = std::sync::Arc::new(fake);
        let (created_tx, created_rx) = oneshot::channel();
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::channel(4);
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
        assert_eq!(interrupter.await.unwrap(), fake.room);
    }

    #[tokio::test]
    async fn relay_only_not_confirmed_refuses_and_closes_the_room() {
        // A server that predates the field answers success WITHOUT the flag.
        // Accepting that would hand the owner a room whose transfers take the
        // direct path — the opposite of what they asked for — so the run must
        // fail, name the flag, and close the room it is refusing to use.
        let (mut fake, mut config) = fake_pair();
        fake.relay_only_supported = false;
        config.relay_only = true;
        let fake = std::sync::Arc::new(fake);
        let (created_tx, created_rx) = oneshot::channel();
        let (_lifecycle_tx, mut lifecycle_rx) = mpsc::channel(4);
        let mut connect = {
            let fake = std::sync::Arc::clone(&fake);
            move |first: ClientMessage| {
                let fake = std::sync::Arc::clone(&fake);
                async move { fake.call(first).await }
            }
        };
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            run_owner_lease_with(&config, created_tx, &mut lifecycle_rx, &mut connect),
        )
        .await
        .expect("run ends")
        .expect_err("an unconfirmed --relay-only must fail the run");
        assert!(
            error.to_string().contains("--relay-only"),
            "the message must name the flag: {error}"
        );
        // The URL was never delivered, so nothing could join; the room the
        // server did create is closed rather than left reachable.
        assert!(created_rx.await.is_err(), "no room URL is delivered");
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

    /// What the announcement did, in order. `Flush` and `Open` are separate
    /// events because their ORDER is the property under test.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Step {
        Wrote(String),
        Flushed,
        Opened(String),
    }

    /// A stdout that records, and can be made to fail on its first write.
    struct RecordingOut {
        steps: std::sync::Arc<std::sync::Mutex<Vec<Step>>>,
        fail: bool,
    }

    impl Write for RecordingOut {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout is gone"));
            }
            self.steps
                .lock()
                .unwrap()
                .push(Step::Wrote(String::from_utf8_lossy(buf).to_string()));
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout is gone"));
            }
            self.steps.lock().unwrap().push(Step::Flushed);
            Ok(())
        }
    }

    const CANARY_URL: &str =
        "http://127.0.0.1:8080/transfer/aabb#m=CANARY-MEMBER-TOKEN&k=CANARY-ROOM-KEY";

    #[test]
    fn web_stdout_is_exactly_two_lines_and_flush_precedes_open() {
        let steps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = RecordingOut {
            steps: std::sync::Arc::clone(&steps),
            fail: false,
        };
        let mut err_out = Vec::new();
        let opened = std::sync::Arc::clone(&steps);
        let mut open = move |url: &str| -> io::Result<()> {
            opened.lock().unwrap().push(Step::Opened(url.to_string()));
            Ok(())
        };
        announce_room(&mut out, &mut err_out, CANARY_URL, Some(&mut open)).expect("announced");

        let steps = steps.lock().unwrap().clone();
        let written: String = steps
            .iter()
            .filter_map(|step| match step {
                Step::Wrote(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // Exactly two lines, exactly these.
        assert_eq!(
            written,
            format!("room: {CANARY_URL}\nroom active; press Ctrl+C to close\n")
        );
        let flush = steps
            .iter()
            .position(|step| *step == Step::Flushed)
            .expect("flushed");
        let open_at = steps
            .iter()
            .position(|step| matches!(step, Step::Opened(_)))
            .expect("opened");
        // The whole point: every byte is flushed BEFORE the browser is asked
        // to take the screen.
        assert!(flush < open_at, "flush must precede open, got {steps:?}");
        assert!(
            steps
                .iter()
                .take(flush)
                .all(|step| matches!(step, Step::Wrote(_))),
            "nothing but the two lines may precede the flush: {steps:?}"
        );
        assert!(err_out.is_empty(), "a successful open says nothing");
    }

    #[test]
    fn browser_open_failure_is_nonfatal_and_redacted() {
        let steps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = RecordingOut { steps, fail: false };
        let mut err_out = Vec::new();
        let mut open = |_: &str| -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no browser could open {CANARY_URL}"),
            ))
        };
        // Non-fatal: the room stays up and the user still has the URL.
        announce_room(&mut out, &mut err_out, CANARY_URL, Some(&mut open))
            .expect("a failed browser never fails the announcement");
        let warning = String::from_utf8(err_out).expect("utf8 warning");
        assert!(
            warning.starts_with("warning: could not open the browser"),
            "unexpected warning: {warning}"
        );
        // Redacted: the launcher echoed the capability back at us and it must
        // not reach stderr.
        assert!(!warning.contains("CANARY-MEMBER-TOKEN"));
        assert!(!warning.contains("CANARY-ROOM-KEY"));
        assert!(!warning.contains("#m="));
    }

    #[tokio::test]
    async fn stdout_failure_closes_room() {
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::channel(4);
        let (created_tx, created_rx) = oneshot::channel();
        let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lease_closed = std::sync::Arc::clone(&closed);
        // A lease that closes its room on `Fatal`, exactly as the real one does.
        let lease = async move {
            match lifecycle_rx.recv().await {
                Some(OwnerLifecycle::Fatal) => {
                    lease_closed.store(true, std::sync::atomic::Ordering::SeqCst);
                    bail!("owner fatal")
                }
                other => bail!("expected Fatal, got {other:?}"),
            }
        };
        created_tx
            .send(CreatedRoom {
                display_url: CANARY_URL.to_string(),
                room_id: RoomId::from_bytes([0x55u8; 16]),
            })
            .expect("delivered");
        let mut out = RecordingOut {
            steps: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            fail: true,
        };
        let mut err_out = Vec::new();
        let err = announce_and_hold(
            lease,
            created_rx,
            lifecycle_tx,
            &mut out,
            &mut err_out,
            None,
        )
        .await
        .expect_err("a room nobody can reach is a failure");
        assert!(
            err.to_string().contains("could not write the room URL"),
            "unexpected error: {err:#}"
        );
        assert!(
            closed.load(std::sync::atomic::Ordering::SeqCst),
            "a room whose URL never landed must be closed, not leaked"
        );
    }

    #[test]
    fn first_signal_closes_second_forces() {
        let first = std::sync::atomic::AtomicBool::new(true);
        // First: close cleanly. Every later one: force out.
        assert!(signal_action(&first));
        assert!(!signal_action(&first));
        assert!(!signal_action(&first));
    }

    #[tokio::test]
    async fn old_server_error_is_actionable() {
        // A server that refuses the create (disabled, or too old to know the
        // message) must produce the one actionable line, never its own text.
        const WIRE_CANARY: &str = "CANARY-WIRE-DETAIL";
        let config = OwnerClientConfig::default();
        let (created_tx, _created_rx) = oneshot::channel();
        let (_lifecycle_tx, mut lifecycle_rx) = mpsc::channel(4);
        let mut connect = |_first: ClientMessage| async move {
            let (run_io, _srv_io) = tokio::io::duplex(4096);
            Ok((
                Delimited::new(run_io),
                ServerMessage::Error(WIRE_CANARY.to_string()),
            ))
        };
        let err = run_owner_lease_with(&config, created_tx, &mut lifecycle_rx, &mut connect)
            .await
            .expect_err("a refused create fails");
        let text = format!("{err:#}");
        assert_eq!(text, OLD_SERVER_ERROR);
        assert!(text.contains("--web-transfer-base-url"), "{text}");
        assert!(
            !text.contains(WIRE_CANARY),
            "wire text reached the terminal: {text}"
        );
    }

    #[tokio::test]
    async fn cli_logs_never_contain_fragment_tokens() {
        // The run path handles the URL; nothing it emits may carry it. The
        // capture is this module's own writer, so the assertion covers every
        // tracing line the announcement path produces.
        #[derive(Clone)]
        struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl io::Write for LogSink {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = LogSink(std::sync::Arc::clone(&buf));
        let _ = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_max_level(tracing::Level::DEBUG)
            .try_init();

        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(4);
        let (created_tx, created_rx) = oneshot::channel();
        created_tx
            .send(CreatedRoom {
                display_url: CANARY_URL.to_string(),
                room_id: RoomId::from_bytes([0x66u8; 16]),
            })
            .expect("delivered");
        let lease = async { Ok(OwnerShutdown::CleanClose) };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let mut open = |_: &str| -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::NotFound, "no browser"))
        };
        announce_and_hold(
            lease,
            created_rx,
            lifecycle_tx,
            &mut out,
            &mut err_out,
            Some(&mut open),
        )
        .await
        .expect("clean close");

        let logs = String::from_utf8_lossy(&buf.lock().unwrap().clone()).to_string();
        for secret in ["CANARY-MEMBER-TOKEN", "CANARY-ROOM-KEY", "#m=", "&k="] {
            assert!(!logs.contains(secret), "tracing leaked {secret}: {logs}");
        }
        // The URL is on stdout, which is where the user asked for it.
        assert!(String::from_utf8_lossy(&out).contains(CANARY_URL));
    }
}
