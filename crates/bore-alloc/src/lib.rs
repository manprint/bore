//! Global allocator for the `bore` binary.
//!
//! On the static musl builds (x86_64 and aarch64: the Docker image and the
//! released Linux binaries) this is mimalloc; everywhere else it forwards to
//! the system allocator. musl's own allocator is the bottleneck of the yamux
//! relay, which allocates one buffer per frame on both sides.
//!
//! This crate owns the `unsafe` the allocator hook needs. The main bore crate
//! keeps `#![forbid(unsafe_code)]` and only names [`Allocator`] and
//! [`configure`].

#![deny(missing_docs)]

use std::alloc::{GlobalAlloc, Layout};

#[cfg(all(
    target_env = "musl",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod imp {
    pub(crate) use mimalloc::MiMalloc as Inner;

    extern "C" {
        fn bore_alloc_configure();
    }

    pub(crate) fn configure() {
        // SAFETY: the shim takes no arguments and only stores one integer in
        // mimalloc's option table. `configure` documents the single-threaded
        // requirement of that store.
        unsafe { bore_alloc_configure() }
    }

    pub(crate) const NAME: &str = "mimalloc";
}

#[cfg(not(all(
    target_env = "musl",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod imp {
    pub(crate) use std::alloc::System as Inner;

    pub(crate) fn configure() {}

    pub(crate) const NAME: &str = "system";
}

/// The allocator to install with `#[global_allocator]`.
pub struct Allocator;

// SAFETY: every method forwards unchanged to a `GlobalAlloc` implementation
// (mimalloc or the system allocator), so its contract is the inner one's.
unsafe impl GlobalAlloc for Allocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        imp::Inner.alloc(layout)
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        imp::Inner.dealloc(ptr, layout)
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        imp::Inner.alloc_zeroed(layout)
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        imp::Inner.realloc(ptr, layout, new_size)
    }
}

/// Applies bore's allocator settings. Call once, first thing in `main`, before
/// any other thread exists: mimalloc's option store is not thread safe.
///
/// With mimalloc this makes freed memory go back to the OS immediately
/// (purge delay 0); without it mimalloc keeps hundreds of MiB after a burst of
/// connections. `MIMALLOC_PURGE_DELAY` in the environment still wins. A no-op
/// on the system allocator.
pub fn configure() {
    imp::configure()
}

/// Name of the allocator this build uses: `"mimalloc"` or `"system"`.
pub const fn name() -> &'static str {
    imp::NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static GLOBAL: Allocator = Allocator;

    #[test]
    fn allocates_grows_and_zeroes_through_the_hook() {
        configure();
        let mut v: Vec<u8> = Vec::with_capacity(16);
        v.extend_from_slice(&[7; 16]);
        v.resize(1 << 20, 9);
        assert_eq!(v[15], 7);
        assert_eq!(v[(1 << 20) - 1], 9);
        let z = vec![0u8; 256 * 1024];
        assert!(z.iter().all(|b| *b == 0));
        assert!(matches!(name(), "mimalloc" | "system"));
    }
}
