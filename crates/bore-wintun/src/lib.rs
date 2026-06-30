//! Safe bore-facing wrapper around WinTun bindings.
//!
//! This crate owns the unsafe DLL-loading boundary required by WinTun. The main
//! bore crate keeps `#![forbid(unsafe_code)]` and calls only safe functions here.

#![deny(missing_docs)]

use std::path::{Path, PathBuf};

use anyhow::Result;

#[cfg(not(windows))]
use anyhow::bail;
#[cfg(windows)]
use anyhow::Context;
#[cfg(windows)]
use std::net::Ipv4Addr;
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use wintun_bindings::{Adapter, Session, Wintun};

/// Default WinTun session ring capacity (4 MiB).
pub const DEFAULT_RING_CAPACITY: u32 = 4 * 1024 * 1024;

/// Runtime WinTun library handle.
#[derive(Clone)]
pub struct WintunRuntime {
    #[cfg(windows)]
    inner: Wintun,
    #[cfg(not(windows))]
    _private: (),
}

impl WintunRuntime {
    /// Load `wintun.dll` from the default DLL search path.
    pub fn load_default() -> Result<Self> {
        #[cfg(windows)]
        {
            // SAFETY: this wrapper intentionally narrows WinTun DLL loading to a
            // small crate. Callers choose between default OS DLL resolution and an
            // explicit path; both are documented operational inputs. The loaded
            // library handle is kept alive for every adapter/session using it.
            let inner = unsafe { wintun_bindings::load() }.context("failed to load wintun.dll")?;
            Ok(Self { inner })
        }
        #[cfg(not(windows))]
        {
            bail!("WinTun is only available on Windows")
        }
    }

    /// Load `wintun.dll` from an explicit path.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        #[cfg(windows)]
        {
            // SAFETY: same boundary as `load_default`, but using an operator-
            // provided path. The caller must pass a trusted path; bore documents
            // this as either the executable directory or `BORE_WINTUN_DLL`.
            let inner = unsafe { wintun_bindings::load_from_path(path.as_os_str()) }
                .with_context(|| format!("failed to load wintun.dll from {}", path.display()))?;
            Ok(Self { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            bail!("WinTun is only available on Windows")
        }
    }

    /// Open an existing adapter by name.
    #[cfg(windows)]
    pub fn open_adapter(&self, name: &str) -> Result<Arc<Adapter>> {
        Adapter::open(&self.inner, name)
            .with_context(|| format!("failed to open WinTun adapter {name}"))
    }

    /// Create a new adapter by name.
    #[cfg(windows)]
    pub fn create_adapter(&self, name: &str, tunnel_type: &str) -> Result<Arc<Adapter>> {
        Adapter::create(&self.inner, name, tunnel_type, None)
            .with_context(|| format!("failed to create WinTun adapter {name}"))
    }
}

/// Bore-owned WinTun adapter/session pair.
#[cfg(windows)]
#[derive(Clone)]
pub struct WintunDevice {
    adapter: Arc<Adapter>,
    session: Arc<Session>,
    name: String,
    index: u32,
}

#[cfg(windows)]
impl WintunDevice {
    /// Open or create a WinTun adapter, configure IPv4/MTU, and start a session.
    pub fn open_or_create(
        dll_path: Option<&Path>,
        name: &str,
        tunnel_type: &str,
        address: Ipv4Addr,
        netmask: Ipv4Addr,
        mtu: usize,
        ring_capacity: u32,
    ) -> Result<Self> {
        let runtime = match dll_path {
            Some(path) => WintunRuntime::load_from_path(path)?,
            None => WintunRuntime::load_default()?,
        };
        let adapter = match runtime.open_adapter(name) {
            Ok(adapter) => adapter,
            Err(_) => runtime.create_adapter(name, tunnel_type)?,
        };
        adapter.bore_set_ipv4(address, netmask)?;
        adapter.bore_set_mtu(mtu)?;
        let resolved_name = adapter.bore_name()?;
        let index = adapter.bore_index()?;
        let session = adapter.start_bore_session(ring_capacity)?;
        Ok(Self {
            adapter,
            session,
            name: resolved_name,
            index,
        })
    }

    /// Resolved adapter name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Adapter interface index.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Receive one packet into `buf`.
    pub fn recv_blocking(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.session.bore_recv(buf)
    }

    /// Send one packet from `buf`.
    pub fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.session.bore_send(buf)
    }

    /// Shutdown blocking receives.
    pub fn shutdown(&self) -> Result<()> {
        self.session.bore_shutdown()
    }

    /// Keep adapter handle alive for the device lifetime.
    pub fn adapter(&self) -> &Arc<Adapter> {
        &self.adapter
    }
}

/// Safe adapter operations bore needs.
#[cfg(windows)]
pub trait AdapterExt {
    /// Start a WinTun session.
    fn start_bore_session(&self, capacity: u32) -> Result<Arc<Session>>;
    /// Return adapter name.
    fn bore_name(&self) -> Result<String>;
    /// Return adapter interface index.
    fn bore_index(&self) -> Result<u32>;
    /// Set adapter MTU.
    fn bore_set_mtu(&self, mtu: usize) -> Result<()>;
    /// Set adapter IPv4 address/prefix pieces.
    fn bore_set_ipv4(&self, address: Ipv4Addr, netmask: Ipv4Addr) -> Result<()>;
}

#[cfg(windows)]
impl AdapterExt for Arc<Adapter> {
    fn start_bore_session(&self, capacity: u32) -> Result<Arc<Session>> {
        self.start_session(capacity)
            .context("failed to start WinTun session")
    }

    fn bore_name(&self) -> Result<String> {
        self.get_name()
            .context("failed to read WinTun adapter name")
    }

    fn bore_index(&self) -> Result<u32> {
        self.get_adapter_index()
            .context("failed to read WinTun adapter index")
    }

    fn bore_set_mtu(&self, mtu: usize) -> Result<()> {
        self.set_mtu(mtu)
            .context("failed to set WinTun adapter MTU")
    }

    fn bore_set_ipv4(&self, address: Ipv4Addr, netmask: Ipv4Addr) -> Result<()> {
        self.set_address(address)
            .context("failed to set WinTun adapter IPv4 address")?;
        self.set_netmask(netmask)
            .context("failed to set WinTun adapter IPv4 netmask")
    }
}

/// Safe session operations bore needs.
#[cfg(windows)]
pub trait SessionExt {
    /// Receive one packet into `buf`.
    fn bore_recv(&self, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Send one packet from `buf`.
    fn bore_send(&self, buf: &[u8]) -> std::io::Result<usize>;
    /// Shutdown blocking receives.
    fn bore_shutdown(&self) -> Result<()>;
}

#[cfg(windows)]
impl SessionExt for Arc<Session> {
    fn bore_recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.recv(buf)
    }

    fn bore_send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.send(buf)
    }

    fn bore_shutdown(&self) -> Result<()> {
        self.shutdown().context("failed to shutdown WinTun session")
    }
}

/// Return the operator-provided `wintun.dll` path, if any.
pub fn dll_path_from_env_var(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_load_functions_are_safe_to_name() {
        fn assert_safe<F>(_f: F) {}
        assert_safe(WintunRuntime::load_default);
        assert_safe(|path: &Path| WintunRuntime::load_from_path(path));
    }

    #[test]
    fn empty_dll_env_is_ignored() {
        assert!(dll_path_from_env_var(Some(std::ffi::OsString::new())).is_none());
    }

    #[test]
    fn dll_env_path_is_returned() {
        assert_eq!(
            dll_path_from_env_var(Some(std::ffi::OsString::from(r"C:\bore\wintun.dll"))).unwrap(),
            PathBuf::from(r"C:\bore\wintun.dll")
        );
    }
}
