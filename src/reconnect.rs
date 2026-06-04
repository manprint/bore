//! Automatic reconnection with capped exponential backoff.
//!
//! Client roles (`bore local`, `bore proxy`) can run a connect/serve cycle once,
//! or — with `--auto-reconnect` — forever: when a connection fails to establish
//! or drops, it is retried after a backoff (default sequence: 1, 2, 4, 8, 16, 32
//! seconds, then every 32 seconds indefinitely). The sequence, cap, and initial
//! delay are configurable via [`Backoff::new_with`] for use cases like the UDP
//! direct-path upgrade retry. A successful connection resets the backoff.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio::time::sleep;
use tracing::{info, warn};

/// Default initial backoff delay, in seconds.
const DEFAULT_INITIAL_BACKOFF_SECS: u64 = 1;

/// Default maximum backoff delay, in seconds.
const DEFAULT_MAX_BACKOFF_SECS: u64 = 32;

/// Capped exponential backoff: yields `initial, initial*2, initial*4, ...`
/// up to `max_secs`, then stays at `max_secs` indefinitely.
/// A successful connection resets back to the initial delay.
#[derive(Debug)]
pub struct Backoff {
    next_secs: u64,
    max_secs: u64,
    initial_secs: u64,
}

impl Backoff {
    /// Create a backoff with the default sequence (1 → 32 s).
    pub fn new() -> Self {
        Self::new_with(DEFAULT_INITIAL_BACKOFF_SECS, DEFAULT_MAX_BACKOFF_SECS)
    }

    /// Create a backoff that starts at `initial_secs` and caps at `max_secs`.
    pub fn new_with(initial_secs: u64, max_secs: u64) -> Self {
        let next_secs = initial_secs.min(max_secs);
        Self {
            next_secs,
            max_secs,
            initial_secs,
        }
    }

    /// Return the next delay without advancing the sequence.
    pub fn peek(&self) -> Duration {
        Duration::from_secs(self.next_secs)
    }

    /// Return the next delay and advance (doubling up to the cap).
    pub fn next_delay(&mut self) -> Duration {
        let delay = Duration::from_secs(self.next_secs);
        self.next_secs = self.next_secs.saturating_mul(2).min(self.max_secs);
        delay
    }

    /// Reset to the initial delay (after a successful connection).
    pub fn reset(&mut self) {
        self.next_secs = self.initial_secs;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a connect/serve cycle.
///
/// `connect` establishes a connection (e.g. dial + handshake); `serve` runs it
/// until it ends. Without `auto_reconnect` this runs exactly once and propagates
/// errors, preserving the original behaviour. With `auto_reconnect` it loops
/// forever, reconnecting with [`Backoff`] and never returning.
pub async fn run<Connect, ConnectFut, Handle, Serve, ServeFut>(
    auto_reconnect: bool,
    mut connect: Connect,
    mut serve: Serve,
) -> Result<()>
where
    Connect: FnMut() -> ConnectFut,
    ConnectFut: Future<Output = Result<Handle>>,
    Serve: FnMut(Handle) -> ServeFut,
    ServeFut: Future<Output = Result<()>>,
{
    if !auto_reconnect {
        let handle = connect().await?;
        return serve(handle).await;
    }

    let mut backoff = Backoff::new();
    loop {
        match connect().await {
            Ok(handle) => {
                info!("connected");
                // A successful connection clears the backoff so a later drop
                // reconnects promptly.
                backoff.reset();
                match serve(handle).await {
                    Ok(()) => info!("connection closed; reconnecting"),
                    Err(err) => warn!(%err, "connection closed with error; reconnecting"),
                }
            }
            Err(err) => warn!(%err, "failed to connect; retrying"),
        }
        let delay = backoff.next_delay();
        info!(seconds = delay.as_secs(), "reconnecting after backoff");
        sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_follows_capped_doubling_sequence() {
        let mut backoff = Backoff::new();
        let seconds: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(seconds, vec![1, 2, 4, 8, 16, 32, 32, 32]);
    }

    #[test]
    fn backoff_reset_returns_to_initial() {
        let mut backoff = Backoff::new();
        for _ in 0..5 {
            backoff.next_delay();
        }
        assert_eq!(backoff.next_delay().as_secs(), 32);
        backoff.reset();
        assert_eq!(backoff.next_delay().as_secs(), 1);
    }

    #[test]
    fn backoff_new_with_custom_params() {
        let mut backoff = Backoff::new_with(2, 256);
        // peek without advancing
        assert_eq!(backoff.peek().as_secs(), 2);
        // sequence: 2, 4, 8, 16, 32, 64, 128, 256, 256…
        let seconds: Vec<u64> = (0..9).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(seconds, vec![2, 4, 8, 16, 32, 64, 128, 256, 256]);
        // peek after advance shows current (not next)
        assert_eq!(backoff.peek().as_secs(), 256);
        // reset goes back to custom initial
        backoff.reset();
        assert_eq!(backoff.peek().as_secs(), 2);
        assert_eq!(backoff.next_delay().as_secs(), 2);
    }

    #[test]
    fn backoff_initial_clamped_to_max() {
        let mut backoff = Backoff::new_with(500, 100);
        assert_eq!(backoff.peek().as_secs(), 100);
        assert_eq!(backoff.next_delay().as_secs(), 100);
    }

    #[test]
    fn backoff_peek_does_not_advance() {
        let mut backoff = Backoff::new_with(5, 100);
        assert_eq!(backoff.peek().as_secs(), 5);
        assert_eq!(backoff.peek().as_secs(), 5); // still 5
        backoff.next_delay();
        assert_eq!(backoff.peek().as_secs(), 10); // advanced to 10
    }
}
