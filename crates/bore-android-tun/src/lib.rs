//! Safe bore-facing wrapper around raw Android TUN creation.
//!
//! `tun-rs`'s `DeviceBuilder` (ioctl-based create-from-scratch) is compiled only for
//! windows/linux(non-ohos)/macos/*bsd — not android. Upstream's android story is a fd
//! sourced from Android's Java-side `VpnService.Builder().establish()` via JNI, which
//! doesn't fit bore's rooted-CLI host-only model (no app shell). Since a rooted android
//! device is still plain Linux underneath (same kernel TUN driver/ABI), we open the
//! kernel TUN clone device ourselves and perform the `TUNSETIFF` ioctl directly, then
//! hand the fd to `tun_rs::AsyncDevice::from_fd`.
//!
//! Both of those steps are `unsafe` (raw ioctl FFI, fd-ownership transfer) and bore's
//! main crate keeps `#![forbid(unsafe_code)]`, so — mirroring `bore-wintun`'s isolation
//! of the WinTun DLL FFI — this crate owns that unsafe boundary and exposes a fully
//! safe function to callers.

#![deny(missing_docs)]

#[cfg(target_os = "android")]
use anyhow::Context;
use anyhow::Result;

/// Open the Android TUN clone device, bind it to `name` via `TUNSETIFF`, and wrap
/// the resulting fd in a `tun_rs::AsyncDevice`.
///
/// Returns the device plus the kernel-resolved interface name (usually `name`
/// verbatim, but the kernel is the source of truth). Address/MTU/up state are not
/// configured here — there is no `DeviceBuilder` to do it, so callers must apply the
/// same `ip addr add` / `ip link set mtu` / `ip link set up` sequence used for routes
/// elsewhere in bore's android host config.
#[cfg(target_os = "android")]
pub fn create(name: &str) -> Result<(tun_rs::AsyncDevice, String)> {
    let dev_path = if std::path::Path::new("/dev/tun").exists() {
        "/dev/tun"
    } else {
        // Android's minimal /dev has no "net" subdirectory on stock ROMs; some
        // kernels/ROMs still provide the desktop-Linux path.
        "/dev/net/tun"
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev_path)
        .with_context(|| format!("opening {dev_path} (TUN clone device)"))?;

    let mut ifr: nix::libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in ifr.ifr_name.iter_mut().zip(name.as_bytes()) {
        *dst = *src as std::os::raw::c_char;
    }
    ifr.ifr_ifru.ifru_flags = (nix::libc::IFF_TUN | nix::libc::IFF_NO_PI) as std::os::raw::c_short;

    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    // SAFETY: `fd` is a valid, open fd for `dev_path` (just opened above); `ifr` is a
    // valid, live `ifreq` for the duration of this call.
    let ret = unsafe { nix::libc::ioctl(fd, nix::libc::TUNSETIFF, std::ptr::addr_of_mut!(ifr)) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error())
            .context("TUNSETIFF ioctl failed on Android TUN clone device");
    }

    let actual_name = {
        let raw = &ifr.ifr_name;
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        raw[..end]
            .iter()
            .map(|&c| c as u8 as char)
            .collect::<String>()
    };

    // Hand the fd to tun-rs. `from_fd` takes ownership (closes on drop), so release it
    // from `file` first to avoid a double-close.
    let raw_fd = std::os::fd::IntoRawFd::into_raw_fd(file);
    // SAFETY: `raw_fd` was just configured as a TUN device above (owned, open, refers
    // to a TUN device) and is not used anywhere else after this call.
    let dev = unsafe { tun_rs::AsyncDevice::from_fd(raw_fd) }
        .with_context(|| format!("wrapping fd for {actual_name} via tun_rs::AsyncDevice"))?;

    Ok((dev, actual_name))
}

/// Stub for non-Android targets so the workspace (and its dev-machine gates) still
/// builds; real callers only reach this behind `#[cfg(target_os = "android")]`.
#[cfg(not(target_os = "android"))]
pub fn create(_name: &str) -> Result<(tun_rs::AsyncDevice, String)> {
    anyhow::bail!("bore-android-tun::create is only available on Android")
}
