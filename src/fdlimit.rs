//! Reconcile `--max-conns` with the process file-descriptor limit.
//!
//! `--max-conns` exists to bound concurrency *gracefully*: at capacity the
//! accept loop refuses one connection, counts it in `conn_rejections` and
//! keeps serving everything else. That contract only holds while the process
//! can still open a descriptor for each admitted connection. Above the
//! descriptor limit the kernel refuses first, with `EMFILE`, and `EMFILE` is
//! not graceful: it lands on `accept()` for EVERY listener the server owns, so
//! a single tunnel's concurrency takes down the admin API, the vhost
//! frontends and every other tunnel with it.
//!
//! MEASURED on the staging deployment (2026-09-11, campaign §11): the
//! container ran with `--max-conns 1024` and a soft `RLIMIT_NOFILE` of 1024.
//! At roughly 976 concurrently held connections through ONE public tunnel the
//! server logged `failed to accept tunnel connection err=No file descriptors
//! available (os error 24)` every 100 ms and answered nothing on its control
//! port for about half a minute — while `conn_rejections` stayed **0**,
//! because the semaphore bound could never be reached. The configured bound
//! was unreachable by construction.
//!
//! A process may raise its own soft limit up to its hard limit without any
//! privilege, so the fix is to do exactly that at startup, and to say so.
//! When even the hard limit cannot cover the bound, the operator is told the
//! two remedies (raise the container/service limit, or lower `--max-conns`)
//! instead of discovering `EMFILE` under load.

/// One descriptor per admitted proxied connection, plus room for the
/// listeners, the tunnel control connections, TLS material, the QUIC sockets,
/// the log files and the admin API.
///
/// The headroom is deliberately generous: the cost of over-reserving
/// descriptors is zero (they are not allocated until used), and the cost of
/// under-reserving is the `EMFILE` storm described above.
pub const FD_HEADROOM: u64 = 256;

/// Descriptors this server wants to be able to open for a given connection
/// bound.
pub fn fds_needed(max_conns: u64) -> u64 {
    max_conns.saturating_add(FD_HEADROOM)
}

/// What the process should do about its descriptor limit, given the bound it
/// was configured with and the limits it currently has.
///
/// Pure on purpose: the decision is unit-tested across the interesting
/// boundaries without touching the process' real limits, which a test cannot
/// change back for the rest of the test binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdBudget {
    /// The soft limit already covers the bound. Nothing to do.
    Sufficient {
        /// The soft limit as it stands.
        soft: u64,
        /// Descriptors the configured bound wants.
        needed: u64,
    },
    /// The soft limit is short but the hard limit allows raising it.
    Raise {
        /// The soft limit as it stands.
        from: u64,
        /// The soft limit to set.
        to: u64,
        /// Descriptors the configured bound wants.
        needed: u64,
    },
    /// Not even the hard limit covers the bound; raise as far as possible and
    /// tell the operator.
    Insufficient {
        /// The soft limit as it stands.
        soft: u64,
        /// The ceiling this process cannot raise itself past.
        hard: u64,
        /// Descriptors the configured bound wants.
        needed: u64,
    },
}

/// Decide what to do with the descriptor limits for `max_conns`.
///
/// `hard` is `None` for an unlimited (`RLIM_INFINITY`) hard limit, which can
/// always satisfy the request.
pub fn fd_budget(max_conns: u64, soft: u64, hard: Option<u64>) -> FdBudget {
    let needed = fds_needed(max_conns);
    if soft >= needed {
        return FdBudget::Sufficient { soft, needed };
    }
    match hard {
        // Unlimited hard limit: always raisable to exactly what is needed.
        None => FdBudget::Raise {
            from: soft,
            to: needed,
            needed,
        },
        Some(hard) if hard >= needed => FdBudget::Raise {
            from: soft,
            to: needed,
            needed,
        },
        // The hard limit is the ceiling. Raising to it is still strictly
        // better than leaving the soft limit where it is — it buys the
        // difference — so the caller does that AND warns.
        Some(hard) => FdBudget::Insufficient { soft, hard, needed },
    }
}

/// Apply [`fd_budget`] to this process, logging what happened.
///
/// Called once, at server startup, before the first listener is bound. Never
/// fails the startup: a server that cannot raise its own limit must still
/// start, and the point of the advisory is that the operator hears about it
/// now rather than during an `EMFILE` storm.
#[cfg(unix)]
pub fn reconcile_fd_limit(max_conns: usize) {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    use tracing::{debug, info, warn};

    let (soft, hard_raw) = match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok(pair) => pair,
        Err(err) => {
            debug!(%err, "cannot read RLIMIT_NOFILE; leaving the descriptor limit alone");
            return;
        }
    };
    // `RLIM_INFINITY` is the sentinel for "no ceiling", which can satisfy any
    // request; anything else is a real ceiling this process cannot pass.
    let hard_opt = (hard_raw != nix::libc::RLIM_INFINITY).then_some(hard_raw);

    match fd_budget(max_conns as u64, soft, hard_opt) {
        FdBudget::Sufficient { soft, needed } => {
            debug!(
                soft,
                needed, max_conns, "file-descriptor limit already covers --max-conns"
            );
        }
        FdBudget::Raise { from, to, needed } => {
            match setrlimit(Resource::RLIMIT_NOFILE, to, hard_raw) {
                Ok(()) => info!(
                    from,
                    to, needed, max_conns, "raised the file-descriptor limit to cover --max-conns"
                ),
                Err(err) => warn!(
                    %err, from, wanted = to, needed, max_conns,
                    "could not raise the file-descriptor limit: at {max_conns} concurrent \
                     connections the kernel will refuse with EMFILE before --max-conns \
                     refuses gracefully, and EMFILE affects every listener including the \
                     admin API. Raise the limit for this process (container: \
                     `ulimits: nofile:`; systemd: `LimitNOFILE=`) or lower --max-conns to \
                     about {}",
                    from.saturating_sub(FD_HEADROOM)
                ),
            }
        }
        FdBudget::Insufficient { soft, hard, needed } => {
            // Still raise to the ceiling: it is the best this process can do
            // for itself, and it is strictly better than the current soft
            // limit.
            let _ = setrlimit(Resource::RLIMIT_NOFILE, hard, hard_raw);
            warn!(
                soft,
                hard,
                needed,
                max_conns,
                "--max-conns {max_conns} needs {needed} file descriptors but the hard \
                 limit is {hard}: raised the soft limit to the hard limit, which is \
                 still short. Above about {} concurrent connections the kernel will \
                 refuse with EMFILE before --max-conns refuses gracefully, and EMFILE \
                 affects every listener including the admin API. Raise the hard limit \
                 for this process (container: `ulimits: nofile:`; systemd: \
                 `LimitNOFILE=`) or lower --max-conns",
                hard.saturating_sub(FD_HEADROOM)
            );
        }
    }
}

/// Non-unix hosts have no `RLIMIT_NOFILE`; Windows bounds descriptors per
/// process by a different mechanism that a process cannot raise at runtime.
#[cfg(not(unix))]
pub fn reconcile_fd_limit(_max_conns: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_soft_limit_above_the_need_is_left_alone() {
        assert_eq!(
            fd_budget(1024, 65536, Some(524288)),
            FdBudget::Sufficient {
                soft: 65536,
                needed: 1280
            }
        );
    }

    #[test]
    fn the_staging_deployment_shape_asks_for_a_raise() {
        // The measured case: --max-conns 1024 with soft == hard-capable 1024.
        assert_eq!(
            fd_budget(1024, 1024, Some(524288)),
            FdBudget::Raise {
                from: 1024,
                to: 1280,
                needed: 1280
            }
        );
    }

    #[test]
    fn exactly_enough_is_enough() {
        // The boundary must not ask for a raise it does not need.
        assert_eq!(
            fd_budget(1024, 1280, Some(1280)),
            FdBudget::Sufficient {
                soft: 1280,
                needed: 1280
            }
        );
        assert!(matches!(
            fd_budget(1024, 1279, Some(1280)),
            FdBudget::Raise { to: 1280, .. }
        ));
    }

    #[test]
    fn an_unlimited_hard_limit_can_always_be_raised_to() {
        assert_eq!(
            fd_budget(4096, 1024, None),
            FdBudget::Raise {
                from: 1024,
                to: 4352,
                needed: 4352
            }
        );
    }

    #[test]
    fn a_hard_limit_below_the_need_is_reported_not_hidden() {
        assert_eq!(
            fd_budget(4096, 1024, Some(2048)),
            FdBudget::Insufficient {
                soft: 1024,
                hard: 2048,
                needed: 4352
            }
        );
    }

    #[test]
    fn the_headroom_is_charged_once_and_cannot_overflow() {
        assert_eq!(fds_needed(0), FD_HEADROOM);
        assert_eq!(fds_needed(u64::MAX), u64::MAX);
        // A pathological bound must still be reported, never wrapped into
        // "sufficient".
        assert!(matches!(
            fd_budget(u64::MAX, 1024, Some(524288)),
            FdBudget::Insufficient { .. }
        ));
    }
}
