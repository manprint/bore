//! Connection liveness policy: how fast each side notices that the other is
//! gone, and how it stays out of the way of a link that is merely slow.
//!
//! **Field report (2026-10-02).** After a ~20 s network outage — most likely an
//! ISP IP change — a `bore vhost --udp --auto-reconnect --carriers 4` client
//! stayed down for ~16 minutes. Nothing in bore noticed that its control
//! connection was dead. The kernel did, after `tcp_retries2` (≈924 s): the
//! client's own heartbeats kept unacknowledged data in flight, so
//! `SO_KEEPALIVE` (which only probes an IDLE connection) never fired. The
//! server, meanwhile, held the subdomain for the old connection until its 60 s
//! reaper, so a quick reconnect would have been refused anyway.
//!
//! Three decisions answer it (plan `005_plan-OutageRecovery`, D1–D3):
//!
//! - **D1, liveness is measured on the transport, not on messages.**
//!   [`crate::mux::ConnActivity`] stamps every byte the socket delivers,
//!   whatever substream it belongs to. A heartbeat queues behind bulk data in
//!   the peer's socket buffer (≈4 MiB at 1 Mbit/s is ≈32 s), so a deadline on
//!   heartbeats would kill a healthy congested tunnel; a deadline on BYTES
//!   cannot, because a path that is moving data is delivering bytes.
//! - **D2, the client gives the server [`CLIENT_SILENCE_DEADLINE`].** Every
//!   current server heartbeats every 500 ms, so 15 s of total silence is 30
//!   missed beats: the path is dead. Flicks shorter than ≈12 s are recovered
//!   by TCP's own retransmission and never reach the deadline.
//! - **D3, the server reaps a client that DECLARED a heartbeat** after
//!   [`transport_reap_deadline`] (≥ [`TRANSPORT_REAP_FLOOR`]), so the name the
//!   dead connection held is free by the time the client reconnects. A client
//!   that declares nothing is never transport-reaped (the DEC-VE2 compat shape:
//!   reaping a peer that never promised to talk would kill healthy legacy
//!   tunnels).

use std::time::Duration;

/// How often a client sends `ClientMessage::Heartbeat` up its control
/// substream. 20 s before plan 005; 5 s now, so that a server deadline of three
/// beats is the 15 s floor rather than a minute.
pub const CTRL_CLIENT_HEARTBEAT: Duration = Duration::from_secs(5);

/// How long a client tolerates total inbound silence from the server before it
/// declares the connection dead, tears it down and reconnects (D2).
pub const CLIENT_SILENCE_DEADLINE: Duration = Duration::from_secs(15);

/// The shortest transport deadline the server applies to a declared client.
pub const TRANSPORT_REAP_FLOOR: Duration = Duration::from_secs(15);

/// How many declared heartbeat intervals of silence the server tolerates.
pub const TRANSPORT_REAP_MULTIPLIER: u32 = 3;

/// No deadline is ever longer than this, whatever a peer declares.
const DEADLINE_CAP: Duration = Duration::from_secs(3600);

/// The shortest silence deadline the override accepts.
const SILENCE_MIN: Duration = Duration::from_millis(100);

/// [`CTRL_CLIENT_HEARTBEAT`] with its override `BORE_CTRL_HEARTBEAT_MS`, read
/// per call so a harness can set it after startup. Mirrors
/// `ssh_open_timeout`'s `BORE_SSH_OPEN_TIMEOUT_MS`.
///
/// Exists because the interesting property — *the client actually sends what it
/// declared on the wire* — is otherwise only provable by a test that idles past
/// a production beat and a longer server deadline. A client that declares a
/// heartbeat and then fails to beat converts every healthy tunnel into a reaped
/// one, so that gate has to be cheap enough to keep.
pub fn ctrl_client_heartbeat() -> Duration {
    match std::env::var("BORE_CTRL_HEARTBEAT_MS") {
        Ok(ms) => match ms.parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => CTRL_CLIENT_HEARTBEAT,
        },
        Err(_) => CTRL_CLIENT_HEARTBEAT,
    }
}

/// The heartbeat interval a client puts on the wire (`ctrl_heartbeat_ms`).
/// Never 0 — 0 means "I do not beat" and disables the server's reaper.
pub fn ctrl_heartbeat_declared_ms() -> u32 {
    declared_ms_for(ctrl_client_heartbeat())
}

/// `interval` as a wire `ctrl_heartbeat_ms`: saturating, and at least 1.
pub fn declared_ms_for(interval: Duration) -> u32 {
    u32::try_from(interval.as_millis())
        .unwrap_or(u32::MAX)
        .max(1)
}

/// The client's silence deadline (D2): [`CLIENT_SILENCE_DEADLINE`], overridden
/// by `BORE_CTRL_SERVER_SILENCE_MS` (read per call; `0` disables it).
pub fn client_silence_deadline() -> Option<Duration> {
    parse_silence_ms(std::env::var("BORE_CTRL_SERVER_SILENCE_MS").ok().as_deref())
}

/// Pure half of [`client_silence_deadline`]. Absent or unparsable keeps the
/// shipped default — a malformed knob must never silently disable detection;
/// only an explicit `0` does. Otherwise clamped to `[100 ms, 1 h]`.
pub fn parse_silence_ms(raw: Option<&str>) -> Option<Duration> {
    match raw.map(str::trim).map(str::parse::<u64>) {
        Some(Ok(0)) => None,
        Some(Ok(ms)) => Some(Duration::from_millis(ms).clamp(SILENCE_MIN, DEADLINE_CAP)),
        _ => Some(CLIENT_SILENCE_DEADLINE),
    }
}

/// How often a loop checks a deadline: a quarter of it, within `[50 ms, 1 s]`,
/// so a trip lands at most one tick after the deadline without busy-polling.
pub fn liveness_tick(deadline: Duration) -> Duration {
    (deadline / 4).clamp(Duration::from_millis(50), Duration::from_secs(1))
}

/// The server's transport deadline for a client that declared `declared_ms`
/// (D3): `None` for 0 (never reaped), else `max(3 × declared, floor)`, never
/// above an hour.
pub fn transport_reap_deadline(declared_ms: u32, floor: Duration) -> Option<Duration> {
    if declared_ms == 0 {
        return None;
    }
    let beats =
        Duration::from_millis(u64::from(declared_ms)).saturating_mul(TRANSPORT_REAP_MULTIPLIER);
    Some(beats.max(floor).min(DEADLINE_CAP))
}

/// A ticker for an optional deadline: [`liveness_tick`] when armed, a future
/// that never completes when not, so a `select!` arm can be disabled without a
/// second copy of the loop. Missed ticks are delayed, never burst.
pub struct LivenessTicker(Option<tokio::time::Interval>);

impl LivenessTicker {
    /// A ticker for `deadline` (`None` never ticks).
    pub fn new(deadline: Option<Duration>) -> Self {
        Self(deadline.map(|d| {
            let period = liveness_tick(d);
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval
        }))
    }

    /// Wait for the next tick (forever when disarmed). Cancel-safe.
    pub async fn tick(&mut self) {
        match self.0.as_mut() {
            Some(interval) => {
                interval.tick().await;
            }
            None => std::future::pending::<()>().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn parse_silence_table() {
        let cases: &[(Option<&str>, Option<Duration>)] = &[
            (None, Some(CLIENT_SILENCE_DEADLINE)),
            (Some("garbage"), Some(CLIENT_SILENCE_DEADLINE)),
            (Some("-5"), Some(CLIENT_SILENCE_DEADLINE)),
            (Some(""), Some(CLIENT_SILENCE_DEADLINE)),
            (Some("0"), None),
            (Some("50"), Some(MS(100))),
            (Some("15000"), Some(MS(15_000))),
            (Some(" 2500 "), Some(MS(2_500))),
            (Some("99999999999"), Some(DEADLINE_CAP)),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_silence_ms(*raw), *want, "raw={raw:?}");
        }
    }

    #[test]
    fn transport_reap_deadline_table() {
        let floor = Duration::from_secs(15);
        assert_eq!(transport_reap_deadline(0, floor), None);
        assert_eq!(transport_reap_deadline(5_000, floor), Some(floor));
        assert_eq!(transport_reap_deadline(10_000, floor), Some(MS(30_000)));
        assert_eq!(transport_reap_deadline(u32::MAX, floor), Some(DEADLINE_CAP));
        assert_eq!(transport_reap_deadline(200, MS(1_000)), Some(MS(1_000)));
        assert_eq!(transport_reap_deadline(1, Duration::ZERO), Some(MS(3)));
    }

    #[test]
    fn liveness_tick_table() {
        assert_eq!(liveness_tick(MS(100)), MS(50));
        assert_eq!(liveness_tick(MS(400)), MS(100));
        assert_eq!(liveness_tick(MS(2_000)), MS(500));
        assert_eq!(
            liveness_tick(Duration::from_secs(15)),
            Duration::from_secs(1)
        );
        assert_eq!(
            liveness_tick(Duration::from_secs(3600)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn declared_ms_is_never_zero_and_saturates() {
        assert!(ctrl_heartbeat_declared_ms() >= 1);
        assert_eq!(declared_ms_for(Duration::ZERO), 1);
        assert_eq!(declared_ms_for(MS(5_000)), 5_000);
        assert_eq!(declared_ms_for(Duration::from_secs(u64::MAX)), u32::MAX);
    }

    /// D3's arithmetic, pinned: the shipped beat, declared, reaps at the floor,
    /// and the client's own deadline equals it — neither side gives up on a
    /// path the other still considers alive for long.
    #[test]
    fn shipped_defaults_are_consistent() {
        assert_eq!(CTRL_CLIENT_HEARTBEAT, Duration::from_secs(5));
        assert_eq!(
            transport_reap_deadline(declared_ms_for(CTRL_CLIENT_HEARTBEAT), TRANSPORT_REAP_FLOOR),
            Some(TRANSPORT_REAP_FLOOR)
        );
        assert_eq!(CLIENT_SILENCE_DEADLINE, TRANSPORT_REAP_FLOOR);
    }

    #[tokio::test(start_paused = true)]
    async fn ticker_disarmed_never_ticks_and_armed_ticks() {
        let mut off = LivenessTicker::new(None);
        assert!(tokio::time::timeout(Duration::from_secs(3600), off.tick())
            .await
            .is_err());
        let mut on = LivenessTicker::new(Some(MS(400)));
        let start = tokio::time::Instant::now();
        on.tick().await;
        assert_eq!(start.elapsed(), MS(100));
    }
}
