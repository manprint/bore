//! Automatic reconnection with capped exponential backoff.
//!
//! Client roles (`bore local`, `bore proxy`) can run a connect/serve cycle once,
//! or — with `--auto-reconnect` — forever: when a connection fails to establish
//! or drops, it is retried after a backoff of 1, 2, 4, 8, 16, 32 seconds, then
//! every 32 seconds indefinitely. A successful connection resets the backoff.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio::time::sleep;
use tracing::{info, warn};

/// Initial backoff delay, in seconds.
const INITIAL_BACKOFF_SECS: u64 = 1;

/// Maximum backoff delay, in seconds.
const MAX_BACKOFF_SECS: u64 = 32;

/// Capped exponential backoff yielding 1, 2, 4, 8, 16, 32, then 32 indefinitely.
#[derive(Debug)]
pub struct Backoff {
    next_secs: u64,
}

impl Backoff {
    /// Create a backoff positioned at the initial delay.
    pub fn new() -> Self {
        Self {
            next_secs: INITIAL_BACKOFF_SECS,
        }
    }

    /// Return the next delay and advance the sequence (doubling up to the cap).
    pub fn next_delay(&mut self) -> Duration {
        let delay = Duration::from_secs(self.next_secs);
        self.next_secs = self.next_secs.saturating_mul(2).min(MAX_BACKOFF_SECS);
        delay
    }

    /// Reset to the initial delay (after a successful connection).
    pub fn reset(&mut self) {
        self.next_secs = INITIAL_BACKOFF_SECS;
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
}
