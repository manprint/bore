//! Web-transfer domain model: protocol constants, IDs, limits and validators.
//!
//! Phase 0 foundation only. No route, server flag, CLI command or runtime
//! registry lives here; those arrive in later phases. Browser/native wire
//! codecs live in [`crate::web_transfer_protocol`].

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    net::IpAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use dashmap::DashMap;
use futures_util::stream::BoxStream;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Browser/native protocol version. Versioned envelopes reject anything else.
pub const WEB_TRANSFER_PROTOCOL_VERSION: u16 = 1;
/// Owner-lease heartbeat the CLI sends on an idle control connection.
pub const WEB_TRANSFER_CLIENT_HEARTBEAT: Duration = Duration::from_secs(20);
/// Control-connection liveness timeout (server reaper, tick-checked).
pub const WEB_TRANSFER_CTRL_TIMEOUT: Duration = Duration::from_secs(60);
/// Server reaper tick; liveness is checked here, never via `timeout(recv)`.
pub const WEB_TRANSFER_REAPER_TICK: Duration = Duration::from_millis(500);
/// Deadline for one direct-path (WebRTC) negotiation attempt.
pub const WEB_TRANSFER_DIRECT_DEADLINE: Duration = Duration::from_secs(10);
/// Time a recipient waits for the peer's relay leg to attach.
pub const WEB_TRANSFER_RELAY_ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on one control-channel heartbeat write (`beat_once` shape).
pub const WEB_TRANSFER_CTRL_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest accepted canonical manifest (bytes).
pub const WEB_TRANSFER_MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Largest accepted control-envelope message (bytes).
pub const WEB_TRANSFER_MAX_CONTROL_BYTES: usize = 320 * 1024;
/// Largest relay message on the opaque WebSocket path (bytes).
pub const WEB_TRANSFER_MAX_RELAY_MESSAGE_BYTES: usize = 32 * 1024;
/// Largest single relay frame on the opaque WebSocket path (bytes).
pub const WEB_TRANSFER_MAX_RELAY_FRAME_BYTES: usize = 32 * 1024;
/// Largest plaintext fragment carried inside one encrypted frame (bytes).
pub const WEB_TRANSFER_MAX_PLAINTEXT_FRAGMENT_BYTES: usize = 24 * 1024;
/// Logical content chunk hashed with SHA-256 (bytes).
pub const WEB_TRANSFER_CHUNK_BYTES: usize = 1024 * 1024;
/// Relay per-connection high-water mark: pause reading above this (bytes).
pub const WEB_TRANSFER_HIGH_WATER_BYTES: usize = 4 * 1024 * 1024;
/// Relay per-connection low-water mark: resume reading below this (bytes).
pub const WEB_TRANSFER_LOW_WATER_BYTES: usize = 1024 * 1024;
/// Largest accepted SDP blob (bytes).
pub const WEB_TRANSFER_MAX_SDP_BYTES: usize = 64 * 1024;
/// Largest accepted single ICE candidate line (bytes).
pub const WEB_TRANSFER_MAX_ICE_CANDIDATE_BYTES: usize = 4 * 1024;
/// Largest accepted ICE candidate count per side.
pub const WEB_TRANSFER_MAX_ICE_CANDIDATES_PER_SIDE: usize = 128;
/// Maximum `sdpMid` length on a forwarded ICE candidate.
pub const WEB_TRANSFER_MAX_ICE_SDP_MID_BYTES: usize = 64;
/// Largest accepted peer display name (Unicode scalar values).
pub const WEB_TRANSFER_MAX_DISPLAY_NAME_CHARS: usize = 48;
/// Largest accepted manifest entry path (bytes).
pub const WEB_TRANSFER_MAX_PATH_BYTES: usize = 4096;
/// Largest accepted single manifest path segment (bytes).
pub const WEB_TRANSFER_MAX_PATH_SEGMENT_BYTES: usize = 255;

/// Parses strict lowercase hex into `[u8; N]`: exact length, `0-9a-f` only.
/// Shared with the protocol codecs so both sides accept identical input.
pub(crate) fn parse_hex_id<const N: usize>(what: &str, s: &str) -> Result<[u8; N], String> {
    if s.len() != N * 2 {
        return Err(format!("{what}: need {} hex chars, got {}", N * 2, s.len()));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("{what}: need canonical lowercase hex"));
    }
    let raw = hex::decode(s).map_err(|e| format!("{what}: invalid hex: {e}"))?;
    raw.try_into()
        .map_err(|_| format!("{what}: contradiction in hex length"))
}

/// Declares one fixed-width lowercase-hex ID. `$redact` selects the `Debug`:
/// `secret` prints `Name(redacted)`, `public` prints the hex like `Display`.
macro_rules! define_web_id {
    ($name:ident, $len:literal, secret) => {
        /// Fixed-width identifier; wire form is canonical lowercase hex.
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name([u8; $len]);

        impl $name {
            /// Builds the ID from raw bytes (generation sites only).
            pub fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// Raw bytes; crate-internal so secrets never cross the API boundary.
            /// Phase 1 registry/URL sites are the first consumers.
            #[allow(dead_code)]
            pub(crate) fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }

        // Hex-string serde for the native control wire (`shared.rs` enums).
        // Deliberate: these types cross the authenticated yamux stream only.
        // Admin/log paths must never serialize rooms, tokens or keys.
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&hex::encode(self.0))
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = <String as serde::Deserialize>::deserialize(deserializer)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), "redacted")
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(parse_hex_id(stringify!($name), s)?))
            }
        }
    };
    ($name:ident, $len:literal, public) => {
        /// Fixed-width identifier; wire form is canonical lowercase hex.
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name([u8; $len]);

        impl $name {
            /// Builds the ID from raw bytes (generation sites only).
            pub fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// Raw bytes; crate-internal to keep construction sites auditable.
            /// Phase 1 registry/URL sites are the first consumers.
            #[allow(dead_code)]
            pub(crate) fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }

        // Hex-string serde for the native control wire (`shared.rs` enums).
        // Deliberate: these types cross the authenticated yamux stream only.
        // Admin/log paths must never serialize rooms, tokens or keys.
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&hex::encode(self.0))
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = <String as serde::Deserialize>::deserialize(deserializer)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), hex::encode(self.0))
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(parse_hex_id(stringify!($name), s)?))
            }
        }
    };
}

define_web_id!(RoomId, 16, public);
define_web_id!(PeerId, 16, public);
define_web_id!(OfferId, 16, public);
define_web_id!(TransferId, 16, public);
define_web_id!(AttemptId, 16, public);
define_web_id!(MemberToken, 32, secret);
define_web_id!(OwnerToken, 32, secret);
define_web_id!(RoomKey, 32, secret);
define_web_id!(RelayTicket, 16, secret);

/// SHA-256 over arbitrary bytes.
fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    Sha256::new().chain_update(data).finalize().into()
}

impl MemberToken {
    /// Hash stored server-side; the raw token never leaves the browser/CLI.
    pub fn sha256_hash(&self) -> [u8; 32] {
        sha256_bytes(self.0.as_slice())
    }
}

impl OwnerToken {
    /// Hash stored server-side; the raw token never leaves the CLI/browser.
    pub fn sha256_hash(&self) -> [u8; 32] {
        sha256_bytes(self.0.as_slice())
    }
}

impl RoomKey {
    /// Hash used only as a key-derivation domain separator input, never logged.
    pub fn sha256_hash(&self) -> [u8; 32] {
        sha256_bytes(self.0.as_slice())
    }
}

impl RelayTicket {
    /// Hash stored server-side; the raw ticket is shown once to its peer.
    pub fn sha256_hash(&self) -> [u8; 32] {
        sha256_bytes(self.0.as_slice())
    }
}

/// Constant-time equality for 32-byte digests (subtle, already linked).
/// Never compare token hashes with `==` or ordinary string equality.
pub fn token_digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(a.as_slice().ct_eq(b.as_slice()))
}

/// Global and per-room admission caps for the web-transfer service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebTransferLimits {
    /// Maximum live rooms on this server.
    pub max_rooms: u64,
    /// Maximum peers summed over all rooms.
    pub max_peers_global: u64,
    /// Maximum peers in one room.
    pub max_peers_per_room: u64,
    /// Maximum offers published by one peer.
    pub max_offers_per_peer: u64,
    /// Maximum manifest entries in one offer.
    pub max_entries_per_offer: u64,
    /// Maximum logical bytes in one offer (1 TiB default).
    pub max_offer_bytes: u64,
    /// Maximum control/catalog metadata held for one room (bytes).
    pub max_metadata_per_room_bytes: u64,
    /// Maximum control/catalog metadata held server-wide (bytes).
    pub max_metadata_total_bytes: u64,
    /// Maximum concurrent transfers one peer takes part in.
    pub max_transfers_per_peer: u64,
    /// Maximum live opaque relay pairs server-wide.
    pub max_relays_global: u64,
    /// Relay throttle per room (bytes/s); `0` disables throttling.
    pub relay_rate_bytes_per_s: u64,
    /// Grace after abnormal owner loss before the room dies (seconds).
    pub owner_grace_secs: u64,
}

impl Default for WebTransferLimits {
    fn default() -> Self {
        Self {
            max_rooms: 1024,
            max_peers_global: 4096,
            max_peers_per_room: 32,
            max_offers_per_peer: 64,
            max_entries_per_offer: 10000,
            max_offer_bytes: 1099511627776,
            max_metadata_per_room_bytes: 16777216,
            max_metadata_total_bytes: 268435456,
            max_transfers_per_peer: 8,
            max_relays_global: 256,
            relay_rate_bytes_per_s: 104857600,
            owner_grace_secs: 60,
        }
    }
}

impl WebTransferLimits {
    /// Smallest owner grace the reaper logic supports (seconds).
    pub const MIN_OWNER_GRACE_SECS: u64 = 5;
    /// Largest owner grace before a dead room pins state too long (seconds).
    pub const MAX_OWNER_GRACE_SECS: u64 = 600;

    /// Rejects zero caps (except the relay-rate sentinel), an out-of-range
    /// grace, a total smaller than one room's share, and values that do not
    /// fit the address space. Fails before any listener binds.
    pub fn validate(&self) -> Result<()> {
        let nonzero: &[(&str, u64)] = &[
            ("max_rooms", self.max_rooms),
            ("max_peers_global", self.max_peers_global),
            ("max_peers_per_room", self.max_peers_per_room),
            ("max_offers_per_peer", self.max_offers_per_peer),
            ("max_entries_per_offer", self.max_entries_per_offer),
            ("max_offer_bytes", self.max_offer_bytes),
            (
                "max_metadata_per_room_bytes",
                self.max_metadata_per_room_bytes,
            ),
            ("max_metadata_total_bytes", self.max_metadata_total_bytes),
            ("max_transfers_per_peer", self.max_transfers_per_peer),
            ("max_relays_global", self.max_relays_global),
        ];
        for (name, value) in nonzero {
            if *value == 0 {
                bail!("web-transfer limit {name} must be nonzero");
            }
        }
        if !(Self::MIN_OWNER_GRACE_SECS..=Self::MAX_OWNER_GRACE_SECS)
            .contains(&self.owner_grace_secs)
        {
            bail!(
                "web-transfer owner grace must be {}..={}s, got {}",
                Self::MIN_OWNER_GRACE_SECS,
                Self::MAX_OWNER_GRACE_SECS,
                self.owner_grace_secs
            );
        }
        if self.max_metadata_total_bytes < self.max_metadata_per_room_bytes {
            bail!(
                "web-transfer total metadata {} must cover one room {}",
                self.max_metadata_total_bytes,
                self.max_metadata_per_room_bytes
            );
        }
        // Checked conversions: every cap that sizes an allocation must fit.
        for (name, value) in nonzero
            .iter()
            .copied()
            .chain([("owner_grace_secs", self.owner_grace_secs)])
        {
            usize::try_from(value)
                .map_err(|_| anyhow::anyhow!("web-transfer limit {name} does not fit usize"))?;
        }
        usize::try_from(self.relay_rate_bytes_per_s)
            .map_err(|_| anyhow::anyhow!("web-transfer relay rate does not fit usize"))?;
        Ok(())
    }
}

/// Parsed `--web-transfer-base-url`: the same-origin root that enables the
/// browser surface. HTTPS only, except loopback HTTP for local development.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebTransferBaseUrl {
    origin: String,
    authority: String,
}

impl WebTransferBaseUrl {
    /// Parses and validates `input`. Rejects userinfo, query, fragment and any
    /// path other than empty/`/`; normalizes a trailing slash away.
    pub fn parse(input: &str) -> Result<Self> {
        let url = url::Url::parse(input)
            .map_err(|e| anyhow::anyhow!("invalid web-transfer base URL: {e}"))?;
        match url.scheme() {
            "https" => {}
            "http" => {
                let host = url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("http base URL needs a host"))?;
                let loopback = host.eq_ignore_ascii_case("localhost")
                    || url
                        .socket_addrs(|| None)
                        .map(|addrs| {
                            addrs.iter().all(|a| match a {
                                std::net::SocketAddr::V4(v4) => v4.ip().octets()[0] == 127,
                                std::net::SocketAddr::V6(v6) => v6.ip().is_loopback(),
                            })
                        })
                        .unwrap_or(false)
                    || host == "[::1]"
                    || host == "::1";
                if !loopback {
                    bail!("http base URL is loopback-only, got host {host}");
                }
            }
            scheme => bail!("base URL scheme must be https (loopback http allowed), got {scheme}"),
        }
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("base URL needs a hostname"))?;
        if !url.username().is_empty() || url.password().is_some() {
            bail!("base URL must not carry userinfo");
        }
        if url.query().is_some() {
            bail!("base URL must not carry a query");
        }
        if url.fragment().is_some() {
            bail!("base URL must not carry a fragment");
        }
        if !(url.path().is_empty() || url.path() == "/") {
            bail!("base URL path must be empty or /, got {}", url.path());
        }
        let _ = host;
        let authority = url.authority().to_string();
        if authority.is_empty() {
            bail!("base URL needs a hostname");
        }
        let origin = format!("{}://{}", url.scheme(), authority);
        Ok(Self { origin, authority })
    }

    /// `scheme://authority`, no trailing slash (e.g. `https://files.example`).
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// `host[:port]` as sent by clients in `Host`.
    pub fn authority(&self) -> &str {
        &self.authority
    }
}

/// Resolved WebRTC ICE server list for one room.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IceServerConfig {
    /// `stun:host:port` entries in try order; empty means host candidates only.
    pub servers: Vec<String>,
}

impl IceServerConfig {
    /// Resolves the effective list. `no_stun` conflicts with `custom`;
    /// `custom` replaces every default; otherwise the native server's own STUN
    /// responder comes first (only when native UDP is enabled) followed by the
    /// exact existing public chain. Reuses [`crate::holepunch`] sources.
    pub fn resolve(
        no_stun: bool,
        custom: &[String],
        server_udp: bool,
        control_host: &str,
        control_port: u16,
    ) -> Result<Self> {
        if no_stun && !custom.is_empty() {
            bail!("--web-transfer-no-stun conflicts with --web-transfer-stun");
        }
        if no_stun {
            return Ok(Self { servers: vec![] });
        }
        if !custom.is_empty() {
            return Ok(Self {
                servers: parse_stun_list(custom)?,
            });
        }
        let mut servers = Vec::new();
        if server_udp {
            servers.push(format!(
                "stun:{}",
                crate::holepunch::bore_stun_target(control_host, control_port)
            ));
        }
        servers.extend(
            crate::holepunch::PUBLIC_STUN
                .iter()
                .map(|s| format!("stun:{s}")),
        );
        Ok(Self { servers })
    }
}

/// Validated service configuration. Server-flag wiring lands in Phase 1; this
/// only proves the three validated pieces fit together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebTransferConfig {
    /// Same-origin root enabling the browser surface.
    pub base_url: WebTransferBaseUrl,
    /// Admission caps.
    pub limits: WebTransferLimits,
    /// Resolved ICE servers.
    pub ice: IceServerConfig,
}

impl WebTransferConfig {
    /// Builds a validated config; fails before any listener binds.
    pub fn new(
        base_url: WebTransferBaseUrl,
        limits: WebTransferLimits,
        ice: IceServerConfig,
    ) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            base_url,
            limits,
            ice,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_accept_only_canonical_lower_hex() {
        for raw in [
            "0123456789abcdef0123456789abcdef",
            "ffffffffffffffffffffffffffffffff",
        ] {
            let room: RoomId = raw.parse().expect("canonical room id");
            assert_eq!(room.to_string(), raw);
            assert_eq!(format!("{room:?}"), format!("RoomId({raw})"));
        }
        for bad in [
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef00",
            "0123456789abcdef0123456789abcdeg",
            " 0123456789abcdef0123456789abcdef",
            "0x0123456789abcdef0123456789abcdef",
            "",
        ] {
            assert!(bad.parse::<RoomId>().is_err(), "must reject {bad:?}");
            assert!(bad.parse::<PeerId>().is_err(), "must reject {bad:?}");
        }
        let peer: PeerId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        assert_eq!(peer.as_bytes().len(), 16);
        let token_hex = "ab".repeat(32);
        let token: MemberToken = token_hex.parse().unwrap();
        assert_eq!(token.to_string(), token_hex);
        assert!("AB".repeat(32).parse::<MemberToken>().is_err());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let hex64 = "cd".repeat(32);
        let member: MemberToken = hex64.parse().unwrap();
        let owner: OwnerToken = hex64.parse().unwrap();
        let key: RoomKey = hex64.parse().unwrap();
        let ticket: RelayTicket = "ef".repeat(16).parse().unwrap();
        for debug in [
            format!("{member:?}"),
            format!("{owner:?}"),
            format!("{key:?}"),
            format!("{ticket:?}"),
        ] {
            assert!(debug.contains("redacted"), "must redact: {debug}");
            assert!(!debug.contains("cdcd"), "leaks secret: {debug}");
            assert!(!debug.contains("efef"), "leaks secret: {debug}");
        }
    }

    #[test]
    fn token_digest_comparison_is_constant_time_api() {
        let a = sha256_bytes(b"member-token-a");
        let b = sha256_bytes(b"member-token-a");
        let c = sha256_bytes(b"member-token-b");
        assert!(token_digests_equal(&a, &b));
        assert!(!token_digests_equal(&a, &c));
        let member: MemberToken = "11".repeat(32).parse().unwrap();
        assert!(token_digests_equal(
            &member.sha256_hash(),
            &member.sha256_hash()
        ));
    }

    #[test]
    fn web_transfer_limits_match_documented_defaults() {
        let limits = WebTransferLimits::default();
        assert_eq!(limits.max_rooms, 1024);
        assert_eq!(limits.max_peers_global, 4096);
        assert_eq!(limits.max_peers_per_room, 32);
        assert_eq!(limits.max_offers_per_peer, 64);
        assert_eq!(limits.max_entries_per_offer, 10000);
        assert_eq!(limits.max_offer_bytes, 1099511627776);
        assert_eq!(limits.max_metadata_per_room_bytes, 16777216);
        assert_eq!(limits.max_metadata_total_bytes, 268435456);
        assert_eq!(limits.max_transfers_per_peer, 8);
        assert_eq!(limits.max_relays_global, 256);
        assert_eq!(limits.relay_rate_bytes_per_s, 104857600);
        assert_eq!(limits.owner_grace_secs, 60);
        limits.validate().expect("documented defaults validate");
    }

    #[test]
    fn web_transfer_limits_reject_invalid_relations() {
        let zero = WebTransferLimits {
            max_rooms: 0,
            ..WebTransferLimits::default()
        };
        assert!(zero.validate().is_err());

        for grace in [0, 4, 601, 3600] {
            let limits = WebTransferLimits {
                owner_grace_secs: grace,
                ..WebTransferLimits::default()
            };
            assert!(limits.validate().is_err(), "grace {grace} must fail");
        }
        for grace in [5, 60, 600] {
            let limits = WebTransferLimits {
                owner_grace_secs: grace,
                ..WebTransferLimits::default()
            };
            assert!(limits.validate().is_ok(), "grace {grace} must pass");
        }

        let flipped = {
            let base = WebTransferLimits::default();
            WebTransferLimits {
                max_metadata_total_bytes: base.max_metadata_per_room_bytes - 1,
                ..base
            }
        };
        assert!(flipped.validate().is_err());

        // Zero relay rate is the sole disabled sentinel and stays valid.
        let unthrottled = WebTransferLimits {
            relay_rate_bytes_per_s: 0,
            ..WebTransferLimits::default()
        };
        assert!(unthrottled.validate().is_ok());
    }

    #[test]
    fn base_url_accepts_https_and_loopback_http_only() {
        for (input, origin) in [
            ("https://files.example.com", "https://files.example.com"),
            ("https://files.example.com/", "https://files.example.com"),
            (
                "https://files.example.com:8443",
                "https://files.example.com:8443",
            ),
            ("http://localhost", "http://localhost"),
            ("http://localhost:8080/", "http://localhost:8080"),
            ("http://127.0.0.1:7835", "http://127.0.0.1:7835"),
            ("http://127.53.0.2/", "http://127.53.0.2"),
            ("http://[::1]/", "http://[::1]"),
            ("http://[::1]:8080", "http://[::1]:8080"),
        ] {
            let parsed = WebTransferBaseUrl::parse(input).expect(input);
            assert_eq!(parsed.origin(), origin, "input {input}");
            assert!(!parsed.origin().ends_with('/'), "slash: {input}");
        }
        for input in [
            "http://files.example.com",
            "http://192.168.1.2/",
            "http://[::2]/",
            "ftp://files.example.com/",
            "https://",
            "not-a-url",
        ] {
            assert!(
                WebTransferBaseUrl::parse(input).is_err(),
                "must reject {input}"
            );
        }
    }

    #[test]
    fn base_url_rejects_path_query_fragment_and_userinfo() {
        for input in [
            "https://files.example.com/transfer",
            "https://files.example.com/transfer/",
            "https://files.example.com/?room=1",
            "https://files.example.com/#m=abc",
            "https://user@files.example.com/",
            "https://user:pass@files.example.com/",
        ] {
            assert!(
                WebTransferBaseUrl::parse(input).is_err(),
                "must reject {input}"
            );
        }
    }

    #[test]
    fn ice_defaults_prepend_derived_server_only_with_udp() {
        let with_udp =
            IceServerConfig::resolve(false, &[], true, "bore.example.com", 7835).unwrap();
        assert_eq!(
            with_udp.servers.first().map(String::as_str),
            Some("stun:bore.example.com:7835")
        );
        let public: Vec<String> = crate::holepunch::PUBLIC_STUN
            .iter()
            .map(|s| format!("stun:{s}"))
            .collect();
        assert_eq!(&with_udp.servers[1..], public.as_slice());

        let without_udp =
            IceServerConfig::resolve(false, &[], false, "bore.example.com", 7835).unwrap();
        assert_eq!(without_udp.servers, public);
    }

    #[test]
    fn custom_stun_replaces_defaults() {
        let custom = vec!["stun:stun.example.com:3478".to_string()];
        let resolved =
            IceServerConfig::resolve(false, &custom, true, "bore.example.com", 7835).unwrap();
        assert_eq!(resolved.servers, custom);
        let valid = vec![
            " stun:[2001:db8::1]:3478, stun:192.0.2.1 ".to_string(),
            "stun:stun.example.com".to_string(),
        ];
        let resolved =
            IceServerConfig::resolve(false, &valid, true, "bore.example.com", 7835).unwrap();
        assert_eq!(
            resolved.servers,
            [
                "stun:[2001:db8::1]:3478",
                "stun:192.0.2.1",
                "stun:stun.example.com"
            ]
        );
        assert!(
            IceServerConfig::resolve(false, &["example.com".to_string()], true, "h", 1).is_err()
        );
        for malformed in [
            "stun:[]",
            "stun:[::1]:0",
            "stun:[::1]:abc",
            "stun:[::1]:65536",
            "stun:[::1]evil",
            "stun:[::1",
            "stun:::1",
            "stun:host/path",
            "stun:user@host",
            "stun:host?query",
            "stun:host name",
        ] {
            assert!(
                IceServerConfig::resolve(
                    false,
                    &[malformed.to_string()],
                    true,
                    "bore.example.com",
                    7835,
                )
                .is_err(),
                "malformed STUN target accepted: {malformed}"
            );
        }
    }

    #[test]
    fn no_stun_conflicts_with_custom_list() {
        let custom = vec!["stun:stun.example.com:3478".to_string()];
        assert!(IceServerConfig::resolve(true, &custom, true, "h", 1).is_err());
        let empty = IceServerConfig::resolve(true, &[], true, "h", 1).unwrap();
        assert!(empty.servers.is_empty());
    }
}

// --- Embedded frontend shell (0.2 scaffold) ---
//
// build.rs embeds the committed `web/transfer/dist` here so `cargo build`
// needs no Node. Routes arrive in Phase 2; until then this is data only.
// Generated code is `pub static WEB_TRANSFER_ASSETS`.
include!(concat!(env!("OUT_DIR"), "/web_transfer_assets.rs"));

#[cfg(test)]
mod assets_tests {
    use super::WEB_TRANSFER_ASSETS;

    #[test]
    fn embedded_web_transfer_assets_have_shell_worker_js_and_css() {
        let mut urls: Vec<&str> = WEB_TRANSFER_ASSETS.iter().map(|(url, _, _)| *url).collect();
        urls.sort_unstable();
        assert_eq!(
            urls,
            [
                "/transfer/assets/app.css",
                "/transfer/assets/app.js",
                "/transfer/assets/index.html",
                "/transfer/assets/offer-worker.js",
                "/transfer/assets/stage-worker.js",
            ],
            "generated map holds exactly the committed shell files"
        );
        for (url, bytes, content_type) in WEB_TRANSFER_ASSETS {
            assert!(!bytes.is_empty(), "{url} embeds no data");
            let want = if url.ends_with(".html") {
                "text/html; charset=utf-8"
            } else if url.ends_with(".js") {
                "text/javascript; charset=utf-8"
            } else {
                "text/css; charset=utf-8"
            };
            assert_eq!(*content_type, want, "{url} MIME");
        }
    }
}

// --- Phase 1: server configuration and bounded registry ---
//
// The registry owns room lifecycle state. Lock discipline: `RoomState` is a
// short synchronous mutex, never held across `.await`. Async cleanup captures
// the room itself (`Arc`/`Weak`) plus its epoch and removes only on pointer
// identity — never by re-resolving the registry key.

/// Machine-readable web-transfer failure. Messages carry room/peer IDs only;
/// tokens, keys, URLs and payload never enter an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTransferError {
    code: &'static str,
    message: String,
}

impl WebTransferError {
    /// Client is not allowed to do this (unknown/false token, stranger).
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            code: "UNAUTHORIZED",
            message: message.into(),
        }
    }

    /// A conflicting immutable object (same offer ID, different bytes/owner).
    pub fn offer_changed(message: impl Into<String>) -> Self {
        Self {
            code: "OFFER_CHANGED",
            message: message.into(),
        }
    }

    /// Known object the caller does not own (withdraw/cancel by a stranger).
    pub fn not_participant(message: impl Into<String>) -> Self {
        Self {
            code: "NOT_PARTICIPANT",
            message: message.into(),
        }
    }

    /// Referenced object does not exist.
    pub fn offer_not_found(message: impl Into<String>) -> Self {
        Self {
            code: "OFFER_NOT_FOUND",
            message: message.into(),
        }
    }

    /// Referenced transfer does not exist.
    pub fn transfer_not_found(message: impl Into<String>) -> Self {
        Self {
            code: "TRANSFER_NOT_FOUND",
            message: message.into(),
        }
    }

    /// Serving peer is not online.
    pub fn source_offline(message: impl Into<String>) -> Self {
        Self {
            code: "SOURCE_OFFLINE",
            message: message.into(),
        }
    }

    /// Source content no longer matches the published offer.
    pub fn source_changed(message: impl Into<String>) -> Self {
        Self {
            code: "SOURCE_CHANGED",
            message: message.into(),
        }
    }

    /// A configured bound refused the request.
    pub fn limit(message: impl Into<String>) -> Self {
        Self {
            code: "LIMIT_EXCEEDED",
            message: message.into(),
        }
    }

    /// Malformed request or stale state the client must not retry as-is.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "INVALID_MESSAGE",
            message: message.into(),
        }
    }

    /// Server-side fault (ID collision exhaustion, poisoned lock).
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "INTERNAL",
            message: message.into(),
        }
    }

    /// Protocol error code for `error` envelopes and logs.
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for WebTransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WebTransferError {}

/// Owner attachment state. Terminal destruction is removal plus room
/// cancellation, never a reusable `Closed` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerState {
    /// CLI holds the lease.
    Attached {
        /// Lease generation; increments on every resume.
        epoch: u64,
        /// Last inbound control activity (reaper tick-checked).
        last_recv: Instant,
    },
    /// Control lost abnormally; room dies at `expires_at` unless resumed.
    Detached {
        /// Lease generation the expiry task must still match.
        epoch: u64,
        /// Grace deadline from the configured owner grace.
        expires_at: Instant,
    },
}

/// Peer metadata placeholder (presence + display name; Phase 2 adds session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    /// Human label shown in the catalog; `None` until renamed.
    pub display_name: Option<String>,
}

/// Offer metadata: owner, retained canonical manifest and accounting. The
/// manifest is one shared `Arc<[u8]>` — snapshots serialize from it without
/// retaining a second copy in `RoomState`.
#[derive(Debug, Clone)]
pub struct OfferRecord {
    /// Publishing peer; only this peer may withdraw.
    pub owner: PeerId,
    /// Canonical manifest bytes (sorted keys, no whitespace).
    pub manifest: std::sync::Arc<[u8]>,
    /// 32-byte manifest MAC (shape-checked; the server holds no room key).
    pub mac: [u8; 32],
    /// Metadata bytes charged for this offer (manifest + fixed record).
    pub metadata_bytes: u64,
}

/// Server-side transfer lifecycle state. The record's presence in a
/// non-terminal state IS each party's active permit (counted against
/// `max_transfers_per_peer`); the relay semaphore permit lives in the live
/// attempt and drops exactly at the terminal transition.
#[derive(Debug)]
pub struct TransferRecord {
    /// Server-allocated transfer ID (map key, duplicated for events).
    pub transfer_id: TransferId,
    /// Offer being pulled.
    pub offer_id: OfferId,
    /// Publishing peer (only it may ready/reject for its side).
    pub source: PeerId,
    /// Requesting peer (only it may request/complete).
    pub recipient: PeerId,
    /// Digest over the selection, recomputed from stored state per message.
    pub selection_digest: [u8; 32],
    /// Selected file entry ID for `raw`; the reserved archive ID
    /// ([`crate::web_transfer_protocol::RESERVED_ZIP_ENTRY_ID`]) for `zip`.
    pub entry_id: u32,
    /// Rolling root of the selected entry, copied from the manifest at
    /// request time (completions compare against it without re-parsing).
    /// `None` for `zip`: an archive's root is dynamic and the server never
    /// learns it — the recipient checks it against the sealed FINAL frame.
    pub entry_root: Option<[u8; 32]>,
    /// Logical size of the selected entry in bytes, `None` for `zip` for the
    /// same reason as `entry_root`.
    pub entry_size: Option<u64>,
    /// Transfer mode.
    pub mode: TransferMode,
    /// Optional resume descriptor (shape-checked; verified at send).
    pub resume: Option<ResumeDescriptor>,
    /// Lifecycle state.
    pub state: TransferState,
    /// Current attempt number (1-based, checked increments).
    pub attempt_number: u64,
    /// Current attempt ID, if any attempt started.
    pub attempt_id: Option<AttemptId>,
    /// Live attempt resources (relay permit); `None` before admission.
    pub attempt: Option<AttemptState>,
    /// The direct attempt that most recently fell back, if any. It is what
    /// lets a recipient's `transfer.direct_failed` that LOST the race to the
    /// source's own report still be recognised as describing that attempt —
    /// see `adopt_late_recipient_resume`.
    pub last_direct_attempt: Option<AttemptId>,
    /// The attempt whose path has already been counted as CARRIED. An
    /// attempt counts at most once, at the first recipient report with
    /// verified bytes on it; a fallback that carried bytes on both paths
    /// counts on both, which is the truth about that transfer.
    pub carried_attempt: Option<AttemptId>,
    /// Set at the terminal transition; the record lingers this long so late
    /// duplicates answer idempotently, then GC frees it.
    pub terminated_at: Option<Instant>,
    /// Cancelled exactly once at the terminal transition. The relay pump and
    /// any parked attach waiter select on it; both exit without further
    /// notices (the transition that cancelled them already notified).
    pub cancel: CancellationToken,
}

/// Transfer lifecycle states (protocol order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    /// Validated and recorded; incoming send pending.
    Requested,
    /// `transfer.incoming` sent; awaiting the source reply.
    WaitingSource,
    /// Source ready; attempt minted, relay admission pending or retryable.
    /// Reached from the direct path's fallback, never from `source_ready`
    /// itself — Phase 4 always tries WebRTC before any relay slot is asked
    /// for.
    WaitingRelay,
    /// Source ready and the direct attempt is negotiating: both peers hold a
    /// `transfer.direct_start`, signaling is forwarded between exactly the
    /// two of them and the 10 s deadline is armed. **No relay permit is
    /// held in this state** — a direct transfer that never needs one must
    /// never take one.
    NegotiatingDirect {
        /// When `transfer.direct_start` went out; the deadline runs from it.
        started_at: Instant,
        /// The source declared its DataChannel usable.
        ready_source: bool,
        /// The recipient declared its DataChannel usable.
        ready_recipient: bool,
        /// Forwarded `rtc.ice` messages from the source (cap 128).
        candidates_source: u32,
        /// Forwarded `rtc.ice` messages from the recipient (cap 128).
        candidates_recipient: u32,
        /// The recipient's single `rtc.offer` has been forwarded.
        offer_seen: bool,
        /// The source's single `rtc.answer` has been forwarded.
        answer_seen: bool,
    },
    /// Both peers ready and `path_commit direct` sent: payload rides the
    /// DataChannel and the server is on no part of it.
    ActiveDirect,
    /// Both legs attached (Phase 3.2 relay pump owns the transition here).
    Active,
    /// Recipient-verified completion.
    Completed,
    /// Cancelled by a participant (or source rejection / cleanup).
    Cancelled,
    /// Failed attach or attempt fault (reached from Phase 3.2 paths).
    Failed,
}

impl TransferState {
    /// Whether the transfer still holds permits and accepts messages.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            TransferState::Requested
                | TransferState::WaitingSource
                | TransferState::WaitingRelay
                | TransferState::NegotiatingDirect { .. }
                | TransferState::ActiveDirect
                | TransferState::Active
        )
    }

    /// Whether the direct attempt is still negotiating (the only state the
    /// deadline timer and the signaling forwarders accept).
    pub fn is_negotiating_direct(self) -> bool {
        matches!(self, TransferState::NegotiatingDirect { .. })
    }

    /// Whether payload may be moving on this state, on either transport.
    /// `transfer.complete` is valid from exactly these two.
    pub fn is_carrying(self) -> bool {
        matches!(self, TransferState::Active | TransferState::ActiveDirect)
    }
}

/// SDP role a peer plays in the direct attempt. Fixed by the protocol and
/// never negotiated: the recipient is always the offerer (it also creates
/// the one DataChannel), the source always the answerer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectRole {
    /// Creates the DataChannel, the offer and sends `rtc.offer`.
    Offerer,
    /// Answers with `rtc.answer` and takes the channel from `ondatachannel`.
    Answerer,
}

impl DirectRole {
    /// Wire value carried by `transfer.direct_start`.
    pub fn as_str(self) -> &'static str {
        match self {
            DirectRole::Offerer => "offerer",
            DirectRole::Answerer => "answerer",
        }
    }
}

/// Transfer mode (only `raw` on the wire in this phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    /// Raw single-file bytes.
    Raw,
    /// The whole offer as one generated archive. The archive's length and
    /// root are DYNAMIC — they are not in the manifest, because the archive
    /// does not exist until it is generated — so the server holds neither
    /// and the recipient authenticates both from the sealed FINAL frame.
    Zip,
}

/// Resume descriptor carried on requests (verified at send time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeDescriptor {
    /// Verified `[start, endExclusive)` chunk ranges, sorted, disjoint.
    pub verified_ranges: Vec<(u64, u64)>,
    /// Expected output length in bytes.
    pub output_length: u64,
}

/// Live attempt resources. Dropping releases the relay permit immediately;
/// terminal transitions take it out first so release never waits for GC.
#[derive(Debug)]
pub struct AttemptState {
    /// Attempt this state belongs to.
    pub attempt_id: AttemptId,
    /// 1-based attempt number.
    pub attempt_number: u64,
    /// Held relay slot, if admission granted one.
    pub relay_permit: Option<OwnedSemaphorePermit>,
}

/// One relay ticket as stored: hash only, never the raw value.
#[derive(Debug, Clone)]
pub struct RelayTicketRecord {
    /// SHA-256 of the raw ticket (lookup key).
    pub ticket_hash: [u8; 32],
    /// Transfer the ticket attaches.
    pub transfer_id: TransferId,
    /// Attempt the ticket attaches.
    pub attempt_id: AttemptId,
    /// Peer the ticket was issued to.
    pub peer_id: PeerId,
    /// Leg the ticket opens.
    pub role: RelayRole,
    /// Deadline for both legs to attach (30 s from issue).
    pub expires_at: Instant,
}

/// Relay leg a ticket opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayRole {
    /// Offering peer's leg.
    Source,
    /// Requesting peer's leg.
    Recipient,
}

/// Validated ticket use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TicketGrant {
    /// Transfer the leg attaches.
    pub transfer_id: TransferId,
    /// Attempt the leg attaches.
    pub attempt_id: AttemptId,
    /// Peer the ticket was issued to.
    pub peer_id: PeerId,
}

/// Why a presented ticket is refused (typed for the attach path's mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketDeny {
    /// Unknown hash or already consumed.
    Unknown,
    /// Past its deadline.
    Expired,
    /// Bound to the other leg.
    RoleMismatch,
}

/// Room metadata: token hashes, owner lease, member maps and counters. Never
/// file bytes, ciphertext, filesystem paths, `File` handles or unbounded
/// queues.
#[derive(Debug)]
pub struct RoomState {
    /// SHA-256 of the member token (raw token never stored).
    pub member_hash: [u8; 32],
    /// SHA-256 of the owner token (raw token never stored).
    pub owner_hash: [u8; 32],
    /// Current owner lease.
    pub owner: OwnerState,
    /// Joined peers by ID.
    pub peers: HashMap<PeerId, PeerRecord>,
    /// Published offers by ID.
    pub offers: HashMap<OfferId, OfferRecord>,
    /// Live transfers by ID.
    pub transfers: HashMap<TransferId, TransferRecord>,
    /// Sum of charged metadata bytes in this room.
    pub metadata_bytes: u64,
    /// Monotonic mutation counter: bumped on join/rename/leave (and, from
    /// Phase 2.3, offer changes). Snapshots frame it in begin/end; every
    /// broadcast event carries the post-mutation value so receivers detect a
    /// missed message without a round trip.
    pub revision: u64,
}

/// Computes a future room revision with checked arithmetic. Callers obtain the
/// revision before mutating state so exhaustion cannot leave a partial change.
fn checked_room_revision(current: u64, steps: usize) -> Result<u64, WebTransferError> {
    let steps =
        u64::try_from(steps).map_err(|_| WebTransferError::internal("room revision exhausted"))?;
    current
        .checked_add(steps)
        .ok_or_else(|| WebTransferError::internal("room revision exhausted"))
}

/// Broadcast room lifecycle events (capacity 256).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    /// Room destroyed; `reason` is an opaque code (`owner-close`, `expired`,
    /// `revision-exhausted`).
    RoomClosed {
        /// Opaque reason code, safe for logs and control messages.
        reason: &'static str,
    },
    /// A peer completed `hello`; carries the post-join revision.
    PeerJoined {
        /// New member.
        peer: PeerId,
        /// Resolved display name (`None` serializes absent).
        display_name: Option<String>,
        /// `RoomState::revision` after the join.
        revision: u64,
    },
    /// A peer changed its display name; carries the post-rename revision.
    PeerRenamed {
        /// Renamed member.
        peer: PeerId,
        /// Normalized new name.
        display_name: String,
        /// `RoomState::revision` after the rename.
        revision: u64,
    },
    /// A peer left or was reaped; carries the post-removal revision.
    PeerLeft {
        /// Departed member.
        peer: PeerId,
        /// `RoomState::revision` after the removal.
        revision: u64,
    },
    /// An offer was published; carries the post-publish revision plus the
    /// retained manifest share (one `Arc` clone per subscriber, never a
    /// second retained copy).
    OfferAdded {
        /// Publishing member.
        peer: PeerId,
        /// Published offer.
        offer: OfferId,
        /// Canonical manifest bytes (shared with the record).
        manifest: std::sync::Arc<[u8]>,
        /// Manifest MAC.
        mac: [u8; 32],
        /// `RoomState::revision` after the publish.
        revision: u64,
    },
    /// An offer was withdrawn or died with its owner.
    OfferRemoved {
        /// Owning member.
        peer: PeerId,
        /// Withdrawn offer.
        offer: OfferId,
        /// `RoomState::revision` after the removal.
        revision: u64,
    },
}

/// One live room: immutable limits, short-locked state, broadcast events, a
/// cancellation token and a monotonic owner epoch.
#[derive(Debug)]
pub struct WebTransferRoom {
    /// Server-generated room ID (registry key).
    pub id: RoomId,
    /// Immutable admission caps for this room.
    pub limits: WebTransferLimits,
    /// Short synchronous lock; never held across `.await`.
    pub state: std::sync::Mutex<RoomState>,
    /// Lifecycle broadcast (capacity 256).
    pub events: broadcast::Sender<RoomEvent>,
    /// Cancelled on destroy; waiters select on it.
    pub cancel: CancellationToken,
    /// Owner lease generation.
    pub epoch: AtomicU64,
    /// Set once by `destroy`; racing close/timeout/shutdown converge here.
    pub(crate) destroyed: AtomicBool,
    /// Back-pointer for counter release; empty once the server is gone.
    pub(crate) registry: std::sync::Weak<RegistryInner>,
    /// Holds one global room slot for the room's whole life.
    #[allow(dead_code)]
    room_permit: OwnedSemaphorePermit,
    /// Live control sessions by peer for targeted delivery (incoming,
    /// tickets, cancel/complete notices). Bounded by the per-room peer cap.
    /// Lock order: `state` first, then `sessions` — never the reverse.
    pub(crate) sessions: std::sync::Mutex<HashMap<PeerId, mpsc::Sender<String>>>,
    /// Shared relay throttle for this room's pumps (rate from limits, burst
    /// exactly 2×rate capped at 200 MiB, disabled when the rate is 0).
    /// Short lock per forwarded frame; never held across await.
    pub(crate) relay_throttle: std::sync::Mutex<RelayThrottle>,
}

impl WebTransferRoom {
    /// Best-effort targeted delivery to one live control session. Never
    /// blocks: a full queue means the peer's own slow-path machinery reaps
    /// it; the transition already committed.
    pub(crate) fn send_to(&self, peer_id: PeerId, message: String) -> bool {
        let sender = match self.sessions.lock() {
            Ok(sessions) => sessions.get(&peer_id).cloned(),
            Err(_) => None,
        };
        match sender {
            Some(tx) => tx.try_send(message).is_ok(),
            None => false,
        }
    }
}

/// RAII peer membership: holds one global peer slot; dropping removes the
/// peer from the room map and releases the slot exactly once.
#[derive(Debug)]
pub struct PeerGuard {
    room: Arc<WebTransferRoom>,
    peer_id: PeerId,
    spent: bool,
    _permit: OwnedSemaphorePermit,
}

impl PeerGuard {
    /// Peer this guard admits.
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Room this guard admits into.
    pub fn room(&self) -> &Arc<WebTransferRoom> {
        &self.room
    }
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        if self.spent {
            return;
        }
        self.spent = true;
        // The guard owns its room Arc: cleanup touches exactly this room and
        // this peer, never whatever the registry key resolves to now (a room
        // re-inserted under the same ID is a different Arc and is untouched).
        self.room.remove_peer(self.peer_id);
        if let Some(registry) = self.room.registry.upgrade() {
            registry.peers_current.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// RAII metadata reservation against the server-wide metadata budget.
pub struct MetadataReservation {
    registry: std::sync::Weak<RegistryInner>,
    bytes: u64,
    spent: bool,
}

impl MetadataReservation {
    /// Converts the reservation into a persistent charge: the bytes stay
    /// accounted until explicitly released with [`release_offer_metadata`].
    /// Offer records own persistent charges; transient work uses the RAII
    /// drop.
    pub(crate) fn spend(mut self) {
        self.spent = true;
    }
}

impl Drop for MetadataReservation {
    fn drop(&mut self) {
        if self.spent {
            return;
        }
        self.spent = true;
        if let Some(registry) = self.registry.upgrade() {
            registry
                .metadata_current
                .fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}

/// RAII transfer slot: counts one active transfer server-wide.
#[allow(dead_code)]
pub struct TransferSlot {
    registry: std::sync::Weak<RegistryInner>,
    spent: bool,
}

#[allow(dead_code)]
impl TransferSlot {
    fn release(&mut self) {
        if self.spent {
            return;
        }
        self.spent = true;
        if let Some(registry) = self.registry.upgrade() {
            registry.transfers_current.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[allow(dead_code)]
impl Drop for TransferSlot {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) struct RegistryInner {
    config: WebTransferConfig,
    rooms: DashMap<RoomId, Arc<WebTransferRoom>>,
    room_permits: Arc<Semaphore>,
    peer_permits: Arc<Semaphore>,
    relay_permits: Arc<Semaphore>,
    /// Upgrades in flight; released the moment the handshake finishes, so it
    /// bounds the HANDSHAKE and never the session that follows it.
    handshake_permits: Arc<Semaphore>,
    peers_current: AtomicU64,
    metadata_current: AtomicU64,
    transfers_current: AtomicU64,
    /// Live offers across all rooms: incremented when a record enters a
    /// room's catalog and decremented on every path that removes one
    /// (withdraw, the owner peer leaving, room destruction).
    offers_current: AtomicU64,
    /// Cumulative CIPHERTEXT bytes forwarded by the relay. Counted per
    /// forwarded frame, not at the end of a pump: a total that only moves
    /// when a transfer finishes cannot answer "is the relay busy now".
    relay_bytes_total: AtomicU64,
    /// Cumulative transfers that reached `Completed` / `Cancelled`.
    completed_total: AtomicU64,
    cancelled_total: AtomicU64,
    /// Cumulative admissions REFUSED because a configured cap or rate was
    /// reached: room/peer/offer/metadata/transfer/relay budgets, the
    /// pre-auth rate limiter and the pending-handshake semaphore. A failure
    /// is not a refusal and is deliberately not counted here.
    rejected_total: AtomicU64,
    /// Attempts that CARRIED on each path, counted once per attempt at the
    /// first recipient report with verified bytes on it — never at the
    /// commit. F-12 measured the other shape on the native side:
    /// `direct_stream_opens` counted attempts and climbed 1 -> 12 during a
    /// blackout that moved nothing, so an operator reading it saw a healthy
    /// direct path serving a tunnel that was entirely on the relay. These
    /// two follow the ONLY party that knows (the recipient verified the
    /// bytes) and the ONLY path the server itself committed.
    direct_carried: AtomicU64,
    relay_carried: AtomicU64,
    /// Pre-authentication rate state by source IP (bounded LRU + overflow
    /// bucket). Guarded by a short synchronous lock; never held across await.
    pre_auth: std::sync::Mutex<PreAuthLimiter>,
    /// Pre-authentication FAILURES by source IP, sampled logarithmically.
    /// Same bound and same TTL as the limiter above: a log line per refusal
    /// is itself an amplifier (one scanner writes the disk), and a counter
    /// per IP with no bound is the same memory leak the limiter avoids.
    auth_failures: std::sync::Mutex<AuthFailureSampler>,
    /// Live relay tickets by hash (hash only, never raw values). Entries die
    /// on consume, on their terminal transition, or lazily at expiry — never
    /// by timer task.
    tickets: DashMap<[u8; 32], RelayTicketRecord>,
    /// First-leg sockets parked while waiting for their pair, keyed by
    /// `(transfer, attempt)`. The parked task owns its own socket and removes
    /// the entry on every exit; entries hold only a handoff sender plus
    /// metadata, never payload.
    relay_waiters: DashMap<(TransferId, AttemptId), RelayWaiter>,
    /// How long relay admission waits for a slot (30 s; tests shorten it).
    admit_timeout: std::sync::Mutex<Duration>,
    /// How long the direct attempt has to reach both-ready before the
    /// automatic relay fallback (10 s; tests shorten it).
    direct_deadline: std::sync::Mutex<Duration>,
    /// Relay ticket lifetime (30 s; both legs must attach inside it).
    ticket_ttl: Duration,
}

/// Server-side room registry: exactly one per enabled server, shared by every
/// connection task. Totals are immutable; live gauges are atomics/semaphores
/// that Phase 6 publishes separately (never conflated, P-11).
#[derive(Clone)]
pub struct WebTransferRegistry {
    inner: Arc<RegistryInner>,
}

/// Room-ID collision retries before creation fails `INTERNAL`.
const ROOM_ID_RETRIES: usize = 8;

impl WebTransferRegistry {
    /// Builds the registry; validates caps before any listener binds.
    pub fn new(config: WebTransferConfig) -> Result<Self, WebTransferError> {
        config
            .limits
            .validate()
            .map_err(|e| WebTransferError::invalid(format!("{e:#}")))?;
        let limits = config.limits;
        let new_sem = |n: u64| -> Result<Arc<Semaphore>, WebTransferError> {
            let permits = usize::try_from(n)
                .map_err(|_| WebTransferError::invalid("web-transfer cap does not fit usize"))?;
            Ok(Arc::new(Semaphore::new(permits)))
        };
        Ok(Self {
            inner: Arc::new(RegistryInner {
                config,
                rooms: DashMap::new(),
                room_permits: new_sem(limits.max_rooms)?,
                peer_permits: new_sem(limits.max_peers_global)?,
                relay_permits: new_sem(limits.max_relays_global)?,
                handshake_permits: Arc::new(Semaphore::new(WEB_TRANSFER_PENDING_HANDSHAKES)),
                peers_current: AtomicU64::new(0),
                metadata_current: AtomicU64::new(0),
                transfers_current: AtomicU64::new(0),
                offers_current: AtomicU64::new(0),
                relay_bytes_total: AtomicU64::new(0),
                completed_total: AtomicU64::new(0),
                cancelled_total: AtomicU64::new(0),
                rejected_total: AtomicU64::new(0),
                direct_carried: AtomicU64::new(0),
                relay_carried: AtomicU64::new(0),
                pre_auth: std::sync::Mutex::new(PreAuthLimiter::default()),
                auth_failures: std::sync::Mutex::new(AuthFailureSampler::default()),
                tickets: DashMap::new(),
                relay_waiters: DashMap::new(),
                admit_timeout: std::sync::Mutex::new(WEB_TRANSFER_RELAY_ADMIT_TIMEOUT),
                direct_deadline: std::sync::Mutex::new(WEB_TRANSFER_DIRECT_DEADLINE),
                ticket_ttl: WEB_TRANSFER_TICKET_TTL,
            }),
        })
    }

    /// Immutable configured totals (never move with load).
    pub fn totals(&self) -> WebTransferLimits {
        self.inner.config.limits
    }

    /// Validated service configuration.
    pub fn config(&self) -> &WebTransferConfig {
        &self.inner.config
    }

    /// Live room count (derived from the semaphore, not a second counter).
    /// Phase 6 publishes gauges from these; tests are the first callers.
    pub fn current_rooms(&self) -> u64 {
        self.inner
            .config
            .limits
            .max_rooms
            .saturating_sub(self.inner.room_permits.available_permits() as u64)
    }

    /// Live peer count across all rooms.
    pub fn current_peers(&self) -> u64 {
        self.inner.peers_current.load(Ordering::Relaxed)
    }

    /// Live metadata bytes across all rooms.
    pub fn current_metadata_bytes(&self) -> u64 {
        self.inner.metadata_current.load(Ordering::Relaxed)
    }

    /// Live transfer count across all rooms (records in non-terminal
    /// states). First consumers arrive in Phase 3.
    pub fn current_transfers(&self) -> u64 {
        self.inner.transfers_current.load(Ordering::Relaxed)
    }

    /// Live offer count across all rooms.
    pub fn current_offers(&self) -> u64 {
        self.inner.offers_current.load(Ordering::Relaxed)
    }

    /// Relay slots still FREE right now. The CONFIGURED total is
    /// `max_relays_global`; publishing both is P-11's rule, and a zero here
    /// is the alarming value, never the absent one.
    pub fn relay_slots_available(&self) -> u64 {
        self.inner.relay_permits.available_permits() as u64
    }

    /// Cumulative ciphertext bytes the relay has forwarded.
    pub fn relay_ciphertext_bytes(&self) -> u64 {
        self.inner.relay_bytes_total.load(Ordering::Relaxed)
    }

    /// Cumulative transfers that reached `Completed`.
    pub fn completed_total(&self) -> u64 {
        self.inner.completed_total.load(Ordering::Relaxed)
    }

    /// Cumulative transfers that reached `Cancelled`.
    pub fn cancelled_total(&self) -> u64 {
        self.inner.cancelled_total.load(Ordering::Relaxed)
    }

    /// Cumulative admissions refused for capacity (see `rejected_total`).
    pub fn rejected_total(&self) -> u64 {
        self.inner.rejected_total.load(Ordering::Relaxed)
    }

    /// Counts one capacity refusal and hands the error back to propagate.
    ///
    /// Every LIMIT_EXCEEDED the registry produces goes through here, so the
    /// counter cannot drift from the refusals an operator actually sees: a
    /// new cap added later is counted by construction if it is refused with
    /// this helper.
    fn refused(&self, error: WebTransferError) -> WebTransferError {
        self.inner.rejected_total.fetch_add(1, Ordering::Relaxed);
        error
    }

    /// Cumulative attempts that carried verified bytes on the DIRECT path.
    /// A TOTAL, never a gauge (P-11): it only grows, and it answers "how
    /// many transfers has this server seen ride the DataChannel", which no
    /// live value can express.
    pub fn direct_carried(&self) -> u64 {
        self.inner.direct_carried.load(Ordering::Relaxed)
    }

    /// Cumulative attempts that carried verified bytes on the RELAY path.
    /// Same rule as [`Self::direct_carried`]; the pair is only meaningful
    /// read together, because a fallback contributes to both.
    pub fn relay_carried(&self) -> u64 {
        self.inner.relay_carried.load(Ordering::Relaxed)
    }

    /// Live relay-pair count (derived from the semaphore).
    pub fn current_relays(&self) -> u64 {
        self.inner
            .config
            .limits
            .max_relays_global
            .saturating_sub(self.inner.relay_permits.available_permits() as u64)
    }

    /// Overrides the relay admission wait (default 30 s). Intended for
    /// tests; production uses [`WEB_TRANSFER_RELAY_ADMIT_TIMEOUT`].
    pub fn set_relay_admit_timeout(&self, timeout: Duration) {
        if let Ok(mut slot) = self.inner.admit_timeout.lock() {
            *slot = timeout;
        }
    }

    /// Creates a room: reserves the global slot, generates a nonzero ID
    /// (bounded collision retries, never overwrites), inserts atomically.
    pub fn create_room(
        &self,
        member_hash: [u8; 32],
        owner_hash: [u8; 32],
    ) -> Result<Arc<WebTransferRoom>, WebTransferError> {
        for _ in 0..ROOM_ID_RETRIES {
            let id = generate_room_id();
            if self.inner.rooms.contains_key(&id) {
                continue;
            }
            let permit = Arc::clone(&self.inner.room_permits)
                .try_acquire_owned()
                .map_err(|_| {
                    self.refused(WebTransferError::limit(
                        "web-transfer room budget exhausted",
                    ))
                })?;
            let room = Arc::new(WebTransferRoom {
                id,
                limits: self.inner.config.limits,
                state: std::sync::Mutex::new(RoomState {
                    member_hash,
                    owner_hash,
                    owner: OwnerState::Attached {
                        epoch: 0,
                        last_recv: Instant::now(),
                    },
                    peers: HashMap::new(),
                    offers: HashMap::new(),
                    transfers: HashMap::new(),
                    metadata_bytes: 0,
                    revision: 0,
                }),
                events: broadcast::channel(256).0,
                cancel: CancellationToken::new(),
                epoch: AtomicU64::new(0),
                destroyed: AtomicBool::new(false),
                registry: Arc::downgrade(&self.inner),
                room_permit: permit,
                sessions: std::sync::Mutex::new(HashMap::new()),
                relay_throttle: std::sync::Mutex::new(RelayThrottle::new(
                    self.inner.config.limits.relay_rate_bytes_per_s,
                )),
            });
            match self.inner.rooms.entry(id) {
                dashmap::mapref::entry::Entry::Occupied(_) => continue,
                dashmap::mapref::entry::Entry::Vacant(slot) => {
                    slot.insert(Arc::clone(&room));
                    return Ok(room);
                }
            }
        }
        Err(WebTransferError::internal("room ID collision exhaustion"))
    }

    /// Removes the entry only when it still holds `expected` (pointer
    /// identity, P-14 rule). A stale expiry can never evict a resumed or
    /// reused room. Pairing rule: whoever removes an entry also runs
    /// `destroy` on it — the expiry monitor relies on this when its `Weak`
    /// no longer upgrades.
    pub fn remove_room_if_current(&self, id: RoomId, expected: &Arc<WebTransferRoom>) -> bool {
        self.inner
            .rooms
            .remove_if(&id, |_, current| Arc::ptr_eq(current, expected))
            .is_some()
    }

    /// Looks a room up by ID (control/peer paths re-resolve under short locks
    /// and re-check epoch; async cleanup never does this).
    pub fn room(&self, id: RoomId) -> Option<Arc<WebTransferRoom>> {
        self.inner.rooms.get(&id).map(|r| Arc::clone(&r))
    }

    /// Admits a peer: validates (and defaults) the display name first so a
    /// malformed name fails before any permit moves, then global budget, then
    /// the room lock with a room budget recheck. Any rejection drops the
    /// global permit (rollback). Duplicate joins are rejected so one global
    /// slot never backs two guards. The lock is released before the join
    /// broadcast (no producer awaits while holding `RoomState`).
    pub fn join_peer(
        &self,
        room: &Arc<WebTransferRoom>,
        peer_id: PeerId,
        display_name: Option<String>,
    ) -> Result<PeerGuard, WebTransferError> {
        let display_name = match display_name {
            Some(raw) => Some(normalize_display_name(&raw)?),
            None => None,
        };
        let permit = Arc::clone(&self.inner.peer_permits)
            .try_acquire_owned()
            .map_err(|_| {
                self.refused(WebTransferError::limit(
                    "web-transfer peer budget exhausted",
                ))
            })?;
        let event = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            if state.peers.len() >= room.limits.max_peers_per_room as usize {
                return Err(self.refused(WebTransferError::limit("room peer budget exhausted")));
            }
            if state.peers.contains_key(&peer_id) {
                return Err(WebTransferError::invalid("peer already joined"));
            }
            let revision = checked_room_revision(state.revision, 1)?;
            let resolved = display_name.or_else(|| Some(default_display_name(peer_id)));
            state.peers.insert(
                peer_id,
                PeerRecord {
                    display_name: resolved.clone(),
                },
            );
            state.revision = revision;
            RoomEvent::PeerJoined {
                peer: peer_id,
                display_name: resolved,
                revision: state.revision,
            }
        };
        self.inner.peers_current.fetch_add(1, Ordering::Relaxed);
        let _ = room.events.send(event);
        Ok(PeerGuard {
            room: Arc::clone(room),
            peer_id,
            spent: false,
            _permit: permit,
        })
    }

    /// Renames a joined peer: normalizes the raw name, stores it, bumps the
    /// revision and broadcasts. `PeerGuard` cleanup never calls this.
    pub fn rename_peer(
        &self,
        room: &Arc<WebTransferRoom>,
        peer_id: PeerId,
        raw_name: &str,
    ) -> Result<String, WebTransferError> {
        let display_name = normalize_display_name(raw_name)?;
        let event = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            if !state.peers.contains_key(&peer_id) {
                return Err(WebTransferError::invalid("unknown peer"));
            }
            let revision = checked_room_revision(state.revision, 1)?;
            let record = state.peers.get_mut(&peer_id).expect("peer checked above");
            record.display_name = Some(display_name.clone());
            state.revision = revision;
            RoomEvent::PeerRenamed {
                peer: peer_id,
                display_name: display_name.clone(),
                revision: state.revision,
            }
        };
        let _ = room.events.send(event);
        Ok(display_name)
    }

    /// Publishes an offer: validates ownership and caps transactionally
    /// (global metadata, per-room metadata, per-peer count — every failure
    /// rolls all of them back), stores one shared canonical manifest plus
    /// its MAC, bumps the revision and broadcasts. An identical republish
    /// (same ID, bytes and MAC) acks idempotently; the same ID with any
    /// different byte conflicts. The structural manifest checks already ran
    /// in [`crate::web_transfer_protocol::parse_manifest`].
    pub fn publish_offer(
        &self,
        room: &Arc<WebTransferRoom>,
        peer_id: PeerId,
        offer_id: OfferId,
        manifest: &crate::web_transfer_protocol::Manifest,
        canonical: std::sync::Arc<[u8]>,
        mac: [u8; 32],
    ) -> Result<PublishOutcome, WebTransferError> {
        if manifest.offer != offer_id {
            return Err(WebTransferError::invalid(
                "offer ID must match its manifest",
            ));
        }
        let charge = offer_charge(canonical.len())?;
        let (event, outcome) = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            if !state.peers.contains_key(&peer_id) {
                return Err(WebTransferError::invalid("unknown peer"));
            }
            match state.offers.get(&offer_id) {
                Some(existing)
                    if existing.owner == peer_id
                        && existing.manifest.as_ref() == canonical.as_ref()
                        && existing.mac == mac =>
                {
                    return Ok(PublishOutcome::Idempotent);
                }
                Some(_) => {
                    return Err(WebTransferError::offer_changed(
                        "offer ID already published",
                    ));
                }
                None => {}
            }
            let reservation = self
                .try_reserve_metadata(charge)
                .map_err(|_| WebTransferError::limit("web-transfer metadata budget exhausted"))?;
            let owned = state.offers.values().filter(|o| o.owner == peer_id).count();
            if owned >= room.limits.max_offers_per_peer as usize {
                return Err(self.refused(WebTransferError::limit("peer offer budget exhausted")));
            }
            let next = state
                .metadata_bytes
                .checked_add(charge)
                .ok_or_else(|| WebTransferError::limit("web-transfer metadata budget exhausted"))?;
            if next > room.limits.max_metadata_per_room_bytes {
                return Err(self.refused(WebTransferError::limit(
                    "web-transfer metadata budget exhausted",
                )));
            }
            let revision = checked_room_revision(state.revision, 1)?;
            state.metadata_bytes = next;
            self.inner.offers_current.fetch_add(1, Ordering::Relaxed);
            state.offers.insert(
                offer_id,
                OfferRecord {
                    owner: peer_id,
                    manifest: std::sync::Arc::clone(&canonical),
                    mac,
                    metadata_bytes: charge,
                },
            );
            state.revision = revision;
            reservation.spend();
            (
                RoomEvent::OfferAdded {
                    peer: peer_id,
                    offer: offer_id,
                    manifest: std::sync::Arc::clone(&canonical),
                    mac,
                    revision: state.revision,
                },
                PublishOutcome::Created,
            )
        };
        let _ = room.events.send(event);
        Ok(outcome)
    }

    /// Withdraws an offer: only its owner may. A missing or already-withdrawn
    /// ID is a terminal ack (withdraw is idempotent); a stranger hears
    /// `NOT_PARTICIPANT`. Removal releases room and global metadata and
    /// broadcasts only the IDs. Transfer cancellation for the offer arrives
    /// with Phase 3 (no transfers exist yet).
    pub fn withdraw_offer(
        &self,
        room: &Arc<WebTransferRoom>,
        peer_id: PeerId,
        offer_id: OfferId,
    ) -> Result<WithdrawOutcome, WebTransferError> {
        let (event, outbox) = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            match state.offers.get(&offer_id) {
                None => return Ok(WithdrawOutcome::AlreadyGone),
                Some(record) if record.owner != peer_id => {
                    return Err(WebTransferError::not_participant(
                        "only the offer owner withdraws",
                    ));
                }
                Some(_) => {}
            }
            let revision = checked_room_revision(state.revision, 1)?;
            let record = state.offers.remove(&offer_id).expect("offer checked above");
            self.inner.offers_current.fetch_sub(1, Ordering::Relaxed);
            state.metadata_bytes = state.metadata_bytes.saturating_sub(record.metadata_bytes);
            let inner = room.registry.upgrade();
            if let Some(registry) = &inner {
                registry
                    .metadata_current
                    .fetch_sub(record.metadata_bytes, Ordering::Relaxed);
            }
            // Withdrawing cancels every transfer of this offer first.
            let mut outbox = Vec::new();
            cancel_where_locked(
                inner.as_deref(),
                &mut state,
                peer_id,
                |transfer| transfer.offer_id == offer_id,
                &mut outbox,
            );
            state.revision = revision;
            (
                RoomEvent::OfferRemoved {
                    peer: peer_id,
                    offer: offer_id,
                    revision: state.revision,
                },
                outbox,
            )
        };
        let _ = room.events.send(event);
        for (peer, message) in outbox {
            let _ = room.send_to(peer, message);
        }
        Ok(WithdrawOutcome::Removed)
    }

    /// Reserves server-wide metadata bytes (CAS loop; releases on drop).
    pub fn try_reserve_metadata(
        &self,
        bytes: u64,
    ) -> Result<MetadataReservation, WebTransferError> {
        let cap = self.inner.config.limits.max_metadata_total_bytes;
        let mut current = self.inner.metadata_current.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(bytes)
                .ok_or_else(|| WebTransferError::limit("web-transfer metadata budget exhausted"))?;
            if next > cap {
                return Err(self.refused(WebTransferError::limit(
                    "web-transfer metadata budget exhausted",
                )));
            }
            match self.inner.metadata_current.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(MetadataReservation {
                        registry: Arc::downgrade(&self.inner),
                        bytes,
                        spent: false,
                    });
                }
                Err(seen) => current = seen,
            }
        }
    }

    /// Acquires one global relay-pair slot. First consumers arrive in Phase 3.
    #[allow(dead_code)]
    pub fn try_acquire_relay(&self) -> Result<OwnedSemaphorePermit, WebTransferError> {
        Arc::clone(&self.inner.relay_permits)
            .try_acquire_owned()
            .map_err(|_| {
                self.refused(WebTransferError::limit(
                    "web-transfer relay budget exhausted",
                ))
            })
    }

    /// Acquires one pending-handshake slot, or `None` when they are all held.
    ///
    /// Non-blocking by design: queueing here would convert a burst into a
    /// pile of waiting tasks, which is the state this bound exists to refuse.
    /// The caller answers a generic `503` and closes — the refusal says
    /// nothing about the room, which is the same rule every other pre-auth
    /// refusal on this surface follows.
    pub fn try_acquire_handshake(&self) -> Option<OwnedSemaphorePermit> {
        let permit = Arc::clone(&self.inner.handshake_permits)
            .try_acquire_owned()
            .ok();
        if permit.is_none() {
            // A 503 is a refusal for capacity like any other, and this one
            // happens BEFORE there is a session to report it on: if it were
            // not counted here it would be counted nowhere.
            self.inner.rejected_total.fetch_add(1, Ordering::Relaxed);
        }
        permit
    }

    /// Pending-handshake slots still free (tests and the audit gate).
    pub fn handshake_slots_available(&self) -> usize {
        self.inner.handshake_permits.available_permits()
    }

    /// Records a pre-authentication FAILURE from `ip` and answers with the
    /// running count when this one is worth a log line (1st, 2nd, 4th, 8th …).
    ///
    /// The caller logs the IP and the count and NOTHING else: which room was
    /// addressed, and whether the token was absent, malformed or simply
    /// wrong, are all things the refusal deliberately keeps
    /// indistinguishable, and a log line is not an exception to that.
    pub fn note_auth_failure(&self, ip: IpAddr) -> Option<u64> {
        let mut sampler = self.inner.auth_failures.lock().ok()?;
        sampler.note(ip, Instant::now())
    }

    /// Pre-authentication rate check by source IP. Consumed once per inbound
    /// pre-auth message (any first message, hello or not); `false` means the
    /// connection must close with `4008` without further parsing.
    pub fn check_pre_auth(&self, ip: IpAddr) -> bool {
        let Ok(mut limiter) = self.inner.pre_auth.lock() else {
            return true;
        };
        let allowed = limiter.check(ip, Instant::now());
        if !allowed {
            self.inner.rejected_total.fetch_add(1, Ordering::Relaxed);
        }
        allowed
    }
}

/// Outcome of [`WebTransferRegistry::publish_offer`]: both variants ack
/// with the offer ID; only `Created` mutates state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// Stored and broadcast.
    Created,
    /// Byte-identical republish; state untouched.
    Idempotent,
}

/// Outcome of [`WebTransferRegistry::withdraw_offer`]: both variants ack
/// with the offer ID; only `Removed` mutates state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawOutcome {
    /// Removed, metadata released, broadcast sent.
    Removed,
    /// Missing or already withdrawn; state untouched.
    AlreadyGone,
}

/// Metadata charged for one retained offer: the canonical manifest bytes
/// plus the fixed record (32-byte MAC, two 16-byte IDs). Checked
/// arithmetic; the caller reserves it globally before the room lock and
/// releases both counters on removal.
pub(crate) fn offer_charge(manifest_len: usize) -> Result<u64, WebTransferError> {
    let len = u64::try_from(manifest_len)
        .map_err(|_| WebTransferError::limit("web-transfer metadata budget exhausted"))?;
    len.checked_add(64)
        .ok_or_else(|| WebTransferError::limit("web-transfer metadata budget exhausted"))
}

/// One catalog entry for snapshot serialization: parsed views over the
/// retained `Arc<[u8]>` manifest (referenced, never copied in `RoomState`).
pub(crate) struct OfferView {
    /// Owning member.
    pub peer: PeerId,
    /// Published offer.
    pub offer: OfferId,
    /// Canonical manifest bytes (shared with the record).
    pub manifest: std::sync::Arc<[u8]>,
    /// Manifest MAC.
    pub mac: [u8; 32],
}

/// Sorted catalog contents under a short lock: every offer by ascending ID.
pub(crate) fn offer_views(room: &Arc<WebTransferRoom>) -> Result<Vec<OfferView>, WebTransferError> {
    let state = room
        .state
        .lock()
        .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
    let mut views: Vec<OfferView> = state
        .offers
        .iter()
        .map(|(id, record)| OfferView {
            peer: record.owner,
            offer: *id,
            manifest: std::sync::Arc::clone(&record.manifest),
            mac: record.mac,
        })
        .collect();
    views.sort_by_key(|view| view.offer.to_string());
    Ok(views)
}

// --- Phase 2.2: peer sessions, snapshots, request cache and rates ---
//
// The control actor (`serve_control_websocket` in `web_transfer_http.rs`) is
// a single task per peer: one bounded outgoing queue, one broadcast receiver
// and one socket reader. Everything below is the synchronous state it drives;
// nothing here awaits while holding `RoomState`.

/// Deadline for the first control message (`hello`) after the handshake.
pub const WEB_TRANSFER_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Uniform delay before closing on any pre-auth failure (absent room, bad
/// token, exhausted cap): identical response, identical timing, no oracle.
pub const WEB_TRANSFER_AUTH_FAIL_DELAY: Duration = Duration::from_millis(500);
/// How long relay admission waits for a global slot before answering
/// `RELAY_BUSY` and leaving the transfer resumable without a permit.
pub const WEB_TRANSFER_RELAY_ADMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Relay ticket lifetime: both legs must attach inside it (Phase 3.2).
pub const WEB_TRANSFER_TICKET_TTL: Duration = Duration::from_secs(30);
/// Terminal transfer records linger this long so late duplicates answer
/// idempotently, then GC frees them (permits release at transition, not GC).
pub const WEB_TRANSFER_TERMINAL_RETENTION: Duration = Duration::from_secs(5 * 60);
/// Peer-ID/transfer/attempt ID collision retries before failing `INTERNAL`.
pub const WEB_TRANSFER_PEER_ID_RETRIES: usize = 8;
/// Capacity of one session's outgoing control queue; a full queue closes the
/// slow peer and lets its `PeerGuard` clean up.
pub const WEB_TRANSFER_OUTGOING_CAP: usize = 64;
/// Sustained rate of the per-session control bucket (messages/second).
pub const WEB_TRANSFER_CONTROL_RATE_PER_SEC: f64 = 30.0;
/// Burst of the per-session control bucket (messages).
pub const WEB_TRANSFER_CONTROL_BURST: f64 = 60.0;
/// Sustained rate of the per-session mutation bucket (messages/second).
pub const WEB_TRANSFER_MUTATION_RATE_PER_SEC: f64 = 4.0;
/// Burst of the per-session mutation bucket (messages).
pub const WEB_TRANSFER_MUTATION_BURST: f64 = 8.0;
/// Sustained rate of the per-IP pre-auth bucket (attempts/second: 10/min).
pub const WEB_TRANSFER_PRE_AUTH_RATE_PER_SEC: f64 = 10.0 / 60.0;
/// Burst of the per-IP pre-auth bucket (attempts).
pub const WEB_TRANSFER_PRE_AUTH_BURST: f64 = 20.0;
/// Upgrades that may sit in the WebSocket handshake at the same time.
///
/// The handshake is the one stretch of an inbound connection that is paid for
/// before anything about the caller is known: it has passed the Origin and
/// subprotocol check and nothing else, so it is not yet a peer, not yet a
/// room member and not counted by any of the caps below. Without a bound, a
/// caller that opens sockets and then falls silent buys unbounded server-side
/// task and buffer state for free.
pub const WEB_TRANSFER_PENDING_HANDSHAKES: usize = 256;
/// Deadline for ONE WebSocket upgrade, from the accepted socket to the
/// completed handshake. A caller that takes longer is not distinguishable
/// from one that will never finish, and the slot it holds is the scarce
/// thing.
pub const WEB_TRANSFER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on tracked source IPs; further unseen IPs share one overflow
/// bucket so the map itself cannot grow.
pub const WEB_TRANSFER_PRE_AUTH_MAX_IPS: usize = 8192;
/// Idle TTL of one tracked source IP.
pub const WEB_TRANSFER_PRE_AUTH_IP_TTL: Duration = Duration::from_secs(10 * 60);
/// Bound on cached `(peer,requestId)` terminal responses (FIFO eviction).
pub const WEB_TRANSFER_REQUEST_CACHE_CAP: usize = 256;
/// TTL of one cached terminal response.
pub const WEB_TRANSFER_REQUEST_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// Peer heartbeat cadence: browsers send `ping` every 20 s; the server
/// answers `pong` and reaps sessions quiet for `WEB_TRANSFER_CTRL_TIMEOUT`
/// (60 s) on its tick. The server transmits nothing on a timer.
pub const WEB_TRANSFER_PEER_PING: Duration = Duration::from_secs(20);

/// Default display name from a peer ID: `Peer <last-4-hex>`.
pub fn default_display_name(peer_id: PeerId) -> String {
    let hex = peer_id.to_string();
    format!("Peer {}", &hex[hex.len() - 4..])
}

/// Normalizes a candidate display name: NFC, trimmed, no control characters,
/// 1..=48 Unicode scalar values. Rejects anything else as `INVALID_MESSAGE`.
pub fn normalize_display_name(raw: &str) -> Result<String, WebTransferError> {
    use unicode_normalization::UnicodeNormalization;
    let normalized: String = raw.nfc().collect();
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return Err(WebTransferError::invalid("display name must not be empty"));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(WebTransferError::invalid(
            "display name must not carry control characters",
        ));
    }
    if trimmed.chars().count() > WEB_TRANSFER_MAX_DISPLAY_NAME_CHARS {
        return Err(WebTransferError::invalid(
            "display name exceeds 48 characters",
        ));
    }
    Ok(trimmed.to_string())
}

/// Fixed token bucket with an explicit clock (deterministic under test).
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Sustained refill rate (tokens/second).
    rate_per_sec: f64,
    /// Maximum held tokens.
    burst: f64,
    /// Currently held tokens.
    tokens: f64,
    /// Last refill instant.
    last: Instant,
}

impl TokenBucket {
    /// Builds a full bucket.
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self {
            rate_per_sec,
            burst,
            tokens: burst,
            last: Instant::now(),
        }
    }

    /// Builds a full bucket with an explicit clock (tests only).
    #[cfg(test)]
    pub(crate) fn new_at(rate_per_sec: f64, burst: f64, now: Instant) -> Self {
        Self {
            rate_per_sec,
            burst,
            tokens: burst,
            last: now,
        }
    }

    /// Takes one token when available; refills by elapsed time first.
    pub fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.burst);
            self.last = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Takes `n` tokens for a byte-sized forward; returns how long the
    /// caller must wait first (`ZERO` when covered). Debt is bounded by one
    /// burst so a huge frame cannot mortgage the far future.
    pub fn take_bytes(&mut self, now: Instant, n: u64) -> Duration {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.burst);
            self.last = now;
        }
        self.tokens -= n as f64;
        if self.tokens >= 0.0 {
            return Duration::ZERO;
        }
        if self.tokens < -self.burst {
            self.tokens = -self.burst;
        }
        Duration::from_secs_f64((-self.tokens) / self.rate_per_sec)
    }

    /// Burst ceiling for tests.
    #[cfg(test)]
    pub fn burst_for_test(&self) -> f64 {
        self.burst
    }
}

/// Bounded per-IP pre-auth limiter: exactly `WEB_TRANSFER_PRE_AUTH_MAX_IPS`
/// tracked IPs with idle TTL; unseen IPs past the cap share one overflow
/// bucket instead of growing the map.
#[derive(Debug)]
pub struct PreAuthLimiter {
    /// Per-IP buckets plus last-seen instant, in insertion order.
    entries: VecDeque<(IpAddr, TokenBucket, Instant)>,
    /// Shared bucket for unseen IPs while at capacity.
    overflow: TokenBucket,
    /// Idle TTL (field so tests run fast with a short TTL).
    ttl: Duration,
}

impl Default for PreAuthLimiter {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            overflow: TokenBucket::new(
                WEB_TRANSFER_PRE_AUTH_RATE_PER_SEC,
                WEB_TRANSFER_PRE_AUTH_BURST,
            ),
            ttl: WEB_TRANSFER_PRE_AUTH_IP_TTL,
        }
    }
}

impl PreAuthLimiter {
    /// Builds a limiter with a custom idle TTL (tests only).
    #[cfg(test)]
    pub(crate) fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            overflow: TokenBucket::new(
                WEB_TRANSFER_PRE_AUTH_RATE_PER_SEC,
                WEB_TRANSFER_PRE_AUTH_BURST,
            ),
            ttl,
        }
    }

    /// Number of tracked IPs (tests only).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Consumes one attempt for `ip`; `false` means close with `4008`.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> bool {
        // Refresh or drop the caller's own entry first (idle TTL).
        let mut found = None;
        for (index, (addr, _, seen)) in self.entries.iter().enumerate() {
            if *addr == ip {
                found = Some(index);
                if now.saturating_duration_since(*seen) >= self.ttl {
                    self.entries.remove(index);
                    found = None;
                }
                break;
            }
        }
        if let Some(index) = found {
            let (_, bucket, seen) = self.entries.get_mut(index).expect("pre-auth entry present");
            *seen = now;
            return bucket.take(now);
        }
        // Unseen IP: purge idle heads, then insert while under the cap.
        // At capacity the attempt shares the single overflow bucket instead
        // of growing the map, so an IP scan cannot evict tracked peers.
        while let Some((_, _, seen)) = self.entries.front() {
            if now.saturating_duration_since(*seen) >= self.ttl {
                self.entries.pop_front();
            } else {
                break;
            }
        }
        if self.entries.len() >= WEB_TRANSFER_PRE_AUTH_MAX_IPS {
            return self.overflow.take(now);
        }
        let mut bucket = TokenBucket {
            rate_per_sec: WEB_TRANSFER_PRE_AUTH_RATE_PER_SEC,
            burst: WEB_TRANSFER_PRE_AUTH_BURST,
            tokens: WEB_TRANSFER_PRE_AUTH_BURST,
            last: now,
        };
        let ok = bucket.take(now);
        self.entries.push_back((ip, bucket, now));
        ok
    }
}

/// Bounded per-IP counter of pre-authentication failures, reported
/// LOGARITHMICALLY: the 1st, 2nd, 4th, 8th … failure from one address.
///
/// Same rule as the SSH gateway's repeated username mismatches, and for the
/// same reason: a line per refusal turns a scanner into a log-volume attack
/// on the operator, while silence hides the one case a human must see — a
/// single address failing thousands of times. Powers of two report the
/// ORDER OF MAGNITUDE, which is what the operator actually acts on, at a cost
/// that grows as log(n).
#[derive(Debug)]
pub struct AuthFailureSampler {
    /// `(ip, failures, last seen)`, oldest first; linear scan at 8192 is
    /// cheaper than a second index, exactly as in the limiter above.
    entries: VecDeque<(IpAddr, u64, Instant)>,
    /// Idle TTL (field so tests run fast with a short TTL).
    ttl: Duration,
}

impl Default for AuthFailureSampler {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            ttl: WEB_TRANSFER_PRE_AUTH_IP_TTL,
        }
    }
}

impl AuthFailureSampler {
    /// Records one failure for `ip`; returns the running count when this
    /// failure is one the caller should LOG, and `None` when it is one of the
    /// many between two powers of two.
    ///
    /// At capacity an unseen IP is counted as a FIRST failure and not
    /// tracked: reporting it once is the useful half, and evicting a tracked
    /// address to make room would let an IP scan silence the sampler.
    pub fn note(&mut self, ip: IpAddr, now: Instant) -> Option<u64> {
        let mut found = None;
        for (index, (addr, _, seen)) in self.entries.iter().enumerate() {
            if *addr == ip {
                found = Some(index);
                if now.saturating_duration_since(*seen) >= self.ttl {
                    self.entries.remove(index);
                    found = None;
                }
                break;
            }
        }
        if let Some(index) = found {
            let (_, count, seen) = self.entries.get_mut(index).expect("auth entry present");
            *count = count.saturating_add(1);
            *seen = now;
            let count = *count;
            return count.is_power_of_two().then_some(count);
        }
        while let Some((_, _, seen)) = self.entries.front() {
            if now.saturating_duration_since(*seen) >= self.ttl {
                self.entries.pop_front();
            } else {
                break;
            }
        }
        if self.entries.len() < WEB_TRANSFER_PRE_AUTH_MAX_IPS {
            self.entries.push_back((ip, 1, now));
        }
        Some(1)
    }

    /// Number of tracked IPs (tests only).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Builds a sampler with a custom idle TTL (tests only).
    #[cfg(test)]
    pub(crate) fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            ttl,
        }
    }
}

/// Bounded FIFO of terminal `(requestId → response)` pairs with TTL: an
/// identical `(peer,requestId)` replay returns the cached response instead of
/// re-executing the mutation.
#[derive(Debug, Default)]
pub struct RequestCache {
    /// Oldest first; linear scan is fine at 256 entries.
    entries: VecDeque<(crate::web_transfer_protocol::RequestId, String, Instant)>,
}

impl RequestCache {
    /// Replays the cached terminal response when present and fresh.
    pub fn get(
        &mut self,
        id: crate::web_transfer_protocol::RequestId,
        now: Instant,
    ) -> Option<String> {
        self.entries.retain(|(_, _, at)| {
            now.saturating_duration_since(*at) < WEB_TRANSFER_REQUEST_CACHE_TTL
        });
        self.entries
            .iter()
            .find(|(cached, _, _)| *cached == id)
            .map(|(_, response, _)| response.clone())
    }

    /// Stores one terminal response, evicting oldest-first past the cap.
    /// Re-inserting an ID replaces its response without growing.
    pub fn insert(
        &mut self,
        id: crate::web_transfer_protocol::RequestId,
        response: String,
        now: Instant,
    ) {
        self.entries.retain(|(_, _, at)| {
            now.saturating_duration_since(*at) < WEB_TRANSFER_REQUEST_CACHE_TTL
        });
        self.entries.retain(|(cached, _, _)| *cached != id);
        while self.entries.len() >= WEB_TRANSFER_REQUEST_CACHE_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back((id, response, now));
    }

    /// Cached entry count (tests only).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// One authenticated control session: the synchronous state the single
/// per-peer actor task drives. Dropping the held `PeerGuard` removes exactly
/// this peer from exactly its room and releases its permits.
pub struct PeerSession {
    /// Authenticated member.
    peer_id: PeerId,
    /// Last resolved display name (default, hello-provided or renamed).
    display_name: String,
    /// Membership guard: drop cleans up exactly this peer in this room.
    guard: PeerGuard,
    /// 30/s burst-60 bucket over every inbound control message.
    control: TokenBucket,
    /// 4/s burst-8 bucket over mutations (`peer.rename` today).
    mutation: TokenBucket,
    /// Last inbound control activity (reaper tick-checked, never
    /// `timeout(recv)`).
    last_recv: Instant,
    /// Room revision covered by the last snapshot/event sent.
    revision_seen: u64,
    /// Bounded duplicate-`(peer,requestId)` terminal responses.
    cache: RequestCache,
    /// Bounded outgoing control queue (capacity 64).
    out_tx: mpsc::Sender<String>,
    /// Room broadcast receiver (capacity 256; lag resynchronizes).
    events: broadcast::Receiver<RoomEvent>,
}

impl PeerSession {
    /// Authenticated member ID.
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Last resolved display name.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Room this session belongs to.
    pub fn room(&self) -> &Arc<WebTransferRoom> {
        self.guard.room()
    }

    /// Updates the resolved display name after a rename.
    pub(crate) fn set_display_name(&mut self, name: String) {
        self.display_name = name;
    }

    /// Records inbound activity.
    pub fn touch(&mut self, now: Instant) {
        self.last_recv = now;
    }

    /// Last inbound activity (reaper input).
    pub fn last_recv(&self) -> Instant {
        self.last_recv
    }

    /// Consumes one control token.
    pub fn take_control(&mut self, now: Instant) -> bool {
        self.control.take(now)
    }

    /// Consumes one mutation token.
    pub fn take_mutation(&mut self, now: Instant) -> bool {
        self.mutation.take(now)
    }

    /// Replays a cached terminal response when the `(peer,requestId)` pair
    /// already completed.
    pub fn replay(
        &mut self,
        id: crate::web_transfer_protocol::RequestId,
        now: Instant,
    ) -> Option<String> {
        self.cache.get(id, now)
    }

    /// Caches one terminal response.
    pub fn remember(
        &mut self,
        id: crate::web_transfer_protocol::RequestId,
        response: String,
        now: Instant,
    ) {
        self.cache.insert(id, response, now);
    }

    /// Revision covered so far (resync bookkeeping).
    pub fn revision_seen(&self) -> u64 {
        self.revision_seen
    }

    /// Advances the covered revision.
    pub fn set_revision_seen(&mut self, revision: u64) {
        self.revision_seen = revision;
    }

    /// Outgoing queue sender (bounded sends close slow peers).
    pub fn sender(&self) -> mpsc::Sender<String> {
        self.out_tx.clone()
    }

    /// Room broadcast receiver.
    pub fn events_mut(&mut self) -> &mut broadcast::Receiver<RoomEvent> {
        &mut self.events
    }
}

/// Sorted snapshot contents under a short lock: current revision plus every
/// peer by ascending ID. Callers serialize one message per entry (never one
/// large aggregate) without holding the lock.
#[allow(clippy::type_complexity)]
pub fn snapshot_parts(
    room: &Arc<WebTransferRoom>,
) -> Result<(u64, Vec<(PeerId, Option<String>)>), WebTransferError> {
    let state = room
        .state
        .lock()
        .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
    let mut peers: Vec<(PeerId, Option<String>)> = state
        .peers
        .iter()
        .map(|(id, record)| (*id, record.display_name.clone()))
        .collect();
    peers.sort_by_key(|(id, _)| id.to_string());
    Ok((state.revision, peers))
}

/// Pre-authentication verdict for one `hello`: the room/token pair either
/// authenticates, is denied uniformly, or names an expired room the token
/// holder may be told about. Absent room, bad token and exhausted caps are
/// all `Deny` (identical response, identical delay — no oracle); `Gone`
/// requires the valid token, so it reveals nothing new.
#[derive(Debug)]
pub(crate) enum HelloAuth {
    /// Token matches a live room; the caller proceeds to ID generation.
    Ok {
        /// Room the token opened.
        room: Arc<WebTransferRoom>,
    },
    /// Deny with `4001` after the uniform delay.
    Deny,
    /// Valid token, destroyed room: deny with `4004`.
    Gone,
}

/// Checks room existence, token and liveness without allocating a session.
/// Never logs or returns the token.
pub(crate) fn authenticate_hello(
    registry: &WebTransferRegistry,
    room_id: RoomId,
    token: &MemberToken,
) -> HelloAuth {
    let Some(room) = registry.room(room_id) else {
        return HelloAuth::Deny;
    };
    let digest_ok = match room.state.lock() {
        Ok(state) => token_digests_equal(&token.sha256_hash(), &state.member_hash),
        Err(_) => return HelloAuth::Deny,
    };
    if !digest_ok {
        return HelloAuth::Deny;
    }
    if room.is_destroyed() {
        return HelloAuth::Gone;
    }
    HelloAuth::Ok { room }
}

/// Generates a nonzero random peer ID from the OS CSPRNG.
pub(crate) fn generate_peer_id() -> PeerId {
    use ring::rand::{SecureRandom, SystemRandom};
    let random = SystemRandom::new();
    let mut bytes = [0u8; 16];
    random.fill(&mut bytes).expect("OS CSPRNG");
    if bytes == [0u8; 16] {
        bytes[15] = 1;
    }
    PeerId::from_bytes(bytes)
}

/// Whether a control session is dead: no inbound activity for the liveness
/// window. Checked on the reaper tick, never via `timeout(recv)`.
pub fn control_liveness_expired(last_recv: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_recv) >= WEB_TRANSFER_CTRL_TIMEOUT
}

impl PeerSession {
    /// Establishes an authenticated session: joins the peer (normalizing the
    /// name, allocating permits), subscribes to room events and builds the
    /// initial messages (`welcome` + one-per-peer snapshot). The caller writes
    /// `initial` to the socket, then drives the session with `out_rx`; dropping
    /// the session removes exactly this peer and releases its permits.
    pub(crate) fn establish(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        peer_id: PeerId,
        display_name: Option<String>,
    ) -> Result<(Self, mpsc::Receiver<String>, Vec<String>), WebTransferError> {
        let guard = registry.join_peer(room, peer_id, display_name)?;
        let (revision, peers) = snapshot_parts(room)?;
        let offers = offer_views(room)?;
        let name = peers
            .iter()
            .find(|(id, _)| *id == peer_id)
            .and_then(|(_, name)| name.clone())
            .unwrap_or_else(|| default_display_name(peer_id));
        let config = registry.config();
        let mut initial = Vec::with_capacity(peers.len() + offers.len() + 3);
        initial.push(crate::web_transfer_protocol::welcome_envelope(
            peer_id,
            room.id,
            &name,
            &config.limits,
            &config.ice.servers,
        ));
        initial.extend(snapshot_offer_strings(revision, &peers, &offers)?);
        let now = Instant::now();
        let (out_tx, out_rx) = mpsc::channel(WEB_TRANSFER_OUTGOING_CAP);
        if let Ok(mut sessions) = room.sessions.lock() {
            sessions.insert(peer_id, out_tx.clone());
        }
        let session = Self {
            peer_id,
            display_name: name,
            guard,
            control: TokenBucket::new(
                WEB_TRANSFER_CONTROL_RATE_PER_SEC,
                WEB_TRANSFER_CONTROL_BURST,
            ),
            mutation: TokenBucket::new(
                WEB_TRANSFER_MUTATION_RATE_PER_SEC,
                WEB_TRANSFER_MUTATION_BURST,
            ),
            last_recv: now,
            revision_seen: revision,
            cache: RequestCache::default(),
            out_tx,
            events: room.events.subscribe(),
        };
        Ok((session, out_rx, initial))
    }
}

/// Rebuilds the full snapshot after a lagged broadcast receiver: revision
/// plus one message per peer plus one per offer. The actor queues these
/// instead of the missed incremental events (never a larger aggregate).
pub(crate) fn build_resync(room: &Arc<WebTransferRoom>) -> Result<Vec<String>, WebTransferError> {
    let (revision, peers) = snapshot_parts(room)?;
    let offers = offer_views(room)?;
    snapshot_offer_strings(revision, &peers, &offers)
}

/// Serializes one snapshot from parts: begin, peers, offers, end. Stored
/// manifests are canonical bytes; a record that fails to parse (impossible
/// for validated stores) aborts the snapshot rather than emitting garbage.
pub(crate) fn snapshot_offer_strings(
    revision: u64,
    peers: &[(PeerId, Option<String>)],
    offers: &[OfferView],
) -> Result<Vec<String>, WebTransferError> {
    use crate::web_transfer_protocol::SnapshotOffer;
    let mut parsed: Vec<(PeerId, OfferId, serde_json::Value, String)> =
        Vec::with_capacity(offers.len());
    for view in offers {
        let manifest: serde_json::Value = serde_json::from_slice(&view.manifest)
            .map_err(|_| WebTransferError::internal("stored manifest is not JSON"))?;
        parsed.push((view.peer, view.offer, manifest, hex::encode(view.mac)));
    }
    let refs: Vec<SnapshotOffer> = parsed
        .iter()
        .map(|(peer, offer, manifest, mac_hex)| SnapshotOffer {
            peer: *peer,
            offer: *offer,
            manifest,
            mac_hex,
        })
        .collect();
    Ok(crate::web_transfer_protocol::snapshot_messages(
        revision, peers, &refs,
    ))
}

// --- Phase 3.1: relay transfer state machine ----------------------------------
// Every transition verifies room, participants, offer and current attempt
// under a single short lock, then delivers notices after unlocking (never
// awaiting while holding `RoomState`). Outbound delivery is best-effort
// `try_send` into each peer's bounded session queue: a dead peer's own
// machinery reaps it, and the transition already committed.

/// Outcome of [`WebTransferRegistry::request_transfer`]: both ack
/// identically with the transfer ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// Validated, recorded, `transfer.incoming` sent to the source.
    Created,
    /// A live transfer already covers this selection; same ID re-acked.
    Existing,
}

/// Outcome of [`WebTransferRegistry::source_ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyOutcome {
    /// Direct attempt opened: both peers hold `transfer.direct_start` and
    /// the caller arms the deadline. No relay permit is held.
    Negotiating,
    /// Relay slot held; both tickets issued, queued for post-ack delivery.
    Admitted,
    /// No slot right now; the actor spawns the 30 s admission waiter.
    Queued,
    /// Nothing to do: the message named an attempt that is no longer
    /// current, or a transfer that already left the direct path.
    Ignored,
}

/// Which forwarded signaling step a message is, for the one validator the
/// three of them share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalKind {
    /// `rtc.offer`, recipient only, once.
    Offer,
    /// `rtc.answer`, source only, once, after the offer.
    Answer,
    /// `rtc.ice`, either side, 128 per side.
    Ice,
}

/// Outcome of [`WebTransferRegistry::cancel_transfer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Moved to `Cancelled`; the notice is queued for post-ack delivery.
    Cancelled,
    /// Already terminal; acked without resending or double-releasing.
    AlreadyTerminal,
}

/// Ordered peer notices a transfer transition produced. The registry never
/// sends these itself: the HTTP actor replies the request ack FIRST and then
/// drains the outbox, so a peer always sees its own ack before any event the
/// same request caused (same mpsc queue, FIFO). Best-effort, in order.
pub(crate) type TransferOutbox = Vec<(PeerId, String)>;

/// What [`WebTransferRegistry::fallback_to_relay`] produces: the admission
/// outcome for the FRESH relay attempt, the notices to deliver after the ack,
/// and the verified ranges that attempt carries (the counterpart's
/// `transfer.direct_failed` quotes them, so they are returned once rather
/// than read back out of the record twice).
pub(crate) type FallbackOutcome = (ReadyOutcome, TransferOutbox, Vec<(u64, u64)>);

/// Drains a [`TransferOutbox`] in order. Delivery failures are dropped: the
/// transition already committed and the peer's own slow-path machinery (reap
/// on next tick, resync on reconnect) converges it.
pub(crate) fn drain_transfer_outbox(room: &WebTransferRoom, outbox: TransferOutbox) {
    for (peer, message) in outbox {
        let _ = room.send_to(peer, message);
    }
}

/// Next attempt number with checked increment (`u64::MAX` is unrepresentable
/// and fails loudly instead of wrapping into a reused number).
pub(crate) fn next_attempt_number(current: u64) -> Result<u64, WebTransferError> {
    current
        .checked_add(1)
        .ok_or_else(|| WebTransferError::internal("attempt number exhausted"))
}

fn random_transfer_16() -> [u8; 16] {
    use ring::rand::{SecureRandom, SystemRandom};
    let random = SystemRandom::new();
    let mut bytes = [0u8; 16];
    random.fill(&mut bytes).expect("OS CSPRNG");
    if bytes == [0u8; 16] {
        bytes[15] = 1;
    }
    bytes
}

fn generate_transfer_id() -> TransferId {
    TransferId::from_bytes(random_transfer_16())
}

fn generate_attempt_id() -> AttemptId {
    AttemptId::from_bytes(random_transfer_16())
}

fn generate_relay_ticket() -> RelayTicket {
    RelayTicket::from_bytes(random_transfer_16())
}

/// Live transfers involving `peer` (either side).
fn live_count_for(state: &RoomState, peer: PeerId) -> usize {
    state
        .transfers
        .values()
        .filter(|record| {
            record.state.is_live() && (record.source == peer || record.recipient == peer)
        })
        .count()
}

/// Moves a live transfer to a terminal state, releasing its relay permit
/// exactly once (taken out of the attempt before drop) and dropping its
/// tickets. Returns false when the record is missing or already terminal —
/// callers ack idempotently without resending. Permits release HERE, never
/// at GC: the retained terminal record holds no resources.
fn terminate_locked(
    inner: Option<&RegistryInner>,
    state: &mut RoomState,
    id: TransferId,
    terminal: TransferState,
) -> bool {
    debug_assert!(matches!(
        terminal,
        TransferState::Completed | TransferState::Cancelled | TransferState::Failed
    ));
    let Some(record) = state.transfers.get_mut(&id) else {
        return false;
    };
    if !record.state.is_live() {
        return false;
    }
    record.state = terminal;
    record.terminated_at = Some(Instant::now());
    // The permit drops with the attempt; the pump/parked waiter selected on
    // `cancel` exits quietly (whoever terminates already notified).
    record.attempt.take();
    record.cancel.cancel();
    if let Some(inner) = inner {
        inner.tickets.retain(|_, ticket| ticket.transfer_id != id);
        inner.transfers_current.fetch_sub(1, Ordering::Relaxed);
        // One funnel for every terminal transition, so the totals cannot
        // disagree with the states the room actually reached. `Failed` has
        // no total of its own: a failure is not a refusal and the plan's
        // metric set names only these two.
        match terminal {
            TransferState::Completed => {
                inner.completed_total.fetch_add(1, Ordering::Relaxed);
            }
            TransferState::Cancelled => {
                inner.cancelled_total.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    gc_terminals_locked(state, Instant::now());
    true
}

/// Drops terminal records older than the retention window (FIFO by
/// termination order). Runs inside every mutating transition.
fn gc_terminals_locked(state: &mut RoomState, now: Instant) {
    state.transfers.retain(|_, record| {
        record.state.is_live()
            || record.terminated_at.is_none_or(|at| {
                now.saturating_duration_since(at) < WEB_TRANSFER_TERMINAL_RETENTION
            })
    });
}

/// Cancels every live transfer matching `matches` as `by_peer`, notifying
/// the other party of each. Shared by offer withdraw, peer drop and room
/// teardown paths, each under its own single lock section.
fn cancel_where_locked(
    inner: Option<&RegistryInner>,
    state: &mut RoomState,
    by_peer: PeerId,
    mut matches: impl FnMut(&TransferRecord) -> bool,
    outbox: &mut Vec<(PeerId, String)>,
) {
    let ids: Vec<TransferId> = state
        .transfers
        .iter()
        .filter(|(_, record)| record.state.is_live() && matches(record))
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        let other = state.transfers.get(&id).map(|record| {
            if record.source == by_peer {
                record.recipient
            } else {
                record.source
            }
        });
        if terminate_locked(inner, state, id, TransferState::Cancelled) {
            if let Some(other) = other {
                if other != by_peer {
                    outbox.push((
                        other,
                        crate::web_transfer_protocol::transfer_cancelled_envelope(id, by_peer),
                    ));
                }
            }
        }
    }
}

impl WebTransferRegistry {
    /// Requests a transfer: validates recipient, offer, live source,
    /// single-file raw selection, digest, caps and resume bounds, then
    /// records `Requested` and sends `transfer.incoming` to the source —
    /// all checks and the insert under one lock, delivery after unlock. A
    /// live transfer for the same parties and selection re-acks instead of
    /// duplicating; a permit-less `WaitingRelay` match upgrades to a fresh
    /// attempt (relay-busy retry) and re-enters `WaitingSource`.
    #[allow(clippy::too_many_arguments)]
    pub fn request_transfer(
        &self,
        room: &Arc<WebTransferRoom>,
        recipient: PeerId,
        offer_id: OfferId,
        entry_ids: Vec<String>,
        selection_digest: [u8; 32],
        mode: &str,
        resume: Option<crate::web_transfer_protocol::ResumeDescriptorBody>,
    ) -> Result<(TransferId, RequestOutcome), WebTransferError> {
        use crate::web_transfer_protocol::selection_digest as compute_digest;
        struct Checked {
            source: PeerId,
            attempt_id: AttemptId,
            upgraded: bool,
            id: TransferId,
        }
        let checked = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            if !state.peers.contains_key(&recipient) {
                return Err(WebTransferError::invalid("unknown peer"));
            }
            let offer = state
                .offers
                .get(&offer_id)
                .ok_or_else(|| WebTransferError::offer_not_found("unknown offer"))?;
            let source = offer.owner;
            // Source must be joined AND live: the sessions map mirrors joins
            // (both mutations are synchronous with no await between), so a
            // joined peer without a session is already half-gone.
            let source_live = state.peers.contains_key(&source)
                && room
                    .sessions
                    .lock()
                    .map(|sessions| sessions.contains_key(&source))
                    .unwrap_or(false);
            if !source_live {
                return Err(WebTransferError::source_offline("source offline"));
            }
            let manifest_value: serde_json::Value = serde_json::from_slice(&offer.manifest)
                .map_err(|_| WebTransferError::internal("stored manifest is not JSON"))?;
            let manifest =
                crate::web_transfer_protocol::parse_manifest(&manifest_value, &room.limits)
                    .map_err(|_| WebTransferError::internal("stored manifest failed validation"))?;
            // The selection rule is pure and lives beside the manifest it is
            // checked against. An archive's root and length are DYNAMIC —
            // they are in no manifest, because the archive does not exist
            // until the source generates it — so the record carries `None`
            // for both and the expectation moves to the recipient, which
            // authenticates them from the sealed FINAL frame (5.2).
            let entry_number =
                match crate::web_transfer_protocol::validate_selection(mode, &entry_ids, &manifest)
                    .map_err(|e| WebTransferError::invalid(e.to_string()))?
                {
                    crate::web_transfer_protocol::Selection::Raw(id) => Some(id),
                    crate::web_transfer_protocol::Selection::Zip => None,
                };
            // For `raw` the manifest holds the entry's root and size, and
            // the server checks the completion against them. For `zip` it
            // holds NEITHER — the archive does not exist until the source
            // generates it — so the server carries no expectation and the
            // recipient authenticates the dynamic tuple from the sealed
            // FINAL frame instead. That is the whole difference between the
            // two modes on this side.
            let entry = match entry_number {
                Some(id) => Some(
                    manifest
                        .entries
                        .iter()
                        .find(|entry| entry.id == id)
                        .ok_or_else(|| WebTransferError::internal("validated entry ID vanished"))?,
                ),
                None => None,
            };
            let entry_root =
                match entry {
                    Some(entry) => Some(entry.root.ok_or_else(|| {
                        WebTransferError::internal("validated entry has no root")
                    })?),
                    None => None,
                };
            let entry_size = entry.map(|entry| entry.size);
            let transfer_mode = if entry_number.is_some() {
                TransferMode::Raw
            } else {
                TransferMode::Zip
            };
            let entry_number =
                entry_number.unwrap_or(crate::web_transfer_protocol::RESERVED_ZIP_ENTRY_ID);
            let expected = compute_digest(&offer_id, &offer.mac, &entry_ids, mode);
            if !token_digests_equal(&expected, &selection_digest) {
                return Err(WebTransferError::source_changed(
                    "selection does not match the offer",
                ));
            }
            let resume = match resume {
                None => None,
                Some(descriptor) => {
                    // Only a `raw` selection has manifest bounds to check
                    // against; an archive's are not knowable here, and the
                    // recipient refuses a tuple that differs from the one it
                    // stored on the first attempt.
                    if let Some(entry) = entry {
                        for (_start, end) in &descriptor.verified_ranges {
                            if *end > entry.chunks.len() as u64 {
                                return Err(WebTransferError::invalid(
                                    "resume range exceeds chunk count",
                                ));
                            }
                        }
                        if descriptor.output_length > entry.size {
                            return Err(WebTransferError::invalid(
                                "resume length exceeds entry size",
                            ));
                        }
                    }
                    Some(ResumeDescriptor {
                        verified_ranges: descriptor.verified_ranges,
                        output_length: descriptor.output_length,
                    })
                }
            };
            if live_count_for(&state, source) >= room.limits.max_transfers_per_peer as usize
                || live_count_for(&state, recipient) >= room.limits.max_transfers_per_peer as usize
            {
                return Err(self.refused(WebTransferError::limit("peer transfer budget exhausted")));
            }
            // Same parties + same selection + live: no duplicate. A
            // permit-less WaitingRelay match is a busy retry and upgrades to
            // a fresh attempt; anything else re-acks the same ID.
            if let Some((id, upgrade)) = state.transfers.iter().find_map(|(id, record)| {
                (record.state.is_live()
                    && record.offer_id == offer_id
                    && record.source == source
                    && record.recipient == recipient
                    && record.entry_id == entry_number
                    && record.mode == transfer_mode)
                    .then(|| {
                        (
                            *id,
                            record.state == TransferState::WaitingRelay
                                && record
                                    .attempt
                                    .as_ref()
                                    .map(|attempt| attempt.relay_permit.is_none())
                                    .unwrap_or(false),
                        )
                    })
            }) {
                if !upgrade {
                    return Ok((id, RequestOutcome::Existing));
                }
                let number = next_attempt_number(
                    state
                        .transfers
                        .get(&id)
                        .map(|record| record.attempt_number)
                        .unwrap_or(0),
                )?;
                let attempt_id = generate_attempt_id();
                if let Some(record) = state.transfers.get_mut(&id) {
                    record.attempt_number = number;
                    record.attempt_id = Some(attempt_id);
                    record.attempt = Some(AttemptState {
                        attempt_id,
                        attempt_number: number,
                        relay_permit: None,
                    });
                    record.state = TransferState::WaitingSource;
                }
                Checked {
                    source,
                    attempt_id,
                    upgraded: true,
                    id,
                }
            } else {
                let mut new_id = generate_transfer_id();
                for _ in 0..8 {
                    if !state.transfers.contains_key(&new_id) {
                        break;
                    }
                    new_id = generate_transfer_id();
                }
                if state.transfers.contains_key(&new_id) {
                    return Err(WebTransferError::internal(
                        "transfer ID collision exhaustion",
                    ));
                }
                let attempt_id = generate_attempt_id();
                state.transfers.insert(
                    new_id,
                    TransferRecord {
                        transfer_id: new_id,
                        offer_id,
                        source,
                        recipient,
                        selection_digest,
                        entry_id: entry_number,
                        mode: transfer_mode,
                        resume,
                        state: TransferState::Requested,
                        attempt_number: 1,
                        attempt_id: Some(attempt_id),
                        attempt: Some(AttemptState {
                            attempt_id,
                            attempt_number: 1,
                            relay_permit: None,
                        }),
                        entry_root,
                        entry_size,
                        last_direct_attempt: None,
                        carried_attempt: None,
                        terminated_at: None,
                        cancel: CancellationToken::new(),
                    },
                );
                self.inner.transfers_current.fetch_add(1, Ordering::Relaxed);
                gc_terminals_locked(&mut state, Instant::now());
                Checked {
                    source,
                    attempt_id,
                    upgraded: false,
                    id: new_id,
                }
            }
        };
        // Deliver outside the lock; a source that vanished in the gap
        // cancels immediately instead of stranding a permit.
        if !room.send_to(
            checked.source,
            crate::web_transfer_protocol::transfer_incoming_envelope(
                checked.id,
                offer_id,
                recipient,
                checked.attempt_id,
                mode,
            ),
        ) {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            terminate_locked(
                room.registry.upgrade().as_deref(),
                &mut state,
                checked.id,
                TransferState::Cancelled,
            );
            return Err(WebTransferError::source_offline("source unreachable"));
        }
        // Requested → WaitingSource once the notice is on the wire. A retry
        // already sits in WaitingSource; a racing terminal wins instead.
        if let Ok(mut state) = room.state.lock() {
            if let Some(record) = state.transfers.get_mut(&checked.id) {
                if record.state == TransferState::Requested {
                    record.state = TransferState::WaitingSource;
                }
            }
        }
        Ok((
            checked.id,
            if checked.upgraded {
                RequestOutcome::Existing
            } else {
                RequestOutcome::Created
            },
        ))
    }
}

impl WebTransferRegistry {
    /// Source answers `transfer.incoming`: only the source, only in
    /// `WaitingSource`, only for the current attempt, only with the stored
    /// digest (freshness attestation over its live `File`).
    ///
    /// Phase 4 changed what success means: it opens the DIRECT attempt.
    /// The transfer moves to `NegotiatingDirect`, both peers receive
    /// `transfer.direct_start` with their fixed SDP role, and the caller
    /// arms the 10 s deadline. **No relay permit is taken here** — the relay
    /// is the fallback, and a direct transfer that never needs a slot must
    /// never hold one while it negotiates.
    ///
    /// The notices are RETURNED, not sent: the HTTP actor acks first, then
    /// drains the outbox (same-peer ack-before-event order).
    pub fn source_ready(
        &self,
        room: &Arc<WebTransferRoom>,
        source: PeerId,
        transfer_id: TransferId,
        attempt_id: AttemptId,
        digest: [u8; 32],
    ) -> Result<(ReadyOutcome, TransferOutbox), WebTransferError> {
        use crate::web_transfer_protocol::transfer_direct_start_envelope;
        let (recipient, attempt_number) = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            let record = state
                .transfers
                .get(&transfer_id)
                .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
            if record.source != source {
                if record.recipient == source {
                    return Err(WebTransferError::invalid("only the source readies"));
                }
                return Err(WebTransferError::not_participant(
                    "stranger to this transfer",
                ));
            }
            if record.state != TransferState::WaitingSource {
                return Err(WebTransferError::invalid(
                    "transfer is not awaiting its source",
                ));
            }
            if record.attempt_id != Some(attempt_id) {
                return Err(WebTransferError::invalid("stale attempt"));
            }
            if !token_digests_equal(&record.selection_digest, &digest) {
                return Err(WebTransferError::source_changed("selection does not match"));
            }
            let recipient = record.recipient;
            let attempt_number = record.attempt_number;
            if let Some(record) = state.transfers.get_mut(&transfer_id) {
                record.state = TransferState::NegotiatingDirect {
                    started_at: Instant::now(),
                    ready_source: false,
                    ready_recipient: false,
                    candidates_source: 0,
                    candidates_recipient: 0,
                    offer_seen: false,
                    answer_seen: false,
                };
            }
            (recipient, attempt_number)
        };
        let ice = &self.inner.config.ice.servers;
        let deadline_ms = u64::try_from(self.direct_deadline().as_millis()).unwrap_or(u64::MAX);
        // The recipient acts first (it creates the one DataChannel and the
        // offer), so it hears first. The source's own notice is queued into
        // its session before any forwarded signaling can be: the recipient
        // cannot answer a message it has not yet received.
        let outbox = vec![
            (
                recipient,
                transfer_direct_start_envelope(
                    transfer_id,
                    attempt_id,
                    attempt_number,
                    DirectRole::Offerer.as_str(),
                    ice,
                    deadline_ms,
                ),
            ),
            (
                source,
                transfer_direct_start_envelope(
                    transfer_id,
                    attempt_id,
                    attempt_number,
                    DirectRole::Answerer.as_str(),
                    ice,
                    deadline_ms,
                ),
            ),
        ];
        Ok((ReadyOutcome::Negotiating, outbox))
    }

    /// The direct deadline in force (10 s; tests shorten it).
    pub fn direct_deadline(&self) -> Duration {
        self.inner
            .direct_deadline
            .lock()
            .map(|slot| *slot)
            .unwrap_or(WEB_TRANSFER_DIRECT_DEADLINE)
    }

    /// Test seam: shortens the direct deadline so a fallback is observable
    /// without sleeping ten seconds.
    pub fn set_direct_deadline(&self, deadline: Duration) {
        if let Ok(mut slot) = self.inner.direct_deadline.lock() {
            *slot = deadline;
        }
    }

    /// Forwards one `rtc.offer`. Only the RECIPIENT may send it, only once,
    /// only while its own attempt negotiates. The SDP is neither parsed nor
    /// stored nor logged: it is copied into a fresh envelope for the source
    /// and dropped.
    pub fn forward_rtc_offer(
        &self,
        room: &Arc<WebTransferRoom>,
        from: PeerId,
        body: &crate::web_transfer_protocol::RtcSdpBody,
    ) -> Result<TransferOutbox, WebTransferError> {
        let counterpart = self.signal_slot(
            room,
            from,
            body.transfer_id,
            body.attempt_id,
            SignalKind::Offer,
        )?;
        Ok(vec![(
            counterpart,
            crate::web_transfer_protocol::rtc_sdp_envelope("rtc.offer", body),
        )])
    }

    /// Forwards one `rtc.answer`. Only the SOURCE may send it, only once and
    /// only after the offer it answers has been forwarded.
    pub fn forward_rtc_answer(
        &self,
        room: &Arc<WebTransferRoom>,
        from: PeerId,
        body: &crate::web_transfer_protocol::RtcSdpBody,
    ) -> Result<TransferOutbox, WebTransferError> {
        let counterpart = self.signal_slot(
            room,
            from,
            body.transfer_id,
            body.attempt_id,
            SignalKind::Answer,
        )?;
        Ok(vec![(
            counterpart,
            crate::web_transfer_protocol::rtc_sdp_envelope("rtc.answer", body),
        )])
    }

    /// Forwards one `rtc.ice`, from either side, up to 128 per side. The
    /// end-of-candidates marker rides the same budget and the same path; the
    /// candidate line itself is opaque here as everywhere else.
    pub fn forward_rtc_ice(
        &self,
        room: &Arc<WebTransferRoom>,
        from: PeerId,
        body: &crate::web_transfer_protocol::RtcIceBody,
    ) -> Result<TransferOutbox, WebTransferError> {
        let counterpart = self.signal_slot(
            room,
            from,
            body.transfer_id,
            body.attempt_id,
            SignalKind::Ice,
        )?;
        Ok(vec![(
            counterpart,
            crate::web_transfer_protocol::rtc_ice_envelope(body),
        )])
    }

    /// One participant declares its DataChannel usable. Valid only from a
    /// participant of the CURRENT attempt, only after that side's own
    /// signaling step, and idempotent. The second distinct ready commits the
    /// direct path: `transfer.path_commit {path:"direct"}` to the recipient
    /// first, then the source (the source may not read a byte before it).
    pub fn direct_ready(
        &self,
        room: &Arc<WebTransferRoom>,
        from: PeerId,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) -> Result<TransferOutbox, WebTransferError> {
        let committed = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            let record = state
                .transfers
                .get(&transfer_id)
                .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
            let is_source = record.source == from;
            if !is_source && record.recipient != from {
                return Err(WebTransferError::not_participant(
                    "stranger to this transfer",
                ));
            }
            if record.attempt_id != Some(attempt_id) {
                return Err(WebTransferError::invalid("stale attempt"));
            }
            let TransferState::NegotiatingDirect {
                started_at,
                mut ready_source,
                mut ready_recipient,
                candidates_source,
                candidates_recipient,
                offer_seen,
                answer_seen,
            } = record.state
            else {
                return Err(WebTransferError::invalid("transfer is not negotiating"));
            };
            // "After signaling" is per-side and exact: the offerer has had
            // its offer forwarded, the answerer its answer. A ready before
            // that describes a channel that cannot exist yet.
            if is_source {
                if !answer_seen {
                    return Err(WebTransferError::invalid("answer not sent yet"));
                }
                ready_source = true;
            } else {
                if !offer_seen {
                    return Err(WebTransferError::invalid("offer not sent yet"));
                }
                ready_recipient = true;
            }
            let both = ready_source && ready_recipient;
            let source = record.source;
            let recipient = record.recipient;
            let resume_ranges = record
                .resume
                .as_ref()
                .map(|resume| resume.verified_ranges.clone())
                .unwrap_or_default();
            if let Some(record) = state.transfers.get_mut(&transfer_id) {
                record.state = if both {
                    TransferState::ActiveDirect
                } else {
                    TransferState::NegotiatingDirect {
                        started_at,
                        ready_source,
                        ready_recipient,
                        candidates_source,
                        candidates_recipient,
                        offer_seen,
                        answer_seen,
                    }
                };
            }
            both.then_some((source, recipient, resume_ranges))
        };
        let Some((source, recipient, resume_ranges)) = committed else {
            return Ok(Vec::new());
        };
        let commit = crate::web_transfer_protocol::transfer_path_commit_envelope(
            transfer_id,
            attempt_id,
            "direct",
            &resume_ranges,
        );
        Ok(vec![(recipient, commit.clone()), (source, commit)])
    }

    /// One participant reports the direct attempt over. The attempt is
    /// invalidated EXACTLY once (a second report, or one naming an attempt
    /// that is no longer current, is acked and ignored), the counterpart
    /// hears a fixed reason code plus the recipient's bounded verified
    /// ranges, and the SAME transfer continues on a fresh relay attempt.
    ///
    /// Only the recipient's ranges are believed: it is the only side that
    /// knows what it verified and wrote.
    pub fn direct_failed(
        &self,
        room: &Arc<WebTransferRoom>,
        from: PeerId,
        body: &crate::web_transfer_protocol::DirectFailedBody,
    ) -> Result<(ReadyOutcome, TransferOutbox), WebTransferError> {
        let transfer_id = body.transfer_id;
        let (is_recipient, counterpart) = {
            let state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            let record = state
                .transfers
                .get(&transfer_id)
                .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
            if record.source == from {
                (false, record.recipient)
            } else if record.recipient == from {
                (true, record.source)
            } else {
                return Err(WebTransferError::not_participant(
                    "stranger to this transfer",
                ));
            }
        };
        let ranges = if is_recipient {
            body.verified_ranges.clone()
        } else {
            Vec::new()
        };
        match self.fallback_to_relay(room, transfer_id, body.attempt_id, ranges) {
            Some((outcome, mut outbox, forwarded_ranges)) => {
                // The counterpart hears WHY before it hears its ticket: the
                // notice explains the new attempt the ticket belongs to.
                outbox.insert(
                    0,
                    (
                        counterpart,
                        crate::web_transfer_protocol::transfer_direct_failed_envelope(
                            transfer_id,
                            body.attempt_id,
                            body.reason,
                            &forwarded_ranges,
                        ),
                    ),
                );
                Ok((outcome, outbox))
            }
            // Stale attempt, already fallen back, or terminal: acked and
            // ignored. A late failure must never touch a newer attempt —
            // except for the one thing only the recipient can say.
            None => {
                if is_recipient && !body.verified_ranges.is_empty() {
                    self.adopt_late_recipient_resume(
                        room,
                        transfer_id,
                        body.attempt_id,
                        &body.verified_ranges,
                    )?;
                }
                Ok((ReadyOutcome::Ignored, Vec::new()))
            }
        }
    }

    /// Takes the recipient's verified ranges from a `transfer.direct_failed`
    /// that arrived AFTER the attempt had already fallen back.
    ///
    /// The source learns its DataChannel is dead on its very next write,
    /// which is synchronous, while the recipient learns it from a `close`
    /// event; so in the ordinary case the SOURCE reports first, and a source
    /// is never believed about ranges (D3). Without this the fallback threw
    /// away the only resume information that exists: the relay attempt
    /// re-sent every chunk already on disk while the recipient, planning
    /// around what it held, read the first relayed chunk as a digest
    /// mismatch.
    ///
    /// Accepted only while the replacement attempt is still `WaitingRelay`
    /// and only for the attempt that actually failed. The ranges ride
    /// `transfer.path_commit`, which is sent when both relay legs attach, so
    /// nothing the source has already been told can change under it.
    fn adopt_late_recipient_resume(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        failed_attempt: AttemptId,
        ranges: &[(u64, u64)],
    ) -> Result<(), WebTransferError> {
        let mut state = room
            .state
            .lock()
            .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
        let Some(record) = state.transfers.get_mut(&transfer_id) else {
            return Ok(());
        };
        if record.state != TransferState::WaitingRelay
            || record.last_direct_attempt != Some(failed_attempt)
        {
            return Ok(());
        }
        // `zip` has no manifest length; the recipient carries its own and
        // the source regenerates from byte zero either way.
        let output_length = record.entry_size.unwrap_or(0);
        record.resume = Some(ResumeDescriptor {
            verified_ranges: ranges.to_vec(),
            output_length,
        });
        Ok(())
    }

    /// Turns a live direct attempt into a fresh RELAY attempt, reusing the
    /// Phase 3 admission and ticket flow unchanged. Returns `None` — and
    /// changes nothing — unless the transfer is still on `failed_attempt`
    /// in a direct state, which is what makes the deadline timer, a late
    /// `transfer.direct_failed` and a racing terminal converge on one
    /// fallback instead of three.
    ///
    /// On success the outbox holds each peer's own relay ticket
    /// ([`ReadyOutcome::Admitted`]) or is empty and the caller must spawn
    /// [`Self::admit_relay`] for the new attempt ([`ReadyOutcome::Queued`]).
    pub fn fallback_to_relay(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        failed_attempt: AttemptId,
        recipient_ranges: Vec<(u64, u64)>,
    ) -> Option<FallbackOutcome> {
        use crate::web_transfer_protocol::transfer_relay_ticket_envelope;
        let mut state = room.state.lock().ok()?;
        let record = state.transfers.get(&transfer_id)?;
        // Direct states only: a transfer already on the relay has had its one
        // automatic fallback, and a terminal one keeps its terminal.
        if !record.state.is_negotiating_direct() && record.state != TransferState::ActiveDirect {
            return None;
        }
        if record.attempt_id != Some(failed_attempt) {
            return None;
        }
        let source = record.source;
        let recipient = record.recipient;
        let output_length = record.entry_size.unwrap_or(0);
        let number = next_attempt_number(record.attempt_number).ok()?;
        let attempt_id = generate_attempt_id();
        let ranges = if recipient_ranges.is_empty() {
            record
                .resume
                .as_ref()
                .map(|resume| resume.verified_ranges.clone())
                .unwrap_or_default()
        } else {
            recipient_ranges
        };
        {
            let record = state.transfers.get_mut(&transfer_id)?;
            record.attempt_number = number;
            record.attempt_id = Some(attempt_id);
            // Dropping the old attempt releases nothing (a direct attempt
            // never held a relay permit) and is what makes a late frame or a
            // stale timer unable to reach the new one.
            record.attempt = Some(AttemptState {
                attempt_id,
                attempt_number: number,
                relay_permit: None,
            });
            record.state = TransferState::WaitingRelay;
            record.last_direct_attempt = Some(failed_attempt);
            if !ranges.is_empty() {
                record.resume = Some(ResumeDescriptor {
                    verified_ranges: ranges.clone(),
                    output_length,
                });
            }
        }
        // Exactly the Phase 3 admission: try the global semaphore inline,
        // mint two distinct role-bound tickets and hand each peer its own;
        // a busy relay leaves the transfer retryable without a permit.
        let granted = match Arc::clone(&self.inner.relay_permits).try_acquire_owned() {
            Ok(permit) => {
                let (source_ticket, recipient_ticket) = loop {
                    let first = generate_relay_ticket();
                    let second = generate_relay_ticket();
                    if first.to_string() != second.to_string() {
                        break (first, second);
                    }
                };
                let expires_at = Instant::now() + self.inner.ticket_ttl;
                if let Some(registry) = room.registry.upgrade() {
                    for (ticket, peer, role) in [
                        (&source_ticket, source, RelayRole::Source),
                        (&recipient_ticket, recipient, RelayRole::Recipient),
                    ] {
                        registry.tickets.insert(
                            ticket.sha256_hash(),
                            RelayTicketRecord {
                                ticket_hash: ticket.sha256_hash(),
                                transfer_id,
                                attempt_id,
                                peer_id: peer,
                                role,
                                expires_at,
                            },
                        );
                    }
                }
                if let Some(record) = state.transfers.get_mut(&transfer_id) {
                    if let Some(attempt) = record.attempt.as_mut() {
                        attempt.relay_permit = Some(permit);
                    }
                }
                Some((source_ticket.to_string(), recipient_ticket.to_string()))
            }
            Err(_) => None,
        };
        drop(state);
        match granted {
            Some((source_ticket, recipient_ticket)) => Some((
                ReadyOutcome::Admitted,
                vec![
                    (
                        source,
                        transfer_relay_ticket_envelope(transfer_id, attempt_id, &source_ticket),
                    ),
                    (
                        recipient,
                        transfer_relay_ticket_envelope(transfer_id, attempt_id, &recipient_ticket),
                    ),
                ],
                ranges,
            )),
            None => Some((ReadyOutcome::Queued, Vec::new(), ranges)),
        }
    }

    /// The attempt ID a transfer is currently on, for callers that must
    /// follow a fallback (the deadline timer's `admit_relay` hand-off).
    pub fn current_attempt(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
    ) -> Option<AttemptId> {
        room.state
            .lock()
            .ok()
            .and_then(|state| state.transfers.get(&transfer_id).and_then(|r| r.attempt_id))
    }

    /// Arms the direct deadline for one attempt. The task captures a `Weak`
    /// room (P-14's rule: a monitor must never resolve a key later and reach
    /// a newer object, and must never pin what it watches) plus the transfer
    /// and attempt IDs; at the deadline it falls back only when all three
    /// still identify the same live direct attempt.
    pub fn spawn_direct_deadline(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) {
        let registry = self.clone();
        let weak = Arc::downgrade(room);
        let deadline = self.direct_deadline();
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            let Some(room) = weak.upgrade() else {
                return;
            };
            registry
                .direct_deadline_elapsed(&room, transfer_id, attempt_id)
                .await;
        });
    }

    /// The deadline body, separated from the sleep so a test can fire it
    /// without a clock. Falls back once and then hands a queued attempt to
    /// the ordinary relay admission waiter.
    pub async fn direct_deadline_elapsed(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) {
        let Some((outcome, outbox, ranges)) =
            self.fallback_to_relay(room, transfer_id, attempt_id, Vec::new())
        else {
            return;
        };
        let counterparts = room
            .state
            .lock()
            .ok()
            .and_then(|state| {
                state
                    .transfers
                    .get(&transfer_id)
                    .map(|record| (record.source, record.recipient))
            })
            .map(|(source, recipient)| vec![source, recipient])
            .unwrap_or_default();
        // A timeout has no reporting peer, so BOTH hear the same fixed code.
        for peer in counterparts {
            let _ = room.send_to(
                peer,
                crate::web_transfer_protocol::transfer_direct_failed_envelope(
                    transfer_id,
                    attempt_id,
                    "timeout",
                    &ranges,
                ),
            );
        }
        drain_transfer_outbox(room, outbox);
        if matches!(outcome, ReadyOutcome::Queued) {
            if let Some(next) = self.current_attempt(room, transfer_id) {
                self.admit_relay(room, transfer_id, next).await;
            }
        }
    }

    /// Shared validation for the three forwarded signaling messages: the
    /// sender must be the side the protocol assigns to that step, on the
    /// current attempt of a negotiating transfer, inside the per-side
    /// candidate budget. Returns the counterpart to forward to.
    fn signal_slot(
        &self,
        room: &Arc<WebTransferRoom>,
        from: PeerId,
        transfer_id: TransferId,
        attempt_id: AttemptId,
        kind: SignalKind,
    ) -> Result<PeerId, WebTransferError> {
        let mut state = room
            .state
            .lock()
            .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
        let record = state
            .transfers
            .get(&transfer_id)
            .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
        let is_source = record.source == from;
        if !is_source && record.recipient != from {
            return Err(WebTransferError::not_participant(
                "stranger to this transfer",
            ));
        }
        if record.attempt_id != Some(attempt_id) {
            return Err(WebTransferError::invalid("stale attempt"));
        }
        let TransferState::NegotiatingDirect {
            started_at,
            ready_source,
            ready_recipient,
            mut candidates_source,
            mut candidates_recipient,
            mut offer_seen,
            mut answer_seen,
        } = record.state
        else {
            return Err(WebTransferError::invalid("transfer is not negotiating"));
        };
        let counterpart = if is_source {
            record.recipient
        } else {
            record.source
        };
        match kind {
            SignalKind::Offer => {
                if is_source {
                    return Err(WebTransferError::invalid("only the recipient offers"));
                }
                if offer_seen {
                    return Err(WebTransferError::invalid("offer already sent"));
                }
                offer_seen = true;
            }
            SignalKind::Answer => {
                if !is_source {
                    return Err(WebTransferError::invalid("only the source answers"));
                }
                if !offer_seen {
                    return Err(WebTransferError::invalid("no offer to answer"));
                }
                if answer_seen {
                    return Err(WebTransferError::invalid("answer already sent"));
                }
                answer_seen = true;
            }
            SignalKind::Ice => {
                let counter = if is_source {
                    &mut candidates_source
                } else {
                    &mut candidates_recipient
                };
                let cap =
                    u32::try_from(WEB_TRANSFER_MAX_ICE_CANDIDATES_PER_SIDE).unwrap_or(u32::MAX);
                if *counter >= cap {
                    return Err(self.refused(WebTransferError::limit("candidate budget spent")));
                }
                *counter += 1;
            }
        }
        if let Some(record) = state.transfers.get_mut(&transfer_id) {
            record.state = TransferState::NegotiatingDirect {
                started_at,
                ready_source,
                ready_recipient,
                candidates_source,
                candidates_recipient,
                offer_seen,
                answer_seen,
            };
        }
        Ok(counterpart)
    }

    /// Source declines `transfer.incoming`: only the source, terminal
    /// `Cancelled` with the recipient notified (a decline is a cancel by
    /// the serving side, so no new message type is needed). The notice is
    /// returned for post-ack delivery.
    pub fn source_reject(
        &self,
        room: &Arc<WebTransferRoom>,
        source: PeerId,
        transfer_id: TransferId,
    ) -> Result<TransferOutbox, WebTransferError> {
        let notice = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            let record = state
                .transfers
                .get(&transfer_id)
                .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
            if record.source != source {
                if record.recipient == source {
                    return Err(WebTransferError::invalid("only the source declines"));
                }
                return Err(WebTransferError::not_participant(
                    "stranger to this transfer",
                ));
            }
            if !record.state.is_live() {
                return Ok(Vec::new());
            }
            let recipient = record.recipient;
            let registry = room.registry.upgrade();
            let changed = terminate_locked(
                registry.as_deref(),
                &mut state,
                transfer_id,
                TransferState::Cancelled,
            );
            changed.then(|| {
                (
                    recipient,
                    crate::web_transfer_protocol::transfer_cancelled_envelope(transfer_id, source),
                )
            })
        };
        if let Some((peer, message)) = notice {
            Ok(vec![(peer, message)])
        } else {
            Ok(Vec::new())
        }
    }

    /// Cancels a transfer: only source or recipient; strangers hear
    /// `NOT_PARTICIPANT`. Terminal repeats ack without resending or
    /// double-releasing. Cancelling drops tickets and frees both permits
    /// at the transition. The notice is returned for post-ack delivery.
    pub fn cancel_transfer(
        &self,
        room: &Arc<WebTransferRoom>,
        peer: PeerId,
        transfer_id: TransferId,
    ) -> Result<(CancelOutcome, TransferOutbox), WebTransferError> {
        let notice = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            let record = state
                .transfers
                .get(&transfer_id)
                .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
            if !record.state.is_live() {
                return Ok((CancelOutcome::AlreadyTerminal, Vec::new()));
            }
            if peer != record.source && peer != record.recipient {
                return Err(WebTransferError::not_participant(
                    "stranger to this transfer",
                ));
            }
            let other = if record.source == peer {
                record.recipient
            } else {
                record.source
            };
            let registry = room.registry.upgrade();
            terminate_locked(
                registry.as_deref(),
                &mut state,
                transfer_id,
                TransferState::Cancelled,
            );
            (
                other,
                crate::web_transfer_protocol::transfer_cancelled_envelope(transfer_id, peer),
            )
        };
        Ok((CancelOutcome::Cancelled, vec![notice]))
    }

    /// The recipient reports VERIFIED bytes, and the server forwards the
    /// report to the source with the path it committed itself.
    ///
    /// Two rules live here and nowhere else. First, only the RECIPIENT may
    /// report: it is the only party that verified a digest against the
    /// manifest, and a source reporting its own progress would be attesting
    /// bytes nobody checked. Second, the `path` in the forwarded message is
    /// the SERVER's, derived from the transfer's own state — a peer may say
    /// what it verified and nothing else, so no peer can move another
    /// transfer's path or claim a transport it is not on.
    ///
    /// A report naming a stale attempt, or arriving once the transfer is no
    /// longer carrying, is acked and dropped: it describes a world that has
    /// already ended.
    pub fn report_progress(
        &self,
        room: &Arc<WebTransferRoom>,
        reporter: PeerId,
        body: &crate::web_transfer_protocol::ProgressBody,
    ) -> Result<TransferOutbox, WebTransferError> {
        let mut state = room
            .state
            .lock()
            .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
        let record = state
            .transfers
            .get_mut(&body.transfer_id)
            .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
        if record.recipient != reporter {
            if record.source == reporter {
                return Err(WebTransferError::invalid("only the recipient reports"));
            }
            return Err(WebTransferError::not_participant(
                "stranger to this transfer",
            ));
        }
        if record.attempt_id != Some(body.attempt_id) || !record.state.is_carrying() {
            // Stale or terminal: acked and ignored, exactly as a late
            // `transfer.direct_failed` is.
            return Ok(Vec::new());
        }
        if record
            .entry_size
            .is_some_and(|size| body.received_bytes > size)
        {
            return Err(WebTransferError::invalid("progress past the entry size"));
        }
        // The first report with bytes on it is what makes the path REAL: up
        // to that moment the transport is committed but nothing has been
        // verified over it, and a path nobody has carried a verified byte on
        // is not a fact about the transfer yet.
        if body.received_bytes == 0 {
            return Ok(Vec::new());
        }
        let direct = record.state == TransferState::ActiveDirect;
        let path = if direct { "direct" } else { "relay" };
        // Count the attempt exactly once, HERE and nowhere else: this is the
        // first moment a verified byte exists on this path, and the counter
        // has to mean "carried" rather than "attempted" (F-12).
        if record.carried_attempt != Some(body.attempt_id) {
            record.carried_attempt = Some(body.attempt_id);
            let counter = if direct {
                &self.inner.direct_carried
            } else {
                &self.inner.relay_carried
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        let source = record.source;
        let notice = crate::web_transfer_protocol::transfer_progress_envelope(
            body.transfer_id,
            body.attempt_id,
            body.received_bytes,
            path,
        );
        Ok(vec![(source, notice)])
    }

    /// Completes a transfer: only the recipient, only the current attempt,
    /// only with the manifest entry root (server-side cross-check of the
    /// recipient's local verification). The source notice is returned for
    /// post-ack delivery.
    pub fn complete_transfer(
        &self,
        room: &Arc<WebTransferRoom>,
        recipient: PeerId,
        transfer_id: TransferId,
        attempt_id: AttemptId,
        root: [u8; 32],
    ) -> Result<TransferOutbox, WebTransferError> {
        let notice = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            let record = state
                .transfers
                .get(&transfer_id)
                .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
            if record.recipient != recipient {
                if record.source == recipient {
                    return Err(WebTransferError::invalid("only the recipient completes"));
                }
                return Err(WebTransferError::not_participant(
                    "stranger to this transfer",
                ));
            }
            if !record.state.is_live() {
                return Err(WebTransferError::invalid("transfer already terminated"));
            }
            // Completion attests verified bytes on the wire, which only
            // exists once both legs attached (Phase 3.2 drives Active).
            // Earlier states reject here; their success path is 3.2-tested.
            // Either transport may be the one that carried it: `Active` is
            // the relay pump's state, `ActiveDirect` the DataChannel's.
            if !record.state.is_carrying() {
                return Err(WebTransferError::invalid("transfer is not active"));
            }
            if record.attempt_id != Some(attempt_id) {
                return Err(WebTransferError::invalid("stale attempt"));
            }
            // `raw` is checked against the manifest the server holds; an
            // archive has no manifest root to check against, and the
            // recipient has already refused a mismatch of its own before it
            // could ever report completion.
            if record.entry_root.is_some_and(|expected| expected != root) {
                return Err(WebTransferError::invalid("root does not match manifest"));
            }
            let source = record.source;
            let registry = room.registry.upgrade();
            terminate_locked(
                registry.as_deref(),
                &mut state,
                transfer_id,
                TransferState::Completed,
            );
            (
                source,
                crate::web_transfer_protocol::transfer_completed_envelope(transfer_id, &root),
            )
        };
        Ok(vec![notice])
    }

    /// Fails a transfer (Phase 3.2 attach faults own this path): terminal
    /// `Failed` with both permits released. Notices are the caller's job —
    /// 3.2 maps attach faults to its retryable errors there.
    #[allow(dead_code)]
    pub(crate) fn fail_transfer(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
    ) -> bool {
        let Ok(mut state) = room.state.lock() else {
            return false;
        };
        terminate_locked(
            room.registry.upgrade().as_deref(),
            &mut state,
            transfer_id,
            TransferState::Failed,
        )
    }

    /// Admits one queued attempt: waits up to the configured timeout for a
    /// global relay slot, then — only if the transfer still waits for this
    /// exact attempt without a permit — stores the permit, mints two
    /// distinct role-bound tickets and sends each peer only its own. On
    /// timeout the recipient hears `RELAY_BUSY` (with the transfer ID for
    /// correlation) and the transfer stays resumable without a permit; a
    /// fresh click/requestId retries it. Spawned by the actor, never awaited
    /// while holding `RoomState`.
    pub async fn admit_relay(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) {
        let wait = self
            .inner
            .admit_timeout
            .lock()
            .map(|timeout| *timeout)
            .unwrap_or(WEB_TRANSFER_RELAY_ADMIT_TIMEOUT);
        let permit =
            match timeout(wait, Arc::clone(&self.inner.relay_permits).acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                _ => {
                    let recipient = room.state.lock().ok().and_then(|state| {
                        state
                            .transfers
                            .get(&transfer_id)
                            .filter(|record| {
                                record.state == TransferState::WaitingRelay
                                    && record.attempt_id == Some(attempt_id)
                            })
                            .map(|record| record.recipient)
                    });
                    if let Some(recipient) = recipient {
                        room.send_to(
                            recipient,
                            crate::web_transfer_protocol::error_envelope_anon(
                                "RELAY_BUSY",
                                Some(&transfer_id.to_string()),
                            ),
                        );
                    }
                    return;
                }
            };
        struct Issued {
            source: PeerId,
            recipient: PeerId,
            source_ticket: String,
            recipient_ticket: String,
        }
        let issued = {
            let mut state = match room.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            let current = state.transfers.get(&transfer_id).map(|record| {
                (
                    record.state,
                    record.attempt_id,
                    record
                        .attempt
                        .as_ref()
                        .map(|attempt| attempt.relay_permit.is_none())
                        .unwrap_or(false),
                    record.source,
                    record.recipient,
                )
            });
            let Some((TransferState::WaitingRelay, Some(current), true, source, recipient)) =
                current
            else {
                return;
            };
            let (source_ticket, recipient_ticket) = loop {
                let first = generate_relay_ticket();
                let second = generate_relay_ticket();
                if first.to_string() != second.to_string() {
                    break (first, second);
                }
            };
            let expires_at = Instant::now() + self.inner.ticket_ttl;
            if let Some(registry) = room.registry.upgrade() {
                for (ticket, peer, role) in [
                    (&source_ticket, source, RelayRole::Source),
                    (&recipient_ticket, recipient, RelayRole::Recipient),
                ] {
                    registry.tickets.insert(
                        ticket.sha256_hash(),
                        RelayTicketRecord {
                            ticket_hash: ticket.sha256_hash(),
                            transfer_id,
                            attempt_id: current,
                            peer_id: peer,
                            role,
                            expires_at,
                        },
                    );
                }
            }
            if let Some(record) = state.transfers.get_mut(&transfer_id) {
                if let Some(attempt) = record.attempt.as_mut() {
                    attempt.relay_permit = Some(permit);
                }
            }
            Issued {
                source,
                recipient,
                source_ticket: source_ticket.to_string(),
                recipient_ticket: recipient_ticket.to_string(),
            }
        };
        use crate::web_transfer_protocol::transfer_relay_ticket_envelope;
        let _ = room.send_to(
            issued.source,
            transfer_relay_ticket_envelope(transfer_id, attempt_id, &issued.source_ticket),
        );
        let _ = room.send_to(
            issued.recipient,
            transfer_relay_ticket_envelope(transfer_id, attempt_id, &issued.recipient_ticket),
        );
    }

    /// Consumes one relay ticket atomically (single lookup-and-remove):
    /// unknown or already-used hashes, expired deadlines and wrong-leg
    /// presentments all fail, and a failed presentment still burns the
    /// ticket. `now` is a parameter so expiry unit-tests need no clock.
    pub fn consume_relay_ticket(
        &self,
        ticket_hex: &str,
        role: RelayRole,
        peer: PeerId,
        now: Instant,
    ) -> Result<TicketGrant, TicketDeny> {
        let ticket: RelayTicket = ticket_hex.parse().map_err(|_| TicketDeny::Unknown)?;
        let (_, record) = self
            .inner
            .tickets
            .remove(&ticket.sha256_hash())
            .ok_or(TicketDeny::Unknown)?;
        if now > record.expires_at {
            return Err(TicketDeny::Expired);
        }
        if record.role != role || record.peer_id != peer {
            return Err(TicketDeny::RoleMismatch);
        }
        Ok(TicketGrant {
            transfer_id: record.transfer_id,
            attempt_id: record.attempt_id,
            peer_id: record.peer_id,
        })
    }
}

// ---------------------------------------------------------------------------
// Opaque relay pair (Phase 3.2): two role-bound WebSocket legs spliced by one
// pump task. The server never holds payload beyond the frame being forwarded:
// no mpsc payload queue, no accumulating Vec, no temp file, no clone of the
// ciphertext bytes (tungstenite `Bytes` move straight through).
// ---------------------------------------------------------------------------

/// First relay leg message deadline: the one text `relay.attach` must arrive
/// within 10 s of the handshake; anything else closes the socket.
pub const WEB_TRANSFER_RELAY_FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-forward send deadline on the warm recipient leg (10 s).
pub const WEB_TRANSFER_RELAY_SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Grace to drain the peer's close echo after our own Close frame (2 s).
/// Dropping TCP first resets the connection, which browsers log as an
/// error even when every byte arrived.
pub const WEB_TRANSFER_RELAY_CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Smallest forwardable source binary: 16-byte header + ≥1 ciphertext byte.
pub const WEB_TRANSFER_RELAY_MIN_FRAME_LEN: usize = 17;

/// Relay frame type: one chunk of payload.
pub const RELAY_FRAME_TYPE_DATA: u8 = 1;

/// Relay frame type: the last frame of an attempt. It is the END OF STREAM
/// on this transport exactly as it is on the DataChannel — the pump stops on
/// it and never waits for the source's transport to close.
pub const RELAY_FRAME_TYPE_FINAL: u8 = 2;
/// Largest forwardable source binary: one encrypted frame per message.
pub const WEB_TRANSFER_RELAY_MAX_FRAME_LEN: usize = 32 * 1024;
/// Throttle burst ceiling: exactly 2×rate, never above 200 MiB.
pub const WEB_TRANSFER_RELAY_BURST_CAP: u64 = 200 * 1024 * 1024;
/// Encrypted-frame magic the pump checks without decrypting (`BWT1`).
pub const RELAY_FRAME_MAGIC: u32 = 0x42575431;
/// Encrypted-frame version the pump accepts.
pub const RELAY_FRAME_VERSION: u16 = 1;

/// Why a source binary frame is refused. Header-only: the pump never holds a
/// key and never decrypts, so a well-formed header with a bad body still
/// forwards (the recipient's AEAD rejects it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFrameReject {
    /// Fewer than 17 bytes (cannot hold header + 1 ciphertext byte).
    TooShort,
    /// More than 32768 bytes (transport already caps this; defense in depth).
    Oversize,
    /// Magic is not `BWT1`.
    BadMagic,
    /// Version is not 1.
    BadVersion,
    /// Type is not DATA (1) or FINAL (2).
    BadType,
    /// Reserved flags are not 0.
    BadFlags,
    /// `body_len` does not equal the trailing bytes.
    LengthMismatch,
    /// Sequence is not exactly previous + 1 (first frame must be 0).
    StaleSeq,
}

/// Validates one encrypted-frame header per the protocol §6 layout, without
/// touching the ciphertext. Returns the accepted sequence number.
pub fn check_relay_frame(prev_seq: Option<u32>, bytes: &[u8]) -> Result<u32, RelayFrameReject> {
    use RelayFrameReject as R;
    if bytes.len() < WEB_TRANSFER_RELAY_MIN_FRAME_LEN {
        return Err(R::TooShort);
    }
    if bytes.len() > WEB_TRANSFER_RELAY_MAX_FRAME_LEN {
        return Err(R::Oversize);
    }
    let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic != RELAY_FRAME_MAGIC {
        return Err(R::BadMagic);
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version != RELAY_FRAME_VERSION {
        return Err(R::BadVersion);
    }
    if bytes[6] != RELAY_FRAME_TYPE_DATA && bytes[6] != RELAY_FRAME_TYPE_FINAL {
        return Err(R::BadType);
    }
    if bytes[7] != 0 {
        return Err(R::BadFlags);
    }
    let seq = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let body_len = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    if body_len != bytes.len() - 16 {
        return Err(R::LengthMismatch);
    }
    let expected = match prev_seq {
        None => 0,
        Some(prev) => prev.checked_add(1).ok_or(R::StaleSeq)?,
    };
    if seq != expected {
        return Err(R::StaleSeq);
    }
    Ok(seq)
}

/// Shared per-room relay throttle. Rate comes from limits
/// (`relay_rate_bytes_per_s`); burst is exactly 2×rate capped at 200 MiB; a
/// zero rate disables throttling entirely (no delay, no debt).
#[derive(Debug)]
pub struct RelayThrottle {
    bucket: Option<TokenBucket>,
}

impl RelayThrottle {
    /// Builds the room throttle from the configured byte rate.
    pub fn new(rate_bytes_per_s: u64) -> Self {
        if rate_bytes_per_s == 0 {
            return Self { bucket: None };
        }
        let rate = rate_bytes_per_s as f64;
        let burst = (2.0 * rate).min(WEB_TRANSFER_RELAY_BURST_CAP as f64);
        Self {
            bucket: Some(TokenBucket::new(rate, burst)),
        }
    }

    /// Returns how long `n` bytes must wait. Zero when throttling is
    /// disabled or the bucket covers the bytes.
    pub fn delay_for(&mut self, now: Instant, n: u64) -> Duration {
        match self.bucket.as_mut() {
            None => Duration::ZERO,
            Some(bucket) => bucket.take_bytes(now, n),
        }
    }

    /// Effective burst for tests (0 when disabled).
    #[cfg(test)]
    pub fn burst(&self) -> f64 {
        self.bucket
            .as_ref()
            .map(|b| b.burst_for_test())
            .unwrap_or(0.0)
    }
}

/// Boxed relay write half: `Sink` is object-safe, so the pairing map never
/// names the transport type (plain TCP, TLS and prefixed streams all pair).
pub type RelaySink =
    std::pin::Pin<Box<dyn futures_util::sink::Sink<Message, Error = WsError> + Send + 'static>>;
/// Boxed relay read half, same type erasure as the sink.
pub type RelayStream = BoxStream<'static, Result<Message, WsError>>;

/// One attached relay leg with its boxed halves.
pub struct RelayLeg {
    /// Attaching peer (already ticket-bound).
    pub peer: PeerId,
    /// Which side this leg serves.
    pub role: RelayRole,
    /// Write half (recipient data + close/pong both sides).
    pub sink: RelaySink,
    /// Read half (source data, violation watch on the recipient).
    pub stream: RelayStream,
}

/// Parked first leg: only a handoff sender plus metadata, never payload.
pub(crate) struct RelayWaiter {
    /// Role of the parked leg; the pair must be complementary.
    pub first_role: RelayRole,
    /// Peer holding the parked socket.
    pub first_peer: PeerId,
    /// Receives the second leg's halves; the parked task becomes the pump.
    pub tx: oneshot::Sender<RelayLeg>,
}

/// One spliced relay pair: the four halves plus the identities and policy
/// the pump needs. Built by the parked task after activation; the pump takes
/// it whole so no call site can mix up the legs.
pub(crate) struct RelayPair {
    /// Transfer being relayed.
    pub transfer_id: TransferId,
    /// Attempt being relayed.
    pub attempt_id: AttemptId,
    /// Uploading peer (reads only).
    pub source: PeerId,
    /// Downloading peer (writes only).
    pub recipient: PeerId,
    /// Source write half (close/pong only).
    pub source_sink: RelaySink,
    /// Source read half (payload).
    pub source_stream: RelayStream,
    /// Recipient write half (payload + close/pong).
    pub recipient_sink: RelaySink,
    /// Recipient read half (violation watch only).
    pub recipient_stream: RelayStream,
    /// Transfer-level cancel token (terminal transitions fire it).
    pub cancel: CancellationToken,
    /// Per-forward send deadline on the warm recipient leg.
    pub send_timeout: Duration,
    /// Grace to drain the close echo at pump end (tests shorten it).
    pub close_grace: Duration,
}

/// Opaque pump counters: aggregate bytes/frames only, never content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelayStats {
    /// Ciphertext bytes forwarded.
    pub bytes: u64,
    /// Frames forwarded.
    pub frames: u64,
    /// Pump start (elapsed derived at the end).
    pub started: Instant,
}

/// How a relay pump ended. Only `Clean` keeps the transfer `Active` (the
/// recipient verifies and completes over control); `Cancelled` is already
/// terminal; every other end fails the attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayEnd {
    /// Source closed orderly after its payload.
    Clean,
    /// Source transport broke.
    SourceGone,
    /// Recipient transport broke or closed first.
    RecipientGone,
    /// Malformed source frame, source text, or any recipient payload.
    Violation,
    /// Warm recipient leg did not drain within the send deadline.
    SendTimeout,
    /// Transfer terminated underneath the pump.
    Cancelled,
}

/// Outcome of offering one leg to the pairing rendezvous.
pub(crate) enum RelayJoin {
    /// First leg: keep the socket, wait for the peer on `wait` until
    /// `deadline`, selecting on `cancel` too.
    Park {
        /// Own halves, kept by the parking task.
        leg: RelayLeg,
        /// Receives the second leg; the parked task then pumps.
        wait: oneshot::Receiver<RelayLeg>,
        /// Pairing deadline (30 s after this attach).
        deadline: Instant,
        /// Transfer-level cancel token (terminal transitions fire it).
        cancel: CancellationToken,
    },
    /// Second leg: hand `leg` to the parked peer, then exit quietly — the
    /// parked task owns both sockets from here on.
    Pair {
        /// Own halves, moved to the parked task.
        leg: RelayLeg,
        /// Sends to the parked task; failure means it left (abort instead).
        send_to_parked: oneshot::Sender<RelayLeg>,
    },
}

impl WebTransferRegistry {
    /// Pair the transfer's relay legs: consume the ticket atomically, bind the
    /// leg to its validated role, then park (first) or hand off (second).
    /// Invalid tickets, stale attempts and duplicate legs all deny with
    /// `TRANSFER_NOT_FOUND`: the caller closes the socket without a control
    /// notice, so every failure shape is indistinguishable on the wire.
    pub(crate) fn join_relay_pair(
        &self,
        room: &Arc<WebTransferRoom>,
        attach: &crate::web_transfer_protocol::RelayAttach,
        ticket_hex: &str,
        sink: RelaySink,
        stream: RelayStream,
    ) -> Result<RelayJoin, WebTransferError> {
        let now = Instant::now();
        // The wire role duplicates the registry role by design (protocol and
        // registry evolve independently); map once at the boundary.
        let role = match attach.role {
            crate::web_transfer_protocol::RelayRole::Source => RelayRole::Source,
            crate::web_transfer_protocol::RelayRole::Recipient => RelayRole::Recipient,
        };
        let grant = self
            .consume_relay_ticket(ticket_hex, role, attach.peer_id, now)
            .map_err(|_| WebTransferError::transfer_not_found("unknown or spent ticket"))?;
        // The body repeats the ticket's own binding; incoherence denies.
        if attach.transfer_id != grant.transfer_id || attach.attempt_id != grant.attempt_id {
            return Err(WebTransferError::transfer_not_found(
                "attach does not match ticket",
            ));
        }
        let cancel = {
            let state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            let record = state
                .transfers
                .get(&grant.transfer_id)
                .ok_or_else(|| WebTransferError::transfer_not_found("unknown transfer"))?;
            if !record.state.is_live() || record.state != TransferState::WaitingRelay {
                return Err(WebTransferError::transfer_not_found(
                    "transfer not awaiting relay",
                ));
            }
            if record.attempt_id != Some(grant.attempt_id) {
                return Err(WebTransferError::transfer_not_found("stale attempt"));
            }
            let parties_match = match role {
                RelayRole::Source => record.source == attach.peer_id,
                RelayRole::Recipient => record.recipient == attach.peer_id,
            };
            if !parties_match {
                return Err(WebTransferError::transfer_not_found(
                    "leg does not match record",
                ));
            }
            record.cancel.clone()
        };
        let key = (grant.transfer_id, grant.attempt_id);
        let leg = RelayLeg {
            peer: attach.peer_id,
            role,
            sink,
            stream,
        };
        match self.inner.relay_waiters.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                let waiter = entry.get();
                // Same role twice (or the same peer twice) is a duplicate
                // leg: deny, the parked first leg keeps waiting.
                if waiter.first_role == role || waiter.first_peer == attach.peer_id {
                    return Err(WebTransferError::transfer_not_found("duplicate relay leg"));
                }
                let waiter = entry.remove();
                Ok(RelayJoin::Pair {
                    leg,
                    send_to_parked: waiter.tx,
                })
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                let (tx, wait) = oneshot::channel();
                slot.insert(RelayWaiter {
                    first_role: role,
                    first_peer: attach.peer_id,
                    tx,
                });
                Ok(RelayJoin::Park {
                    leg,
                    wait,
                    deadline: now + WEB_TRANSFER_RELAY_ATTACH_TIMEOUT,
                    cancel,
                })
            }
        }
    }

    /// Drops a pairing waiter if still present (parked task leaving via
    /// timeout or cancel). `false` means the peer already took it and owns
    /// the outcome — the caller must exit quietly, never abort.
    pub(crate) fn abandon_relay_waiter(
        &self,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) -> bool {
        self.inner
            .relay_waiters
            .remove(&(transfer_id, attempt_id))
            .is_some()
    }

    /// Moves a paired transfer to `Active` and hands out its cancel token.
    /// `None` means the attempt went stale while pairing (the pair aborts).
    pub(crate) fn activate_relay(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) -> Option<CancellationToken> {
        let mut state = room.state.lock().ok()?;
        let record = state.transfers.get_mut(&transfer_id)?;
        if !record.state.is_live() || record.state != TransferState::WaitingRelay {
            return None;
        }
        if record.attempt_id != Some(attempt_id) {
            return None;
        }
        record.state = TransferState::Active;
        Some(record.cancel.clone())
    }

    /// Fails a relay attempt once: terminal `Failed` plus a retryable
    /// `DIRECT_FAILED` notice naming the transfer on both control sockets.
    /// Idempotent — only the transition winner notifies.
    pub(crate) fn abort_relay_attempt(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) {
        let parties = {
            let Ok(state) = room.state.lock() else {
                return;
            };
            let Some(record) = state.transfers.get(&transfer_id) else {
                return;
            };
            if record.attempt_id != Some(attempt_id) {
                return;
            }
            (record.source, record.recipient)
        };
        if self.fail_transfer(room, transfer_id) {
            let notice = crate::web_transfer_protocol::error_envelope_anon(
                "DIRECT_FAILED",
                Some(&transfer_id.to_string()),
            );
            let _ = room.send_to(parties.0, notice.clone());
            let _ = room.send_to(parties.1, notice);
        }
    }

    /// Releases the attempt's relay permit exactly once (pump end and
    /// terminal transitions both take-if-present; the second is a no-op).
    pub(crate) fn release_relay_permit(
        &self,
        room: &Arc<WebTransferRoom>,
        transfer_id: TransferId,
        attempt_id: AttemptId,
    ) -> bool {
        let Ok(mut state) = room.state.lock() else {
            return false;
        };
        let Some(record) = state.transfers.get_mut(&transfer_id) else {
            return false;
        };
        if record.attempt_id != Some(attempt_id) {
            return false;
        }
        match record.attempt.as_mut() {
            Some(attempt) => attempt.relay_permit.take().is_some(),
            None => false,
        }
    }

    /// Sends Close on one leg and drains its close echo (or the
    /// transport end) within `grace`, so the TCP close never resets a
    /// fully-delivered connection out from under the browser.
    pub(crate) async fn graceful_relay_close(
        mut sink: RelaySink,
        mut stream: RelayStream,
        grace: Duration,
    ) {
        use futures_util::{SinkExt, StreamExt};
        let _ = sink.close().await;
        let _ = timeout(grace, async {
            while let Some(message) = stream.next().await {
                match message {
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        })
        .await;
    }

    /// Runs one paired relay: reads a source frame, rate-limits, forwards it
    /// to the recipient, then reads the next — at most one application frame
    /// is ever held. Ends `Clean` only on the source's orderly close (the
    /// recipient then verifies and completes over control); every other end
    /// fails the attempt. Releases the relay permit exactly once and logs
    /// opaque aggregates only (IDs, role, bytes, frames, outcome).
    pub(crate) async fn run_relay_pair(
        &self,
        room: &Arc<WebTransferRoom>,
        pair: RelayPair,
    ) -> (RelayEnd, RelayStats) {
        use futures_util::{SinkExt, StreamExt};
        let RelayPair {
            transfer_id,
            attempt_id,
            source,
            recipient,
            mut source_sink,
            mut source_stream,
            mut recipient_sink,
            mut recipient_stream,
            cancel,
            send_timeout,
            close_grace,
        } = pair;
        let mut stats = RelayStats {
            bytes: 0,
            frames: 0,
            started: Instant::now(),
        };
        // The recipient's verified ranges reach the SOURCE here and nowhere
        // else: the descriptor rides `transfer.request`, which the source
        // never sees. Absent record (already terminal) means no skipping.
        let resume_ranges = room
            .state
            .lock()
            .ok()
            .and_then(|state| {
                state
                    .transfers
                    .get(&transfer_id)
                    .and_then(|record| record.resume.as_ref())
                    .map(|resume| resume.verified_ranges.clone())
            })
            .unwrap_or_default();
        let commit = crate::web_transfer_protocol::transfer_path_commit_envelope(
            transfer_id,
            attempt_id,
            "relay",
            &resume_ranges,
        );
        let _ = room.send_to(source, commit.clone());
        let _ = room.send_to(recipient, commit);
        let mut last_seq: Option<u32> = None;
        let end = loop {
            tokio::select! {
                msg = source_stream.next() => {
                    match msg {
                        Some(Ok(Message::Binary(body))) => {
                            let len = body.len();
                            let seq = match check_relay_frame(last_seq, &body) {
                                Ok(seq) => seq,
                                Err(_) => break RelayEnd::Violation,
                            };
                            // Read before the frame is moved into the send.
                            let is_final = body[6] == RELAY_FRAME_TYPE_FINAL;
                            let delay = match room.relay_throttle.lock() {
                                Ok(mut throttle) => throttle.delay_for(Instant::now(), len as u64),
                                Err(_) => Duration::ZERO,
                            };
                            if !delay.is_zero() {
                                tokio::time::sleep(delay).await;
                            }
                            match timeout(
                                send_timeout,
                                recipient_sink.send(Message::Binary(body)),
                            )
                            .await
                            {
                                Ok(Ok(())) => {
                                    stats.bytes += len as u64;
                                    stats.frames += 1;
                                    self.inner
                                        .relay_bytes_total
                                        .fetch_add(len as u64, Ordering::Relaxed);
                                    last_seq = Some(seq);
                                    if is_final {
                                        // The attempt ends on its own terms.
                                        // Waiting for the source's TRANSPORT
                                        // to close instead made the end of
                                        // the stream a property of the
                                        // engine: MEASURED on WebKit, a
                                        // source that had written all
                                        // 8 388 613 bytes and then closed
                                        // its socket delivered 7 087 168 of
                                        // them — the close outran the frames
                                        // still queued behind it, the pump
                                        // read the stream as ended and
                                        // failed a COMPLETE transfer for
                                        // both peers. A close cannot
                                        // truncate a stream that has already
                                        // said it was over.
                                        break RelayEnd::Clean;
                                    }
                                }
                                _ => break RelayEnd::SendTimeout,
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = source_sink.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) => break RelayEnd::Clean,
                        Some(Ok(Message::Text(_))) => break RelayEnd::Violation,
                        // Raw fragments never carry a complete frame: the
                        // sender must emit whole Binary messages.
                        Some(Ok(Message::Frame(_))) => break RelayEnd::Violation,
                        Some(Err(_)) | None => break RelayEnd::SourceGone,
                    }
                }
                msg = recipient_stream.next() => {
                    match msg {
                        // The recipient leg never carries payload upstream.
                        Some(Ok(Message::Binary(_)))
                        | Some(Ok(Message::Text(_)))
                        | Some(Ok(Message::Frame(_))) => break RelayEnd::Violation,
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = recipient_sink.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                            break RelayEnd::RecipientGone
                        }
                    }
                }
                _ = cancel.cancelled() => break RelayEnd::Cancelled,
            }
        };
        let _ = tokio::join!(
            Self::graceful_relay_close(source_sink, source_stream, close_grace),
            Self::graceful_relay_close(recipient_sink, recipient_stream, close_grace)
        );
        let _ = self.release_relay_permit(room, transfer_id, attempt_id);
        let elapsed = stats.started.elapsed();
        match end {
            RelayEnd::Clean => {
                debug!(
                    transfer = %transfer_id,
                    attempt = %attempt_id,
                    bytes = stats.bytes,
                    frames = stats.frames,
                    elapsed_ms = elapsed.as_millis(),
                    "relay pair closed clean",
                );
            }
            RelayEnd::Cancelled => {
                debug!(
                    transfer = %transfer_id,
                    attempt = %attempt_id,
                    bytes = stats.bytes,
                    frames = stats.frames,
                    "relay pair cancelled",
                );
            }
            failed => {
                warn!(
                    transfer = %transfer_id,
                    attempt = %attempt_id,
                    bytes = stats.bytes,
                    frames = stats.frames,
                    outcome = ?failed,
                    "relay pair failed",
                );
                self.abort_relay_attempt(room, transfer_id, attempt_id);
            }
        }
        (end, stats)
    }
}

/// Generates a nonzero random room ID from the OS CSPRNG.
fn generate_room_id() -> RoomId {
    use ring::rand::{SecureRandom, SystemRandom};
    let random = SystemRandom::new();
    let mut bytes = [0u8; 16];
    random.fill(&mut bytes).expect("OS CSPRNG");
    if bytes == [0u8; 16] {
        bytes[15] = 1;
    }
    RoomId::from_bytes(bytes)
}

/// Plain server-flag values (clap-independent so lib tests pin the semantics
/// and `main.rs` only maps its flags onto this struct).
#[derive(Clone, Debug)]
pub struct WebTransferServerArgs {
    /// `--web-transfer-base-url`; `None` disables the service.
    pub base_url: Option<String>,
    /// `--web-transfer-stun` entries (repeat/comma, raw).
    pub stun: Vec<String>,
    /// `--web-transfer-no-stun`.
    pub no_stun: bool,
    /// All numeric caps in plan order.
    pub max_rooms: u64,
    /// Global peer cap.
    pub max_peers_global: u64,
    /// Per-room peer cap.
    pub max_peers_per_room: u64,
    /// Per-peer offer cap.
    pub max_offers_per_peer: u64,
    /// Per-offer entry cap.
    pub max_entries_per_offer: u64,
    /// Per-offer byte cap.
    pub max_offer_bytes: u64,
    /// Per-room metadata cap.
    pub max_metadata_per_room_bytes: u64,
    /// Server-wide metadata cap.
    pub max_metadata_total_bytes: u64,
    /// Per-peer transfer cap.
    pub max_transfers_per_peer: u64,
    /// Server-wide relay-pair cap.
    pub max_relays_global: u64,
    /// Per-room relay rate (0 disables throttling).
    pub relay_rate_bytes_per_s: u64,
    /// Abnormal owner-loss grace, 5..=600 s.
    pub owner_grace_secs: u64,
}

impl Default for WebTransferServerArgs {
    fn default() -> Self {
        let limits = WebTransferLimits::default();
        Self {
            base_url: None,
            stun: Vec::new(),
            no_stun: false,
            max_rooms: limits.max_rooms,
            max_peers_global: limits.max_peers_global,
            max_peers_per_room: limits.max_peers_per_room,
            max_offers_per_peer: limits.max_offers_per_peer,
            max_entries_per_offer: limits.max_entries_per_offer,
            max_offer_bytes: limits.max_offer_bytes,
            max_metadata_per_room_bytes: limits.max_metadata_per_room_bytes,
            max_metadata_total_bytes: limits.max_metadata_total_bytes,
            max_transfers_per_peer: limits.max_transfers_per_peer,
            max_relays_global: limits.max_relays_global,
            relay_rate_bytes_per_s: limits.relay_rate_bytes_per_s,
            owner_grace_secs: limits.owner_grace_secs,
        }
    }
}

/// Validates the target portion of `stun:HOST[:PORT]` without normalizing it.
fn validate_stun_target(target: &str, whole: &str) -> Result<()> {
    if target.is_empty()
        || target.chars().any(char::is_whitespace)
        || target.contains(['@', '/', '?', '#'])
    {
        bail!("STUN server must look like stun:HOST[:PORT], got {whole:?}");
    }

    let validate_port = |raw: &str| -> Result<()> {
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("STUN server port must be numeric in {whole:?}");
        }
        match raw.parse::<u16>() {
            Ok(port) if port != 0 => Ok(()),
            _ => bail!("STUN server port must be 1..=65535 in {whole:?}"),
        }
    };

    if let Some(rest) = target.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("STUN IPv6 host has unmatched brackets in {whole:?}"))?;
        let literal = &rest[..end];
        literal.parse::<std::net::Ipv6Addr>().map_err(|_| {
            anyhow::anyhow!("STUN bracketed host must be an IPv6 literal in {whole:?}")
        })?;
        let suffix = &rest[end + 1..];
        if suffix.is_empty() {
            return Ok(());
        }
        let raw = suffix
            .strip_prefix(':')
            .ok_or_else(|| anyhow::anyhow!("STUN IPv6 host has an invalid suffix in {whole:?}"))?;
        return validate_port(raw);
    }

    if target.contains(['[', ']']) {
        bail!("STUN host has unmatched brackets in {whole:?}");
    }
    let (host, port) = match target.matches(':').count() {
        0 => (target, None),
        1 => {
            let (host, raw) = target
                .rsplit_once(':')
                .ok_or_else(|| anyhow::anyhow!("invalid STUN target in {whole:?}"))?;
            (host, Some(raw))
        }
        _ => bail!("STUN IPv6 literals must be bracketed in {whole:?}"),
    };
    match url::Host::parse(host) {
        Ok(url::Host::Ipv6(_)) => bail!("STUN IPv6 literals must be bracketed in {whole:?}"),
        Ok(_) => {}
        Err(_) => bail!("invalid STUN host in {whole:?}"),
    }
    if let Some(raw) = port {
        validate_port(raw)?;
    }
    Ok(())
}

/// Parses one `--web-transfer-stun` list: splits commas, trims delimiter
/// whitespace, requires the `stun:` scheme with a host and optional numeric
/// port, deduplicates preserving first occurrence. `turn:` is never accepted.
pub fn parse_stun_list(entries: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in entries {
        for part in entry.split(',') {
            let part = part.trim();
            if part.is_empty() {
                bail!("STUN server entry must not be empty");
            }
            if part.starts_with("turn:") || part.starts_with("turns:") {
                bail!("web transfer has no TURN support, got {part:?}");
            }
            let target = part.strip_prefix("stun:").ok_or_else(|| {
                anyhow::anyhow!("STUN server must look like stun:HOST[:PORT], got {part:?}")
            })?;
            if target.is_empty() {
                bail!("STUN server must look like stun:HOST[:PORT], got {part:?}");
            }
            validate_stun_target(target, part)?;
            let normalized = format!("stun:{target}");
            if !out.contains(&normalized) {
                out.push(normalized);
            }
        }
    }
    Ok(out)
}

/// Resolves the server flags into an enabled service config, or `None` when
/// disabled. Any non-default web-transfer flag without `--web-transfer-base-url`
/// is an error, never silently ignored. Runs before the first listener binds.
pub fn resolve_server_config(
    args: &WebTransferServerArgs,
    server_udp: bool,
    control_port: u16,
) -> Result<Option<WebTransferConfig>> {
    let Some(base_url_raw) = args.base_url.as_deref() else {
        let defaults = WebTransferServerArgs::default();
        if args.no_stun {
            bail!("--web-transfer-no-stun requires --web-transfer-base-url");
        }
        if !args.stun.is_empty() {
            bail!("--web-transfer-stun requires --web-transfer-base-url");
        }
        macro_rules! check_default {
            ($field:ident, $flag:literal) => {
                if args.$field != defaults.$field {
                    bail!(concat!($flag, " requires --web-transfer-base-url"));
                }
            };
        }
        check_default!(max_rooms, "--web-transfer-max-rooms");
        check_default!(max_peers_global, "--web-transfer-max-peers");
        check_default!(max_peers_per_room, "--web-transfer-max-peers-per-room");
        check_default!(max_offers_per_peer, "--web-transfer-max-offers-per-peer");
        check_default!(
            max_entries_per_offer,
            "--web-transfer-max-entries-per-offer"
        );
        check_default!(max_offer_bytes, "--web-transfer-max-offer-bytes");
        check_default!(
            max_metadata_per_room_bytes,
            "--web-transfer-max-metadata-per-room"
        );
        check_default!(
            max_metadata_total_bytes,
            "--web-transfer-max-metadata-total"
        );
        check_default!(
            max_transfers_per_peer,
            "--web-transfer-max-transfers-per-peer"
        );
        check_default!(max_relays_global, "--web-transfer-max-relays");
        check_default!(relay_rate_bytes_per_s, "--web-transfer-relay-rate");
        check_default!(owner_grace_secs, "--web-transfer-owner-grace");
        return Ok(None);
    };
    if args.no_stun && !args.stun.is_empty() {
        bail!("--web-transfer-no-stun conflicts with --web-transfer-stun");
    }
    let base_url = WebTransferBaseUrl::parse(base_url_raw)?;
    let limits = WebTransferLimits {
        max_rooms: args.max_rooms,
        max_peers_global: args.max_peers_global,
        max_peers_per_room: args.max_peers_per_room,
        max_offers_per_peer: args.max_offers_per_peer,
        max_entries_per_offer: args.max_entries_per_offer,
        max_offer_bytes: args.max_offer_bytes,
        max_metadata_per_room_bytes: args.max_metadata_per_room_bytes,
        max_metadata_total_bytes: args.max_metadata_total_bytes,
        max_transfers_per_peer: args.max_transfers_per_peer,
        max_relays_global: args.max_relays_global,
        relay_rate_bytes_per_s: args.relay_rate_bytes_per_s,
        owner_grace_secs: args.owner_grace_secs,
    };
    limits.validate()?;
    // Derived server STUN uses the base-URL host (the address browsers reach)
    // with the control UDP port. Keep IPv6 brackets so `host:port` stays
    // parseable; strip an explicit port otherwise.
    let authority = base_url.authority();
    let host_part = if let Some(rest) = authority.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => &authority[..end + 2],
            None => authority,
        }
    } else if authority.matches(':').count() == 1 {
        authority.split(':').next().unwrap_or(authority)
    } else {
        authority
    };
    let ice = IceServerConfig::resolve(
        args.no_stun,
        &args.stun,
        server_udp,
        host_part,
        control_port,
    )?;
    Ok(Some(WebTransferConfig::new(base_url, limits, ice)?))
}

// Owner lease lifecycle: detach/resume/expiry/destroy (Phase 1.2).
//
// Drop detaches (schedules grace expiry), never destroys. Destruction is
// explicit (`close_explicit`), by grace expiry, or not at all.
impl WebTransferRoom {
    /// Grace for abnormal owner loss, from the room's immutable limits.
    pub fn owner_grace(&self) -> Duration {
        Duration::from_secs(self.limits.owner_grace_secs)
    }

    /// True once `destroy` ran (idempotence gate for racing close/timeout).
    pub fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::Relaxed)
    }

    /// Destroys the room exactly once: broadcasts the opaque reason, then
    /// cancels waiters. The registry entry is removed separately via
    /// `remove_room_if_current` so only the current holder destroys.
    pub fn destroy(&self, reason: &'static str) {
        if self.destroyed.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = self.events.send(RoomEvent::RoomClosed { reason });
        self.cancel.cancel();
    }

    /// Removes exactly `peer_id` from THIS room: its presence record, every
    /// offer it owns (attributed since Phase 2.3) and every live transfer it
    /// takes part in (cancelled as the departed peer), each removal with its
    /// own revisioned removal broadcast — offers first, peer last, so
    /// receivers never see an offer of a departed peer. Transfer cancels go
    /// direct to the other party. If the complete revision range cannot be
    /// represented, cleanup still runs, the exhausted room is removed and
    /// destroyed, and subscribers receive one opaque room-close event
    /// instead of a wrapped incremental revision.
    /// Releases room and global metadata per offer. Called by `PeerGuard::drop`;
    /// never resolves a registry key.
    pub(crate) fn remove_peer(self: &Arc<Self>, peer_id: PeerId) {
        let (events, outbox, revision_exhausted) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if !state.peers.contains_key(&peer_id) {
                return;
            }
            let owned: Vec<OfferId> = state
                .offers
                .iter()
                .filter(|(_, record)| record.owner == peer_id)
                .map(|(id, _)| *id)
                .collect();
            let incremental = owned
                .len()
                .checked_add(1)
                .and_then(|steps| checked_room_revision(state.revision, steps).ok())
                .is_some();
            state.peers.remove(&peer_id);
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.remove(&peer_id);
            }
            let inner = self.registry.upgrade();
            let mut events = Vec::new();
            let mut outbox = Vec::new();
            let mut revision = state.revision;
            for offer in owned {
                if let Some(record) = state.offers.remove(&offer) {
                    state.metadata_bytes =
                        state.metadata_bytes.saturating_sub(record.metadata_bytes);
                    if let Some(registry) = &inner {
                        registry
                            .metadata_current
                            .fetch_sub(record.metadata_bytes, Ordering::Relaxed);
                        registry.offers_current.fetch_sub(1, Ordering::Relaxed);
                    }
                    if incremental {
                        revision = checked_room_revision(revision, 1)
                            .expect("revision range was checked before cleanup");
                        events.push(RoomEvent::OfferRemoved {
                            peer: peer_id,
                            offer,
                            revision,
                        });
                    }
                }
            }
            cancel_where_locked(
                inner.as_deref(),
                &mut state,
                peer_id,
                |transfer| transfer.source == peer_id || transfer.recipient == peer_id,
                &mut outbox,
            );
            if incremental {
                revision = checked_room_revision(revision, 1)
                    .expect("revision range was checked before cleanup");
                state.revision = revision;
                events.push(RoomEvent::PeerLeft {
                    peer: peer_id,
                    revision,
                });
            }
            (events, outbox, !incremental)
        };
        for (peer, message) in outbox {
            let _ = self.send_to(peer, message);
        }
        if revision_exhausted {
            if let Some(registry) = self.registry.upgrade() {
                let holder = WebTransferRegistry { inner: registry };
                holder.remove_room_if_current(self.id, self);
            }
            self.destroy("revision-exhausted");
            return;
        }
        for event in events {
            let _ = self.events.send(event);
        }
    }
}

/// Owner lease: holds the room, its ID and lease epoch. Dropping detaches
/// (grace expiry follows); only `close_explicit` destroys immediately.
#[derive(Debug)]
pub struct OwnerLease {
    room: Option<Arc<WebTransferRoom>>,
    id: RoomId,
    epoch: u64,
    closed: bool,
}

impl OwnerLease {
    /// Creates a room and takes its initial lease (epoch 0, attached).
    pub fn create(
        registry: &WebTransferRegistry,
        member_hash: [u8; 32],
        owner_hash: [u8; 32],
    ) -> Result<Self, WebTransferError> {
        let room = registry.create_room(member_hash, owner_hash)?;
        Ok(Self {
            id: room.id,
            epoch: 0,
            room: Some(room),
            closed: false,
        })
    }

    /// Room this lease holds.
    pub fn room(&self) -> &Arc<WebTransferRoom> {
        self.room
            .as_ref()
            .expect("lease holds its room until close")
    }

    /// Room ID.
    pub fn id(&self) -> RoomId {
        self.id
    }

    /// Lease epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Explicit clean close: destroys immediately, exactly once. Consumes the
    /// lease so `Drop` cannot detach afterwards.
    pub fn close_explicit(mut self, registry: &WebTransferRegistry) {
        if let Some(room) = self.room.take() {
            self.closed = true;
            if registry.remove_room_if_current(self.id, &room) {
                room.destroy("owner-close");
            } else {
                // Already gone (raced expiry won): still destroy our own Arc
                // exactly once for waiter wakeup; the winner's destroy ran.
                room.destroy("owner-close");
            }
        }
    }

    /// Resumes a detached room with the owner token: constant-time hash
    /// compare, attached rooms rejected as `UNAUTHORIZED` (same code as a
    /// bad token, revealing nothing), epoch bumped with overflow check.
    /// The old expiry dies logically: its epoch no longer matches.
    pub fn resume(
        registry: &WebTransferRegistry,
        room_id: RoomId,
        owner_token: &OwnerToken,
    ) -> Result<Self, WebTransferError> {
        // Generic message everywhere: unknown room, bad token and attached
        // room are indistinguishable to the caller.
        let room = registry
            .room(room_id)
            .ok_or_else(|| WebTransferError::unauthorized("room unavailable"))?;
        let presented = owner_token.sha256_hash();
        let matched = room
            .state
            .lock()
            .map(|state| token_digests_equal(&state.owner_hash, &presented))
            .unwrap_or(false);
        if !matched {
            return Err(WebTransferError::unauthorized("room unavailable"));
        }
        let epoch = {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            match state.owner {
                OwnerState::Attached { .. } => {
                    return Err(WebTransferError::unauthorized("room unavailable"));
                }
                OwnerState::Detached { epoch, .. } => {
                    let next = epoch
                        .checked_add(1)
                        .ok_or_else(|| WebTransferError::internal("owner epoch overflow"))?;
                    state.owner = OwnerState::Attached {
                        epoch: next,
                        last_recv: Instant::now(),
                    };
                    room.epoch.store(next, Ordering::Relaxed);
                    next
                }
            }
        };
        Ok(Self {
            id: room_id,
            epoch,
            room: Some(room),
            closed: false,
        })
    }

    /// Detaches with the room's configured grace (production path; `Drop`).
    pub fn detach(room: &Arc<WebTransferRoom>) {
        let grace = room.owner_grace();
        Self::detach_with_grace(room, grace);
    }

    /// Detaches with an explicit grace and schedules the expiry monitor.
    /// The monitor captures only a `Weak` plus the epoch (never a strong
    /// `Arc`, which would pin the room's memory and global slot for the whole
    /// grace) and removes only on pointer identity — a resumed or reused room
    /// is never evicted. Re-detaching an already-detached same-epoch lease
    /// only re-arms the monitor (monitors are epoch-checked, so duplicates
    /// are harmless).
    pub fn detach_with_grace(room: &Arc<WebTransferRoom>, grace: Duration) {
        let epoch = room.epoch.load(Ordering::Relaxed);
        {
            let Ok(mut state) = room.state.lock() else {
                return;
            };
            match state.owner {
                OwnerState::Attached { epoch: current, .. } if current == epoch => {
                    state.owner = OwnerState::Detached {
                        epoch,
                        expires_at: Instant::now() + grace,
                    };
                }
                // Same epoch but already detached: re-arm the monitor below.
                OwnerState::Detached { epoch: current, .. } if current == epoch => {}
                // Stale caller (resumed past us, destroyed): leave it alone.
                _ => return,
            }
        }
        let weak = Arc::downgrade(room);
        let id = room.id;
        // Only the registry back-pointer is needed at fire time; resolve it
        // from the room while we still hold it.
        let registry = room.registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let Some(room) = weak.upgrade() else {
                // Every strong owner is gone; whoever removed the entry also
                // ran `destroy` (close/expiry paths always pair them).
                return;
            };
            let stale = room
                .state
                .lock()
                .map(|state| {
                    matches!(state.owner, OwnerState::Detached { epoch: e, .. } if e == epoch)
                })
                .unwrap_or(false);
            if !stale {
                return;
            }
            // P-14: act on the captured entry, never on a re-resolved key.
            match registry.upgrade() {
                Some(registry) => {
                    let holder = WebTransferRegistry { inner: registry };
                    if holder.remove_room_if_current(id, &room) {
                        room.destroy("expired");
                    }
                }
                None => room.destroy("expired"),
            }
        });
    }
}

impl Drop for OwnerLease {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if let Some(room) = self.room.take() {
            Self::detach(&room);
        }
    }
}

impl WebTransferRegistry {
    /// Test/recovery constructor: inserts a room under a chosen ID (vacant
    /// only, never overwrites). Production uses [`Self::create_room`].
    pub fn create_room_with_id(
        &self,
        member_hash: [u8; 32],
        owner_hash: [u8; 32],
        id: RoomId,
    ) -> Result<Arc<WebTransferRoom>, WebTransferError> {
        let permit = Arc::clone(&self.inner.room_permits)
            .try_acquire_owned()
            .map_err(|_| {
                self.refused(WebTransferError::limit(
                    "web-transfer room budget exhausted",
                ))
            })?;
        let room = Arc::new(WebTransferRoom {
            id,
            limits: self.inner.config.limits,
            state: std::sync::Mutex::new(RoomState {
                member_hash,
                owner_hash,
                owner: OwnerState::Attached {
                    epoch: 0,
                    last_recv: Instant::now(),
                },
                peers: HashMap::new(),
                offers: HashMap::new(),
                transfers: HashMap::new(),
                metadata_bytes: 0,
                revision: 0,
            }),
            events: broadcast::channel(256).0,
            cancel: CancellationToken::new(),
            epoch: AtomicU64::new(0),
            destroyed: AtomicBool::new(false),
            registry: Arc::downgrade(&self.inner),
            room_permit: permit,
            sessions: std::sync::Mutex::new(HashMap::new()),
            relay_throttle: std::sync::Mutex::new(RelayThrottle::new(
                self.inner.config.limits.relay_rate_bytes_per_s,
            )),
        });
        match self.inner.rooms.entry(id) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                Err(WebTransferError::invalid("room ID already in use"))
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(Arc::clone(&room));
                Ok(room)
            }
        }
    }
}

/// Owner control-loop outcome (covers the 1.3 heartbeat/reaper/close tests
/// without exposing loop internals to the server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerControlOutcome {
    /// A matching `CloseWebTransferRoom` destroyed the room.
    pub closed_explicit: bool,
    /// Heartbeats observed on this session.
    pub heartbeats: u64,
    /// The 500 ms tick reaped an idle control.
    pub timed_out: bool,
}

/// Disabled-service / upgrade error: a new client maps this to "upgrade the
/// server and set --web-transfer-base-url".
pub const WEB_TRANSFER_DISABLED_ERROR: &str =
    "web transfer is not enabled on this server: upgrade the server and set --web-transfer-base-url";

/// Dispatches the FIRST message of a native owner control stream. Version is
/// verified before any allocation; a disabled service answers the existing
/// generic protocol error so the client can map it. Returns the loop outcome.
pub async fn serve_owner_first_message<S>(
    registry: Option<std::sync::Arc<WebTransferRegistry>>,
    control: &mut crate::shared::Delimited<S>,
    msg: crate::shared::ClientMessage,
    ctrl_timeout: Duration,
) -> anyhow::Result<OwnerControlOutcome>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::shared::{ClientMessage, ServerMessage};
    use crate::web_transfer_protocol::PROTOCOL_VERSION;

    let idle = || OwnerControlOutcome {
        closed_explicit: false,
        heartbeats: 0,
        timed_out: false,
    };
    match msg {
        ClientMessage::CreateWebTransferRoom {
            version,
            member_token_hash,
            owner_token_hash,
        } => {
            if version != PROTOCOL_VERSION {
                control.send(ServerMessage::Error(format!(
                    "unsupported web-transfer version {version}: upgrade the client and server together"
                ))).await?;
                return Ok(idle());
            }
            let Some(registry) = registry else {
                control
                    .send(ServerMessage::Error(
                        WEB_TRANSFER_DISABLED_ERROR.to_string(),
                    ))
                    .await?;
                return Ok(idle());
            };
            let lease = OwnerLease::create(&registry, member_token_hash, owner_token_hash)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let (id, epoch) = (lease.id(), lease.epoch());
            let base_url = registry.config().base_url.origin().to_string();
            control
                .send(ServerMessage::WebTransferRoomCreated {
                    version: PROTOCOL_VERSION,
                    room_id: id,
                    base_url,
                    owner_epoch: epoch,
                })
                .await?;
            Ok(serve_owner_control(lease, control, ctrl_timeout).await?)
        }
        ClientMessage::ResumeWebTransferRoom {
            version,
            room_id,
            owner_token,
        } => {
            if version != PROTOCOL_VERSION {
                control.send(ServerMessage::Error(format!(
                    "unsupported web-transfer version {version}: upgrade the client and server together"
                ))).await?;
                return Ok(idle());
            }
            let Some(registry) = registry else {
                control
                    .send(ServerMessage::Error(
                        WEB_TRANSFER_DISABLED_ERROR.to_string(),
                    ))
                    .await?;
                return Ok(idle());
            };
            // Hash immediately. The raw token is `Copy`, so no drop can scrub
            // the stack; instead the value never reaches room state, logs or
            // errors — only its hash crosses this scope, and the binding ends
            // here. Callers must not retain the token past the call.
            let outcome = match OwnerLease::resume(&registry, room_id, &owner_token) {
                Ok(lease) => {
                    let (id, epoch) = (lease.id(), lease.epoch());
                    let base_url = registry.config().base_url.origin().to_string();
                    control
                        .send(ServerMessage::WebTransferRoomResumed {
                            version: PROTOCOL_VERSION,
                            room_id: id,
                            base_url,
                            owner_epoch: epoch,
                        })
                        .await?;
                    serve_owner_control(lease, control, ctrl_timeout).await?
                }
                Err(_) => {
                    control
                        .send(ServerMessage::Error("room unavailable".to_string()))
                        .await?;
                    idle()
                }
            };
            Ok(outcome)
        }
        ClientMessage::CloseWebTransferRoom {
            room_id,
            owner_epoch,
        } => {
            let Some(registry) = registry else {
                control
                    .send(ServerMessage::Error(
                        WEB_TRANSFER_DISABLED_ERROR.to_string(),
                    ))
                    .await?;
                return Ok(idle());
            };
            // Idempotent close: a missing room is already gone (success).
            // A present room must match the live epoch or nothing happens.
            if let Some(room) = registry.room(room_id) {
                let current = room.epoch.load(Ordering::Relaxed);
                if current == owner_epoch && registry.remove_room_if_current(room_id, &room) {
                    room.destroy("owner-close");
                }
            }
            Ok(idle())
        }
        // Anything else as a first owner message terminates the stream.
        _ => Ok(idle()),
    }
}

/// Runs one owner control session: `Heartbeat` refreshes `last_recv`, a
/// matching close destroys, anything else terminates the loop, EOF/detach
/// unwinds through the lease `Drop`. Liveness is checked on the 500 ms tick
/// against `last_recv` — never `timeout(recv)` — and every heartbeat send
/// elsewhere uses the bounded `beat_once` shape.
pub async fn serve_owner_control<S>(
    lease: OwnerLease,
    control: &mut crate::shared::Delimited<S>,
    ctrl_timeout: Duration,
) -> anyhow::Result<OwnerControlOutcome>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::shared::ClientMessage;

    let mut last_recv = Instant::now();
    let mut heartbeats = 0u64;
    let mut tick = tokio::time::interval(WEB_TRANSFER_REAPER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if last_recv.elapsed() >= ctrl_timeout {
                    return Ok(OwnerControlOutcome {
                        closed_explicit: false,
                        heartbeats,
                        timed_out: true,
                    });
                }
            }
            msg = control.recv::<ClientMessage>() => {
                match msg? {
                    None => {
                        return Ok(OwnerControlOutcome {
                            closed_explicit: false,
                            heartbeats,
                            timed_out: false,
                        });
                    }
                    Some(ClientMessage::Heartbeat) => {
                        last_recv = Instant::now();
                        heartbeats += 1;
                    }
                    Some(ClientMessage::CloseWebTransferRoom { room_id, owner_epoch }) => {
                        if room_id == lease.id() && owner_epoch == lease.epoch() {
                            if let Some(registry) = lease.room().registry.upgrade() {
                                let holder = WebTransferRegistry { inner: registry };
                                lease.close_explicit(&holder);
                            }
                            return Ok(OwnerControlOutcome {
                                closed_explicit: true,
                                heartbeats,
                                timed_out: false,
                            });
                        }
                        return Ok(OwnerControlOutcome {
                            closed_explicit: false,
                            heartbeats,
                            timed_out: false,
                        });
                    }
                    Some(_) => {
                        return Ok(OwnerControlOutcome {
                            closed_explicit: false,
                            heartbeats,
                            timed_out: false,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod server_config_tests {
    use super::*;

    fn enabled_args() -> WebTransferServerArgs {
        WebTransferServerArgs {
            base_url: Some("http://127.0.0.1:8080/".to_string()),
            ..WebTransferServerArgs::default()
        }
    }

    #[test]
    fn server_args_web_transfer_defaults_are_exact() {
        let args = WebTransferServerArgs::default();
        let limits = WebTransferLimits::default();
        assert_eq!(args.base_url, None);
        assert!(args.stun.is_empty());
        assert!(!args.no_stun);
        assert_eq!(args.max_rooms, limits.max_rooms);
        assert_eq!(args.max_peers_global, limits.max_peers_global);
        assert_eq!(args.max_peers_per_room, limits.max_peers_per_room);
        assert_eq!(args.max_offers_per_peer, limits.max_offers_per_peer);
        assert_eq!(args.max_entries_per_offer, limits.max_entries_per_offer);
        assert_eq!(args.max_offer_bytes, limits.max_offer_bytes);
        assert_eq!(
            args.max_metadata_per_room_bytes,
            limits.max_metadata_per_room_bytes
        );
        assert_eq!(
            args.max_metadata_total_bytes,
            limits.max_metadata_total_bytes
        );
        assert_eq!(args.max_transfers_per_peer, limits.max_transfers_per_peer);
        assert_eq!(args.max_relays_global, limits.max_relays_global);
        assert_eq!(args.relay_rate_bytes_per_s, limits.relay_rate_bytes_per_s);
        assert_eq!(args.owner_grace_secs, limits.owner_grace_secs);
        // Disabled by default: no registry, no error.
        assert!(resolve_server_config(&args, false, 7835).unwrap().is_none());
    }

    #[test]
    fn web_transfer_flags_require_base_url() {
        let args = WebTransferServerArgs {
            max_rooms: 8,
            ..WebTransferServerArgs::default()
        };
        let err = resolve_server_config(&args, false, 7835).unwrap_err();
        assert!(
            err.to_string().contains("--web-transfer-max-rooms"),
            "{err}"
        );
        let args = WebTransferServerArgs {
            no_stun: true,
            ..WebTransferServerArgs::default()
        };
        let err = resolve_server_config(&args, false, 7835).unwrap_err();
        assert!(err.to_string().contains("--web-transfer-no-stun"), "{err}");
        let args = WebTransferServerArgs {
            stun: vec!["stun:stun.example.com:3478".to_string()],
            ..WebTransferServerArgs::default()
        };
        let err = resolve_server_config(&args, false, 7835).unwrap_err();
        assert!(err.to_string().contains("--web-transfer-stun"), "{err}");
        // With a base URL the same values resolve.
        let args = WebTransferServerArgs {
            base_url: Some("https://files.example.com".to_string()),
            max_rooms: 8,
            ..WebTransferServerArgs::default()
        };
        let config = resolve_server_config(&args, false, 7835).unwrap().unwrap();
        assert_eq!(config.limits.max_rooms, 8);
    }

    #[test]
    fn stun_and_no_stun_conflict() {
        let args = WebTransferServerArgs {
            base_url: Some("https://files.example.com".to_string()),
            no_stun: true,
            stun: vec!["stun:stun.example.com:3478".to_string()],
            ..WebTransferServerArgs::default()
        };
        assert!(resolve_server_config(&args, false, 7835).is_err());
        assert!(parse_stun_list(&["turn:example.com:3478".to_string()]).is_err());
        assert!(parse_stun_list(&["example.com".to_string()]).is_err());
        assert!(parse_stun_list(&["stun:".to_string()]).is_err());
        assert!(parse_stun_list(&["stun:host:0".to_string()]).is_err());
        assert!(parse_stun_list(&["stun:host:99999".to_string()]).is_err());
        // Trims delimiter whitespace, dedups preserving first occurrence.
        let list = parse_stun_list(&[
            " stun:a.example:3478 ,stun:b.example ".to_string(),
            "stun:a.example:3478".to_string(),
        ])
        .unwrap();
        assert_eq!(
            list,
            vec![
                "stun:a.example:3478".to_string(),
                "stun:b.example".to_string()
            ]
        );
    }

    #[test]
    fn all_caps_validate_before_listener_bind() {
        for grace in [4, 601] {
            let args = WebTransferServerArgs {
                base_url: Some("https://files.example.com".to_string()),
                owner_grace_secs: grace,
                ..WebTransferServerArgs::default()
            };
            assert!(
                resolve_server_config(&args, false, 7835).is_err(),
                "grace {grace}"
            );
        }
        let args = WebTransferServerArgs {
            base_url: Some("https://files.example.com".to_string()),
            max_rooms: 0,
            ..WebTransferServerArgs::default()
        };
        assert!(resolve_server_config(&args, false, 7835).is_err());
        let args = WebTransferServerArgs {
            base_url: Some("https://files.example.com".to_string()),
            max_metadata_total_bytes: 16777215,
            ..WebTransferServerArgs::default()
        };
        assert!(resolve_server_config(&args, false, 7835).is_err());
        assert!(resolve_server_config(&enabled_args(), false, 7835)
            .unwrap()
            .is_some());
    }

    fn tiny_config() -> WebTransferConfig {
        let limits = WebTransferLimits {
            max_rooms: 2,
            max_peers_global: 2,
            max_peers_per_room: 1,
            max_metadata_per_room_bytes: 1024,
            max_metadata_total_bytes: 1024,
            ..WebTransferLimits::default()
        };
        WebTransferConfig::new(
            WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
            limits,
            IceServerConfig {
                servers: Vec::new(),
            },
        )
        .unwrap()
    }

    fn room_id(n: u8) -> RoomId {
        RoomId::from_bytes([n; 16])
    }

    fn peer_id(n: u8) -> PeerId {
        PeerId::from_bytes([n; 16])
    }

    #[test]
    fn registry_global_and_room_admission_rolls_back_on_failure() {
        let registry = WebTransferRegistry::new(tiny_config()).unwrap();
        let room_a = registry.create_room([1u8; 32], [2u8; 32]).unwrap();
        let room_b = registry.create_room([3u8; 32], [4u8; 32]).unwrap();
        assert!(registry.create_room([5u8; 32], [6u8; 32]).is_err());
        assert_eq!(registry.current_rooms(), 2);

        let _guard_a = registry.join_peer(&room_a, peer_id(1), None).unwrap();
        assert_eq!(registry.current_peers(), 1);
        // Room is full (per-room cap 1): the global slot must roll back, so a
        // join into the OTHER room still succeeds afterwards.
        let err = registry.join_peer(&room_a, peer_id(2), None).unwrap_err();
        assert_eq!(err.code(), "LIMIT_EXCEEDED");
        let guard_b = registry.join_peer(&room_b, peer_id(2), None).unwrap();
        assert_eq!(registry.current_peers(), 2);
        // Duplicate join is rejected without consuming a slot.
        assert!(registry.join_peer(&room_b, peer_id(2), None).is_err());
        drop(guard_b);
        assert_eq!(registry.current_peers(), 1);
        drop(_guard_a);
        assert_eq!(registry.current_peers(), 0);
        let _ = room_id(0);
    }

    #[test]
    fn configured_totals_do_not_change_after_permit_acquisition() {
        let registry = WebTransferRegistry::new(tiny_config()).unwrap();
        let before = registry.totals();
        let room = registry.create_room([1u8; 32], [2u8; 32]).unwrap();
        let _peer = registry
            .join_peer(&room, peer_id(9), Some("Ada".to_string()))
            .unwrap();
        let _meta = registry.try_reserve_metadata(512).unwrap();
        assert_eq!(registry.totals(), before);
        assert_eq!(registry.current_rooms(), 1);
        assert_eq!(registry.current_peers(), 1);
        assert_eq!(registry.current_metadata_bytes(), 512);
        assert!(registry.try_reserve_metadata(513).is_err());
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::time::Duration;

    fn test_config() -> WebTransferConfig {
        WebTransferConfig::new(
            WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
            WebTransferLimits::default(),
            IceServerConfig {
                servers: Vec::new(),
            },
        )
        .unwrap()
    }

    fn registry() -> WebTransferRegistry {
        WebTransferRegistry::new(test_config()).unwrap()
    }

    const MEMBER_HASH: [u8; 32] = [11u8; 32];
    const OWNER_HASH: [u8; 32] = [22u8; 32];

    fn owner_token(byte: u8) -> OwnerToken {
        OwnerToken::from_bytes([byte; 32])
    }

    fn owner_hash(byte: u8) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::new()
            .chain_update([byte; 32])
            .finalize()
            .into()
    }

    #[test]
    fn room_creation_reserves_and_drop_releases_global_slot() {
        let limits = WebTransferLimits {
            max_rooms: 1,
            ..WebTransferLimits::default()
        };
        let config = WebTransferConfig::new(
            WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
            limits,
            IceServerConfig {
                servers: Vec::new(),
            },
        )
        .unwrap();
        let registry = WebTransferRegistry::new(config).unwrap();
        let room = registry.create_room(MEMBER_HASH, OWNER_HASH).unwrap();
        assert_eq!(registry.current_rooms(), 1);
        assert!(registry.create_room(MEMBER_HASH, OWNER_HASH).is_err());
        let id = room.id;
        assert!(registry.remove_room_if_current(id, &room));
        assert!(!registry.remove_room_if_current(id, &room));
        drop(room);
        // Slot released only after every Arc is gone: creation works again.
        let room2 = registry.create_room(MEMBER_HASH, OWNER_HASH).unwrap();
        assert_eq!(registry.current_rooms(), 1);
        let _ = room2;
    }

    #[test]
    fn room_id_collision_never_overwrites_existing_room() {
        let registry = registry();
        let id = RoomId::from_bytes([77u8; 16]);
        let first = registry
            .create_room_with_id(MEMBER_HASH, OWNER_HASH, id)
            .unwrap();
        let err = registry
            .create_room_with_id([9u8; 32], [9u8; 32], id)
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_MESSAGE");
        let current = registry.room(id).unwrap();
        assert!(Arc::ptr_eq(&current, &first));
        let _ = current;
    }

    #[tokio::test]
    async fn owner_guard_drop_detaches_instead_of_destroying() {
        let registry = registry();
        let lease = OwnerLease::create(&registry, MEMBER_HASH, OWNER_HASH).unwrap();
        let id = lease.id();
        drop(lease);
        // Still registered, now detached — never destroyed by a drop.
        let room = registry.room(id).expect("drop detaches, not destroys");
        assert!(!room.is_destroyed());
        let state = room.state.lock().unwrap();
        assert!(matches!(state.owner, OwnerState::Detached { epoch: 0, .. }));
    }

    #[test]
    fn explicit_close_destroys_once() {
        let registry = registry();
        let lease = OwnerLease::create(&registry, MEMBER_HASH, OWNER_HASH).unwrap();
        let id = lease.id();
        let room = lease.room().clone();
        let mut events = room.events.subscribe();
        lease.close_explicit(&registry);
        assert!(registry.room(id).is_none());
        assert!(room.is_destroyed());
        assert!(matches!(
            events.try_recv(),
            Ok(RoomEvent::RoomClosed {
                reason: "owner-close"
            })
        ));
        // Second destroy is a no-op (idempotent under close/timeout races).
        room.destroy("expired");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn resume_requires_matching_token_and_detached_state() {
        let registry = registry();
        let token = owner_token(5);
        let lease = OwnerLease::create(&registry, MEMBER_HASH, owner_hash(5)).unwrap();
        let id = lease.id();
        // Attached: even the right token is UNAUTHORIZED.
        assert_eq!(
            OwnerLease::resume(&registry, id, &token)
                .unwrap_err()
                .code(),
            "UNAUTHORIZED"
        );
        // Unknown room: same code, nothing revealed.
        let ghost = RoomId::from_bytes([99u8; 16]);
        assert_eq!(
            OwnerLease::resume(&registry, ghost, &token)
                .unwrap_err()
                .code(),
            "UNAUTHORIZED"
        );
        drop(lease);
        // Detached with the wrong token: same code.
        assert_eq!(
            OwnerLease::resume(&registry, id, &owner_token(6))
                .unwrap_err()
                .code(),
            "UNAUTHORIZED"
        );
        // Right token while detached: attaches.
        let resumed = OwnerLease::resume(&registry, id, &token).unwrap();
        assert_eq!(resumed.epoch(), 1);
    }

    #[tokio::test]
    async fn resume_increments_epoch() {
        let registry = registry();
        let token = owner_token(5);
        let lease = OwnerLease::create(&registry, MEMBER_HASH, owner_hash(5)).unwrap();
        let id = lease.id();
        drop(lease);
        let first = OwnerLease::resume(&registry, id, &token).unwrap();
        assert_eq!(first.epoch(), 1);
        drop(first);
        let second = OwnerLease::resume(&registry, id, &token).unwrap();
        assert_eq!(second.epoch(), 2);
    }

    #[tokio::test]
    async fn stale_expiry_cannot_destroy_resumed_room() {
        let registry = registry();
        let token = owner_token(5);
        let lease = OwnerLease::create(&registry, MEMBER_HASH, owner_hash(5)).unwrap();
        let id = lease.id();
        let room = lease.room().clone();
        drop(lease);
        // First detach schedules a 60 ms monitor; resume before it fires.
        OwnerLease::detach_with_grace(&room, Duration::from_millis(60));
        let resumed = OwnerLease::resume(&registry, id, &token).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(registry.room(id).is_some(), "stale monitor must not evict");
        assert!(!room.is_destroyed());
        drop(resumed);
    }

    #[tokio::test]
    async fn stale_expiry_cannot_destroy_reused_room_id() {
        let registry = registry();
        let id = RoomId::from_bytes([55u8; 16]);
        let room = registry
            .create_room_with_id(MEMBER_HASH, OWNER_HASH, id)
            .unwrap();
        OwnerLease::detach_with_grace(&room, Duration::from_millis(60));
        // Recreate under the same ID before the old monitor fires.
        assert!(registry.remove_room_if_current(id, &room));
        room.destroy("owner-close");
        let room2 = registry
            .create_room_with_id([9u8; 32], [9u8; 32], id)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let current = registry
            .room(id)
            .expect("reused room survives the stale monitor");
        assert!(Arc::ptr_eq(&current, &room2));
        assert!(!room2.is_destroyed());
    }

    #[tokio::test]
    async fn expiry_releases_all_counters_and_cancels_waiters() {
        let registry = registry();
        let lease = OwnerLease::create(&registry, MEMBER_HASH, OWNER_HASH).unwrap();
        let room = lease.room().clone();
        let mut events = room.events.subscribe();
        drop(lease);
        OwnerLease::detach_with_grace(&room, Duration::from_millis(30));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(registry.room(room.id).is_none());
        assert!(room.is_destroyed());
        assert!(room.cancel.is_cancelled());
        assert!(matches!(
            events.try_recv(),
            Ok(RoomEvent::RoomClosed { reason: "expired" })
        ));
        // The global slot frees when the last Arc drops (RAII, like the
        // admin `Registration` precedent) — not while a guard still holds it.
        drop(room);
        assert_eq!(registry.current_rooms(), 0);
    }

    #[tokio::test]
    async fn concurrent_close_resume_timeout_has_one_terminal_winner() {
        use tokio::sync::Barrier;
        for _ in 0..25 {
            let registry = registry();
            let token = owner_token(5);
            let lease = OwnerLease::create(&registry, MEMBER_HASH, owner_hash(5)).unwrap();
            let id = lease.id();
            let room = lease.room().clone();
            drop(lease);
            let barrier = Arc::new(Barrier::new(4));
            let spawn = |f: fn(
                WebTransferRegistry,
                Arc<WebTransferRoom>,
                RoomId,
                OwnerToken,
                Arc<Barrier>,
            )| {
                let (registry, room, barrier) = (registry.clone(), room.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    f(registry, room, id, token, barrier);
                })
            };
            // Plain function pointers keep the harness sync: the race is on
            // the mutex/epoch/remove_if, none of which needs .await.
            fn do_close(
                r: WebTransferRegistry,
                room: Arc<WebTransferRoom>,
                id: RoomId,
                _: OwnerToken,
                _: Arc<Barrier>,
            ) {
                if r.remove_room_if_current(id, &room) {
                    room.destroy("owner-close");
                }
            }
            fn do_resume(
                r: WebTransferRegistry,
                _: Arc<WebTransferRoom>,
                id: RoomId,
                t: OwnerToken,
                _: Arc<Barrier>,
            ) {
                let _ = OwnerLease::resume(&r, id, &t);
            }
            fn do_expire(
                r: WebTransferRegistry,
                room: Arc<WebTransferRoom>,
                id: RoomId,
                _: OwnerToken,
                _: Arc<Barrier>,
            ) {
                if r.remove_room_if_current(id, &room) {
                    room.destroy("expired");
                }
            }
            let h1 = spawn(do_close);
            let h2 = spawn(do_resume);
            let h3 = spawn(do_expire);
            let h4 = spawn(do_resume);
            let _ = tokio::join!(h1, h2, h3, h4);
            drop(room);
            // Drive whatever the race left behind to a terminal state, then
            // prove convergence: exactly one destroy, counters back to zero.
            match registry.room(id) {
                Some(current) => {
                    // Whoever won the attach race left a coherent lease behind
                    // (a resumed-then-dropped lease detaches, never destroys).
                    let state = current.state.lock().unwrap();
                    let coherent = matches!(
                        state.owner,
                        OwnerState::Attached { .. } | OwnerState::Detached { .. }
                    );
                    assert!(coherent, "present room holds a coherent lease");
                    drop(state);
                    assert!(registry.remove_room_if_current(id, &current));
                    current.destroy("test-close");
                    assert!(!registry.remove_room_if_current(id, &current));
                    drop(current);
                }
                None => assert_eq!(registry.current_rooms(), 0),
            }
            assert_eq!(registry.current_rooms(), 0);
            // Resume handles for the won-over case must not linger detached:
            // dropping them only detaches, never destroys (covered above).
        }
    }
}

#[cfg(test)]
mod owner_control_tests {
    use super::*;
    use crate::shared::{ClientMessage, Delimited, ServerMessage};

    fn test_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits::default(),
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn owner_pair() -> (OwnerToken, [u8; 32], [u8; 32]) {
        let member = MemberToken::from_bytes([0x11u8; 32]);
        let owner = OwnerToken::from_bytes([0x22u8; 32]);
        (owner, member.sha256_hash(), owner.sha256_hash())
    }

    async fn duplex_pair() -> (
        Delimited<tokio::io::DuplexStream>,
        Delimited<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(65536);
        (Delimited::new(a), Delimited::new(b))
    }

    #[tokio::test]
    async fn create_rejects_disabled_service_and_wrong_version_without_allocating() {
        let (owner, member_hash, owner_hash) = owner_pair();
        let _ = owner;
        // Disabled service: generic error, no room.
        let (mut client, mut server) = duplex_pair().await;
        let outcome = serve_owner_first_message(
            None,
            &mut server,
            ClientMessage::CreateWebTransferRoom {
                version: 1,
                member_token_hash: member_hash,
                owner_token_hash: owner_hash,
            },
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            OwnerControlOutcome {
                closed_explicit: false,
                heartbeats: 0,
                timed_out: false
            }
        );
        assert!(matches!(
            client.recv::<ServerMessage>().await.unwrap(),
            Some(ServerMessage::Error(_))
        ));
        // Wrong version: rejected before any allocation.
        let registry = test_registry();
        let (mut client, mut server) = duplex_pair().await;
        serve_owner_first_message(
            Some(Arc::new(registry.clone())),
            &mut server,
            ClientMessage::CreateWebTransferRoom {
                version: 2,
                member_token_hash: member_hash,
                owner_token_hash: owner_hash,
            },
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(matches!(
            client.recv::<ServerMessage>().await.unwrap(),
            Some(ServerMessage::Error(_))
        ));
        assert_eq!(registry.current_rooms(), 0);
    }

    #[tokio::test]
    async fn create_response_never_contains_token_or_key() {
        let (owner, member_hash, owner_hash) = owner_pair();
        let member_hex = MemberToken::from_bytes([0x11u8; 32]).to_string();
        let owner_hex = owner.to_string();
        let registry = test_registry();
        let (mut client, mut server) = duplex_pair().await;
        let server_task = tokio::spawn(async move {
            serve_owner_first_message(
                Some(Arc::new(registry)),
                &mut server,
                ClientMessage::CreateWebTransferRoom {
                    version: 1,
                    member_token_hash: member_hash,
                    owner_token_hash: owner_hash,
                },
                Duration::from_secs(60),
            )
            .await
        });
        let reply = client.recv::<ServerMessage>().await.unwrap();
        let json = serde_json::to_string(&reply).unwrap();
        assert!(!json.contains(&member_hex), "member token leaks: {json}");
        assert!(!json.contains(&owner_hex), "owner token leaks: {json}");
        assert!(
            !json.contains('#'),
            "fragment must never ride the wire: {json}"
        );
        assert!(matches!(
            reply,
            Some(ServerMessage::WebTransferRoomCreated { .. })
        ));
        drop(client);
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn resume_zeroizes_or_drops_raw_owner_token_after_hash_scope() {
        let (owner, member_hash, owner_hash) = owner_pair();
        let owner_hex = owner.to_string();
        let registry = test_registry();
        let lease = OwnerLease::create(&registry, member_hash, owner_hash).unwrap();
        let id = lease.id();
        let room = lease.room().clone();
        drop(lease);
        // Server-side state debug never carries the raw token...
        let state_debug = format!("{:?}", room.state.lock().unwrap());
        assert!(
            !state_debug.contains(&owner_hex),
            "token in state: {state_debug}"
        );
        // ...and a resumed session reports no token either.
        let (mut client, mut server) = duplex_pair().await;
        let server_task = tokio::spawn(async move {
            serve_owner_first_message(
                Some(Arc::new(registry)),
                &mut server,
                ClientMessage::ResumeWebTransferRoom {
                    version: 1,
                    room_id: id,
                    owner_token: owner,
                },
                Duration::from_secs(60),
            )
            .await
        });
        let reply = client.recv::<ServerMessage>().await.unwrap();
        let json = serde_json::to_string(&reply).unwrap();
        assert!(!json.contains(&owner_hex), "token in reply: {json}");
        drop(client);
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn owner_heartbeat_updates_last_recv() {
        let (owner, member_hash, owner_hash) = owner_pair();
        let _ = owner;
        let registry = test_registry();
        let (mut client, mut server) = duplex_pair().await;
        let server_task = tokio::spawn(async move {
            serve_owner_first_message(
                Some(Arc::new(registry)),
                &mut server,
                ClientMessage::CreateWebTransferRoom {
                    version: 1,
                    member_token_hash: member_hash,
                    owner_token_hash: owner_hash,
                },
                Duration::from_millis(300),
            )
            .await
        });
        let created = client.recv::<ServerMessage>().await.unwrap();
        let (id, epoch) = match created {
            Some(ServerMessage::WebTransferRoomCreated {
                room_id,
                owner_epoch,
                ..
            }) => (room_id, owner_epoch),
            other => panic!("expected Created, got {other:?}"),
        };
        // Heartbeats keep a short-timeout session alive across several ticks.
        for _ in 0..4 {
            client.send(ClientMessage::Heartbeat).await.unwrap();
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        client
            .send(ClientMessage::CloseWebTransferRoom {
                room_id: id,
                owner_epoch: epoch,
            })
            .await
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("heartbeats must keep the loop responsive")
            .unwrap()
            .unwrap();
        assert_eq!(outcome.heartbeats, 4);
        assert!(outcome.closed_explicit);
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn owner_reaper_checks_on_tick_not_timeout_recv() {
        let registry = test_registry();
        let lease = OwnerLease::create(&registry, [1u8; 32], [2u8; 32]).unwrap();
        let (_client, mut server) = duplex_pair().await;
        let started = Instant::now();
        // Silent but OPEN control: a `timeout(recv)` implementation would park
        // here for its whole deadline, while the tick reaper fires at 50 ms.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            serve_owner_control(lease, &mut server, Duration::from_millis(50)),
        )
        .await
        .expect("reaper must fire on its tick, not on a recv deadline")
        .unwrap();
        assert!(outcome.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(_client);
    }

    #[tokio::test]
    async fn close_requires_current_room_and_epoch() {
        let (owner, member_hash, owner_hash) = owner_pair();
        let _ = owner;
        let registry = Arc::new(test_registry());
        let (mut client, mut server) = duplex_pair().await;
        let task_registry = Arc::clone(&registry);
        let server_task = tokio::spawn(async move {
            serve_owner_first_message(
                Some(task_registry),
                &mut server,
                ClientMessage::CreateWebTransferRoom {
                    version: 1,
                    member_token_hash: member_hash,
                    owner_token_hash: owner_hash,
                },
                Duration::from_secs(60),
            )
            .await
        });
        let created = client.recv::<ServerMessage>().await.unwrap();
        let (id, epoch) = match created {
            Some(ServerMessage::WebTransferRoomCreated {
                room_id,
                owner_epoch,
                ..
            }) => (room_id, owner_epoch),
            other => panic!("expected Created, got {other:?}"),
        };
        // Wrong epoch: loop ends, room survives.
        client
            .send(ClientMessage::CloseWebTransferRoom {
                room_id: id,
                owner_epoch: epoch + 1,
            })
            .await
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("loop ends")
            .unwrap()
            .unwrap();
        assert!(!outcome.closed_explicit);
        let room = registry.room(id).expect("epoch mismatch must not destroy");
        assert!(!room.is_destroyed());
        drop(client);
        drop(room);
    }

    #[tokio::test]
    async fn owner_eof_detaches() {
        let (owner, member_hash, owner_hash) = owner_pair();
        let _ = owner;
        let registry = Arc::new(test_registry());
        let (client, mut server) = duplex_pair().await;
        let task_registry = Arc::clone(&registry);
        let server_task = tokio::spawn(async move {
            serve_owner_first_message(
                Some(task_registry),
                &mut server,
                ClientMessage::CreateWebTransferRoom {
                    version: 1,
                    member_token_hash: member_hash,
                    owner_token_hash: owner_hash,
                },
                Duration::from_secs(60),
            )
            .await
        });
        // EOF before the Created reply is even read: the send fails, the
        // lease drops and detaches — the room is never destroyed by a drop.
        drop(client);
        let result = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("eof ends the session")
            .unwrap();
        assert!(
            result.is_err(),
            "broken pipe must surface, not be swallowed"
        );
        assert_eq!(registry.current_rooms(), 1);
    }
}

#[cfg(test)]
mod control_session_tests {
    use super::*;
    use std::collections::HashSet;

    fn session_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits::default(),
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn session_registry_with_limits(limits: WebTransferLimits) -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                limits,
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn member_owner() -> (MemberToken, OwnerToken, [u8; 32], [u8; 32]) {
        let member = MemberToken::from_bytes([0x31u8; 32]);
        let owner = OwnerToken::from_bytes([0x32u8; 32]);
        let member_hash = member.sha256_hash();
        let owner_hash = owner.sha256_hash();
        (member, owner, member_hash, owner_hash)
    }

    fn open_room(registry: &WebTransferRegistry) -> (OwnerLease, MemberToken, RoomId) {
        let (member, _owner, member_hash, owner_hash) = member_owner();
        let lease = OwnerLease::create(registry, member_hash, owner_hash).unwrap();
        let id = lease.id();
        (lease, member, id)
    }

    /// 6.3: every credential on this surface is either single-use or
    /// idempotent, and nothing in between.
    ///
    /// Three replays, three different right answers. A relay TICKET is
    /// single-use: presenting it twice must fail the second time, and a
    /// WRONG presentment must still burn it (otherwise a guesser gets one
    /// free role probe per ticket). A requestId REPLAY is idempotent: the
    /// same terminal response comes back and the mutation does not run twice.
    /// A member TOKEN is the room credential and is meant to be reused — by
    /// every peer, for the life of the room — so the property worth pinning
    /// is that it is bound to ITS room and dies with it.
    #[tokio::test]
    async fn token_ticket_request_replays_are_idempotent_or_rejected() {
        use crate::web_transfer_protocol::RequestId;

        let registry = session_registry();
        let (lease, member, id) = open_room(&registry);
        let now = Instant::now();

        // --- ticket: single use ------------------------------------------
        let peer = generate_peer_id();
        let ticket = generate_relay_ticket();
        let record = RelayTicketRecord {
            ticket_hash: ticket.sha256_hash(),
            transfer_id: TransferId::from_bytes([1u8; 16]),
            attempt_id: AttemptId::from_bytes([2u8; 16]),
            peer_id: peer,
            role: RelayRole::Source,
            expires_at: now + Duration::from_secs(30),
        };
        registry.inner.tickets.insert(ticket.sha256_hash(), record);
        let granted = registry
            .consume_relay_ticket(&ticket.to_string(), RelayRole::Source, peer, now)
            .expect("the first presentment is the legitimate leg");
        assert_eq!(granted.peer_id, peer);
        assert!(
            matches!(
                registry.consume_relay_ticket(&ticket.to_string(), RelayRole::Source, peer, now),
                Err(TicketDeny::Unknown)
            ),
            "a relay ticket must not work twice"
        );

        // A WRONG presentment burns the ticket too: the legitimate leg then
        // fails, which is the correct outcome — the attempt falls back — and
        // it is what stops a guesser from probing roles for free.
        let ticket2 = generate_relay_ticket();
        registry.inner.tickets.insert(
            ticket2.sha256_hash(),
            RelayTicketRecord {
                ticket_hash: ticket2.sha256_hash(),
                transfer_id: TransferId::from_bytes([1u8; 16]),
                attempt_id: AttemptId::from_bytes([2u8; 16]),
                peer_id: peer,
                role: RelayRole::Source,
                expires_at: now + Duration::from_secs(30),
            },
        );
        assert!(matches!(
            registry.consume_relay_ticket(&ticket2.to_string(), RelayRole::Recipient, peer, now),
            Err(TicketDeny::RoleMismatch)
        ));
        assert!(matches!(
            registry.consume_relay_ticket(&ticket2.to_string(), RelayRole::Source, peer, now),
            Err(TicketDeny::Unknown)
        ));

        // An expired ticket is refused and burned as well.
        let ticket3 = generate_relay_ticket();
        registry.inner.tickets.insert(
            ticket3.sha256_hash(),
            RelayTicketRecord {
                ticket_hash: ticket3.sha256_hash(),
                transfer_id: TransferId::from_bytes([1u8; 16]),
                attempt_id: AttemptId::from_bytes([2u8; 16]),
                peer_id: peer,
                role: RelayRole::Source,
                expires_at: now,
            },
        );
        assert!(matches!(
            registry.consume_relay_ticket(
                &ticket3.to_string(),
                RelayRole::Source,
                peer,
                now + Duration::from_secs(1)
            ),
            Err(TicketDeny::Expired)
        ));
        // A ticket that was never issued is `Unknown`, never a panic.
        assert!(matches!(
            registry.consume_relay_ticket("not-hex", RelayRole::Source, peer, now),
            Err(TicketDeny::Unknown)
        ));

        // --- requestId: idempotent ----------------------------------------
        let mut cache = RequestCache::default();
        let request: RequestId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        assert!(cache.get(request, now).is_none());
        cache.insert(request, "{\"type\":\"ack\"}".to_string(), now);
        assert_eq!(
            cache.get(request, now).as_deref(),
            Some("{\"type\":\"ack\"}"),
            "a replayed requestId must replay its own terminal response"
        );
        // ... and it expires, so a replay is idempotent for a bounded window
        // and not forever (the cache is 256 entries, not a log).
        assert!(cache
            .get(
                request,
                now + WEB_TRANSFER_REQUEST_CACHE_TTL + Duration::from_secs(1)
            )
            .is_none());

        // --- member token: reusable, but bound to its room ----------------
        assert!(matches!(
            authenticate_hello(&registry, id, &member),
            HelloAuth::Ok { .. }
        ));
        assert!(matches!(
            authenticate_hello(&registry, id, &member),
            HelloAuth::Ok { .. }
        ));
        // A second room with its OWN token: `open_room` mints a fixed one,
        // so the binding has to be tested against a room that really has a
        // different credential.
        let other_member = MemberToken::from_bytes([0x7au8; 32]);
        let other_owner = OwnerToken::from_bytes([0x7bu8; 32]);
        let other_lease = OwnerLease::create(
            &registry,
            other_member.sha256_hash(),
            other_owner.sha256_hash(),
        )
        .expect("a second room");
        let other_id = other_lease.id();
        assert!(
            matches!(
                authenticate_hello(&registry, other_id, &member),
                HelloAuth::Deny
            ),
            "a token must not open a room it was not minted for"
        );
        assert!(
            matches!(
                authenticate_hello(&registry, id, &other_member),
                HelloAuth::Deny
            ),
            "and the binding holds in the other direction too"
        );
        lease.close_explicit(&registry);
        // After an explicit close the room is removed from the registry, so
        // the answer is `Deny` (unknown room) rather than `Gone` (known and
        // destroyed) — which is what a peer arriving late must hear anyway.
        // The property under test is that it is never `Ok`.
        assert!(
            !matches!(
                authenticate_hello(&registry, id, &member),
                HelloAuth::Ok { .. }
            ),
            "a token must die with its room"
        );
        other_lease.close_explicit(&registry);
    }

    #[tokio::test]
    async fn peer_auth_errors_do_not_oracle_room_or_token() {
        let registry = session_registry();
        let (_lease, member, id) = open_room(&registry);
        let bad = MemberToken::from_bytes([0x33u8; 32]);
        // Absent room, bad token: the same Deny.
        assert!(matches!(
            authenticate_hello(&registry, RoomId::from_bytes([9u8; 16]), &member),
            HelloAuth::Deny
        ));
        assert!(matches!(
            authenticate_hello(&registry, id, &bad),
            HelloAuth::Deny
        ));
        // Good token on a live room authenticates.
        assert!(matches!(
            authenticate_hello(&registry, id, &member),
            HelloAuth::Ok { .. }
        ));
        // Destroyed room: the valid token hears Gone, anyone else Deny.
        let room = registry.room(id).unwrap();
        room.destroy("owner-close");
        assert!(matches!(
            authenticate_hello(&registry, id, &member),
            HelloAuth::Gone
        ));
        assert!(matches!(
            authenticate_hello(&registry, id, &bad),
            HelloAuth::Deny
        ));
    }

    #[tokio::test]
    async fn peer_ids_and_default_names_are_canonical() {
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let id = generate_peer_id();
            let hex = id.to_string();
            assert_eq!(hex.len(), 32);
            assert!(hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            assert_ne!(id, PeerId::from_bytes([0u8; 16]));
            assert!(seen.insert(id));
            let name = default_display_name(id);
            assert!(name.starts_with("Peer "));
            assert_eq!(&name[5..], &hex[28..]);
        }
    }

    #[tokio::test]
    async fn rename_normalizes_and_bounds_unicode() {
        assert_eq!(normalize_display_name("e\u{301}").unwrap(), "é");
        assert_eq!(normalize_display_name("  Bobi  ").unwrap(), "Bobi");
        assert!(normalize_display_name("").is_err());
        assert!(normalize_display_name("   ").is_err());
        assert!(normalize_display_name("a\nb").is_err());
        assert!(normalize_display_name("a\0b").is_err());
        assert!(normalize_display_name(&"x".repeat(48)).is_ok());
        assert!(normalize_display_name(&"x".repeat(49)).is_err());
        assert!(normalize_display_name(&"é".repeat(48)).is_ok());
        // Stored form is the normalized one, and the rename broadcasts.
        let registry = session_registry();
        let (_lease, _member, id) = open_room(&registry);
        let room = registry.room(id).unwrap();
        let peer = generate_peer_id();
        let _guard = registry
            .join_peer(&room, peer, Some("  Raw  ".to_string()))
            .unwrap();
        assert_eq!(
            room.state.lock().unwrap().peers[&peer]
                .display_name
                .as_deref(),
            Some("Raw")
        );
        let mut events = room.events.subscribe();
        let stored = registry.rename_peer(&room, peer, "e\u{301}xtra ").unwrap();
        assert_eq!(stored, "éxtra");
        assert_eq!(
            room.state.lock().unwrap().peers[&peer]
                .display_name
                .as_deref(),
            Some("éxtra")
        );
        match events.try_recv().unwrap() {
            RoomEvent::PeerRenamed {
                peer: got,
                display_name,
                revision,
            } => {
                assert_eq!(got, peer);
                assert_eq!(display_name, "éxtra");
                assert_eq!(revision, 2);
            }
            other => panic!("expected rename event, got {other:?}"),
        }
        assert!(registry.rename_peer(&room, peer, "").is_err());
        assert!(registry
            .rename_peer(&room, generate_peer_id(), "Nobody")
            .is_err());
        assert_eq!(room.state.lock().unwrap().revision, 2);
    }

    #[tokio::test]
    async fn peer_caps_roll_back_on_failed_auth() {
        let limits = WebTransferLimits {
            max_peers_global: 1,
            ..WebTransferLimits::default()
        };
        let registry = session_registry_with_limits(limits);
        let (_lease, _member, id) = open_room(&registry);
        let room = registry.room(id).unwrap();
        let first = generate_peer_id();
        let guard = registry.join_peer(&room, first, None).unwrap();
        assert_eq!(registry.current_peers(), 1);
        // Second join fails on the cap and releases its global permit.
        assert!(registry.join_peer(&room, generate_peer_id(), None).is_err());
        assert_eq!(registry.current_peers(), 1);
        drop(guard);
        assert_eq!(registry.current_peers(), 0);
        // Malformed names fail before any permit moves.
        assert!(registry
            .join_peer(&room, generate_peer_id(), Some("".to_string()))
            .is_err());
        assert_eq!(registry.current_peers(), 0);
    }

    #[tokio::test]
    async fn peer_guard_removes_only_captured_peer_and_releases_permits() {
        let registry = session_registry();
        let (_lease, _member, id) = open_room(&registry);
        let room = registry.room(id).unwrap();
        let first = generate_peer_id();
        let second = generate_peer_id();
        let guard_a = registry
            .join_peer(&room, first, Some("A".to_string()))
            .unwrap();
        let guard_b = registry
            .join_peer(&room, second, Some("B".to_string()))
            .unwrap();
        assert_eq!(registry.current_peers(), 2);
        let mut events = room.events.subscribe();
        drop(guard_a);
        {
            let state = room.state.lock().unwrap();
            assert!(!state.peers.contains_key(&first));
            assert_eq!(state.peers[&second].display_name.as_deref(), Some("B"));
        }
        assert_eq!(registry.current_peers(), 1);
        match events.try_recv().unwrap() {
            RoomEvent::PeerLeft { peer, .. } => assert_eq!(peer, first),
            other => panic!("expected leave event, got {other:?}"),
        }
        drop(guard_b);
        assert!(room.state.lock().unwrap().peers.is_empty());
        assert_eq!(registry.current_peers(), 0);
    }

    #[tokio::test]
    async fn stale_guard_cannot_mutate_reused_room() {
        let registry = session_registry();
        let (member, _owner, member_hash, owner_hash) = member_owner();
        let _ = member;
        let forced = RoomId::from_bytes([0xabu8; 16]);
        let first = registry
            .create_room_with_id(member_hash, owner_hash, forced)
            .unwrap();
        let peer = generate_peer_id();
        let guard = registry.join_peer(&first, peer, None).unwrap();
        assert!(registry.remove_room_if_current(forced, &first));
        first.destroy("owner-close");
        let second = registry
            .create_room_with_id([8u8; 32], [8u8; 32], forced)
            .unwrap();
        drop(guard);
        // The reused room is untouched; the stale peer died with its own Arc.
        assert!(second.state.lock().unwrap().peers.is_empty());
        assert!(!second.is_destroyed());
        assert_eq!(registry.current_peers(), 0);
    }

    #[tokio::test]
    async fn heartbeat_reaper_checks_last_recv_on_tick() {
        let now = Instant::now();
        assert!(!control_liveness_expired(now, now));
        let fresh = now.checked_sub(Duration::from_secs(59)).unwrap();
        assert!(!control_liveness_expired(fresh, now));
        let stale = now.checked_sub(Duration::from_secs(60)).unwrap();
        assert!(control_liveness_expired(stale, now));
        let older = now.checked_sub(Duration::from_secs(61)).unwrap();
        assert!(control_liveness_expired(older, now));
    }

    #[tokio::test]
    async fn blocked_send_is_bounded_and_cleans_peer() {
        let registry = session_registry();
        let (_lease, _member, id) = open_room(&registry);
        let room = registry.room(id).unwrap();
        let peer = generate_peer_id();
        let (session, mut out_rx, _initial) =
            PeerSession::establish(&registry, &room, peer, None).unwrap();
        assert_eq!(registry.current_peers(), 1);
        let tx = session.sender();
        for _ in 0..WEB_TRANSFER_OUTGOING_CAP {
            tx.try_send("queued".to_string()).unwrap();
        }
        assert!(tx.try_send("overflow".to_string()).is_err());
        drop(session);
        drop(tx);
        assert!(!room.state.lock().unwrap().peers.contains_key(&peer));
        assert_eq!(registry.current_peers(), 0);
        while out_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn request_cache_replays_exact_response_and_evicts_fifo() {
        use crate::web_transfer_protocol::RequestId;
        let now = Instant::now();
        let mut cache = RequestCache::default();
        let first: RequestId = "00000000000000000000000000000000".parse().unwrap();
        cache.insert(first, "r0".to_string(), now);
        assert_eq!(cache.get(first, now).as_deref(), Some("r0"));
        // 255 more distinct IDs fill the cache exactly; the first survives.
        for i in 1u16..=255 {
            let id = RequestId::from_bytes([i as u8; 16]);
            cache.insert(id, format!("r{i}"), now);
        }
        assert_eq!(cache.len(), 256);
        assert_eq!(cache.get(first, now).as_deref(), Some("r0"));
        // One more distinct ID evicts the oldest.
        let extra: RequestId =
            RequestId::from_bytes([0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        cache.insert(extra, "extra".to_string(), now);
        assert_eq!(cache.len(), 256);
        assert!(cache.get(first, now).is_none());
        assert_eq!(cache.get(extra, now).as_deref(), Some("extra"));
        // Re-inserting an ID replaces its response without growing.
        cache.insert(extra, "extra2".to_string(), now);
        assert_eq!(cache.get(extra, now).as_deref(), Some("extra2"));
        assert_eq!(cache.len(), 256);
    }

    #[tokio::test]
    async fn lagged_receiver_gets_ordered_snapshot_not_large_aggregate() {
        use crate::web_transfer_protocol::parse_server_envelope;
        let registry = session_registry();
        let (_lease, _member, id) = open_room(&registry);
        let room = registry.room(id).unwrap();
        let mut peers = Vec::new();
        let mut guards = Vec::new();
        for _ in 0..3 {
            let peer = generate_peer_id();
            guards.push(registry.join_peer(&room, peer, None).unwrap());
            peers.push(peer);
        }
        let mut lagged = room.events.subscribe();
        for i in 0..300 {
            registry
                .rename_peer(&room, peers[0], &format!("name-{i:03}"))
                .unwrap();
        }
        assert!(lagged.try_recv().is_err());
        let messages = build_resync(&room).unwrap();
        // Begin + 3 peers + end: 5 messages, not 300+ incremental events.
        assert_eq!(messages.len(), 5);
        let mut types = Vec::new();
        let mut revision = None;
        for message in &messages {
            let env = parse_server_envelope(message).unwrap();
            types.push(env.typ.clone());
            let rev = env.body.get("revision").and_then(|v| v.as_u64()).unwrap();
            revision = Some(rev);
        }
        assert_eq!(
            types,
            vec![
                "snapshot.begin",
                "snapshot.peer",
                "snapshot.peer",
                "snapshot.peer",
                "snapshot.end"
            ]
        );
        let revision = revision.unwrap();
        assert_eq!(room.state.lock().unwrap().revision, revision);
        let mut ids: Vec<String> = messages[1..4]
            .iter()
            .map(|m| {
                let env = parse_server_envelope(m).unwrap();
                assert_eq!(
                    env.body.get("revision").and_then(|v| v.as_u64()),
                    Some(revision)
                );
                env.body
                    .get("peerId")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_string()
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        ids.sort();
        let mut expected: Vec<String> = peers.iter().map(|p| p.to_string()).collect();
        expected.sort();
        assert_eq!(ids, expected);
        let _ = guards;
    }

    #[tokio::test]
    async fn control_rate_buckets_are_exact_and_idle_ip_entries_expire() {
        let start = Instant::now();
        let mut control = TokenBucket::new_at(
            WEB_TRANSFER_CONTROL_RATE_PER_SEC,
            WEB_TRANSFER_CONTROL_BURST,
            start,
        );
        for _ in 0..60 {
            assert!(control.take(start));
        }
        assert!(!control.take(start));
        let later = start + Duration::from_secs(1);
        for _ in 0..30 {
            assert!(control.take(later));
        }
        assert!(!control.take(later));
        let mut mutation = TokenBucket::new_at(
            WEB_TRANSFER_MUTATION_RATE_PER_SEC,
            WEB_TRANSFER_MUTATION_BURST,
            start,
        );
        for _ in 0..8 {
            assert!(mutation.take(start));
        }
        assert!(!mutation.take(start));
        // Pre-auth: 20 burst, then refusal, per IP.
        let mut limiter = PreAuthLimiter::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..20 {
            assert!(limiter.check(ip, start));
        }
        assert!(!limiter.check(ip, start));
        let other: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(limiter.check(other, start));
        // Idle entries expire with a short TTL.
        let mut short = PreAuthLimiter::with_ttl(Duration::from_millis(50));
        assert!(short.check(ip, start));
        std::thread::sleep(Duration::from_millis(60));
        assert!(short.check(ip, Instant::now()));
        assert_eq!(short.len(), 1);
        // The map never grows past its cap; unseen IPs share the overflow.
        let mut full = PreAuthLimiter::default();
        for i in 0..WEB_TRANSFER_PRE_AUTH_MAX_IPS {
            let addr = IpAddr::from([(10u8), ((i / 256) % 256) as u8, (i % 256) as u8, 1u8]);
            assert!(full.check(addr, start));
        }
        assert_eq!(full.len(), WEB_TRANSFER_PRE_AUTH_MAX_IPS);
        let fresh: IpAddr = "192.0.2.1".parse().unwrap();
        let _ = full.check(fresh, start);
        assert_eq!(full.len(), WEB_TRANSFER_PRE_AUTH_MAX_IPS);
    }

    /// 6.2: repeated pre-auth failures from one address are reported at the
    /// 1st, 2nd, 4th, 8th … attempt and nowhere in between.
    ///
    /// Both ends of this are defects: a line per refusal makes one scanner a
    /// log-volume attack on the operator, and no line at all hides a single
    /// address failing ten thousand times. The powers of two are the order of
    /// magnitude, which is what an operator acts on.
    #[test]
    fn repeated_pre_auth_failures_are_logged_logarithmically() {
        let start = Instant::now();
        let mut sampler = AuthFailureSampler::default();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let mut reported = Vec::new();
        for _ in 0..64 {
            if let Some(count) = sampler.note(ip, start) {
                reported.push(count);
            }
        }
        assert_eq!(reported, vec![1, 2, 4, 8, 16, 32, 64]);

        // Another address is another story: the count is per IP, so a second
        // attacker is never hidden by the first one's silence.
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        assert_eq!(sampler.note(other, start), Some(1));

        // An idle entry expires: a host that failed once yesterday starts
        // again at "first failure" rather than at the old address's count.
        let mut short = AuthFailureSampler::with_ttl(Duration::from_millis(50));
        assert_eq!(short.note(ip, start), Some(1));
        assert_eq!(short.note(ip, start), Some(2));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(short.note(ip, Instant::now()), Some(1));

        // The map never grows past the limiter's own cap, and an IP scan at
        // capacity is still reported once instead of silencing the sampler.
        let mut full = AuthFailureSampler::default();
        for i in 0..WEB_TRANSFER_PRE_AUTH_MAX_IPS {
            let addr = IpAddr::from([(10u8), ((i / 256) % 256) as u8, (i % 256) as u8, 2u8]);
            assert_eq!(full.note(addr, start), Some(1));
        }
        assert_eq!(full.len(), WEB_TRANSFER_PRE_AUTH_MAX_IPS);
        let fresh: IpAddr = "198.51.100.9".parse().unwrap();
        assert_eq!(full.note(fresh, start), Some(1));
        assert_eq!(full.len(), WEB_TRANSFER_PRE_AUTH_MAX_IPS);
    }
}

#[cfg(test)]
mod offer_tests {
    use super::*;
    use crate::web_transfer_protocol::{canonical_json, file_root, manifest_value, parse_manifest};

    fn offer_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits::default(),
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn tight_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits {
                    max_metadata_per_room_bytes: 5000,
                    max_metadata_total_bytes: 5000,
                    ..WebTransferLimits::default()
                },
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn member_owner_pair() -> (MemberToken, OwnerToken, [u8; 32], [u8; 32]) {
        let member = MemberToken::from_bytes([0x51u8; 32]);
        let owner = OwnerToken::from_bytes([0x52u8; 32]);
        (member, owner, member.sha256_hash(), owner.sha256_hash())
    }

    /// Builds a minimal single-file manifest value with a recomputed root.
    fn manifest_json(offer_hex: &str, label: &str) -> serde_json::Value {
        let leaf: [u8; 32] =
            hex::decode("094c9eb526be7e2dea0b396331085eaba0d76f639116ccc014055c490b48daac")
                .unwrap()
                .try_into()
                .unwrap();
        let root = hex::encode(file_root(1, &[leaf]).unwrap());
        serde_json::json!({
            "offer": offer_hex,
            "mode": "single",
            "label": label,
            "kind": "file",
            "chunkSize": "1048576",
            "createdAt": "2026-09-14T12:00:00Z",
            "entries": [{
                "id": "0",
                "path": "hello.txt",
                "size": "11",
                "mtime": "1757779200",
                "chunks": ["094c9eb526be7e2dea0b396331085eaba0d76f639116ccc014055c490b48daac"],
                "chunkCount": "1",
                "root": root,
            }],
        })
    }

    fn publish_canonical(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        peer: PeerId,
        offer_hex: &str,
        label: &str,
    ) -> (
        Result<PublishOutcome, WebTransferError>,
        std::sync::Arc<[u8]>,
    ) {
        let value = manifest_json(offer_hex, label);
        let manifest = parse_manifest(&value, &registry.config().limits).unwrap();
        let canonical: std::sync::Arc<[u8]> = canonical_json(&manifest_value(&manifest))
            .unwrap()
            .into_bytes()
            .into();
        let mac = [0xeeu8; 32];
        let offer_id: OfferId = offer_hex.parse().unwrap();
        let outcome =
            registry.publish_offer(room, peer, offer_id, &manifest, canonical.clone(), mac);
        (outcome, canonical)
    }

    fn publish(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        peer: PeerId,
        offer_hex: &str,
        label: &str,
    ) -> Result<PublishOutcome, WebTransferError> {
        publish_canonical(registry, room, peer, offer_hex, label).0
    }

    fn open_peer_room(
        registry: &WebTransferRegistry,
    ) -> (
        OwnerLease,
        Arc<WebTransferRoom>,
        PeerId,
        PeerId,
        Vec<PeerGuard>,
    ) {
        let (_member, _owner, member_hash, owner_hash) = member_owner_pair();
        let lease = OwnerLease::create(registry, member_hash, owner_hash).unwrap();
        let room = lease.room().clone();
        let first = generate_peer_id();
        let second = generate_peer_id();
        let guard_a = registry
            .join_peer(&room, first, Some("A".to_string()))
            .unwrap();
        let guard_b = registry
            .join_peer(&room, second, Some("B".to_string()))
            .unwrap();
        (lease, room, first, second, vec![guard_a, guard_b])
    }

    #[tokio::test]
    async fn offer_reservations_roll_back_atomically() {
        let registry = tight_registry();
        let (_lease, room, first, _second, _guards) = open_peer_room(&registry);
        let before_room = room.state.lock().unwrap().metadata_bytes;
        let before_global = registry.current_metadata_bytes();
        // Per-peer cap is default 64: exhaust the tiny metadata budget first.
        let mut stored = 0u32;
        let mut refused = false;
        for i in 0..40u32 {
            let offer_hex = format!("{i:032x}");
            match publish(&registry, &room, first, &offer_hex, "item") {
                Ok(_) => stored += 1,
                Err(e) => {
                    assert_eq!(e.code(), "LIMIT_EXCEEDED");
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused, "tiny budget must refuse");
        assert!(stored > 0);
        let state = room.state.lock().unwrap();
        assert_eq!(state.offers.len(), stored as usize);
        assert_eq!(
            state.metadata_bytes,
            before_room + registry.current_metadata_bytes() - before_global
        );
        drop(state);
        // Failed publish stored nothing and moved no counter.
        let rooms = room.state.lock().unwrap().metadata_bytes;
        let global = registry.current_metadata_bytes();
        let offer_hex = format!("{:032x}", 999u32);
        assert!(publish(&registry, &room, first, &offer_hex, "item").is_err());
        assert_eq!(room.state.lock().unwrap().metadata_bytes, rooms);
        assert_eq!(registry.current_metadata_bytes(), global);
        assert!(!room
            .state
            .lock()
            .unwrap()
            .offers
            .contains_key(&offer_hex.parse().unwrap()));
    }

    #[tokio::test]
    async fn identical_publish_is_idempotent_but_changed_id_conflicts() {
        let registry = offer_registry();
        let (_lease, room, first, second, _guards) = open_peer_room(&registry);
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let (first_outcome, canonical) =
            publish_canonical(&registry, &room, first, offer_hex, "Demo");
        assert_eq!(first_outcome.unwrap(), PublishOutcome::Created);
        // Identical bytes, same owner: idempotent ack, still one offer.
        let manifest =
            parse_manifest(&manifest_json(offer_hex, "Demo"), &registry.config().limits).unwrap();
        let outcome = registry
            .publish_offer(
                &room,
                first,
                offer_hex.parse().unwrap(),
                &manifest,
                canonical,
                [0xeeu8; 32],
            )
            .unwrap();
        assert_eq!(outcome, PublishOutcome::Idempotent);
        assert_eq!(room.state.lock().unwrap().offers.len(), 1);
        // Same ID, different label bytes: conflict.
        let outcome = publish(&registry, &room, first, offer_hex, "Changed");
        assert_eq!(outcome.unwrap_err().code(), "OFFER_CHANGED");
        // Same ID, different owner: conflict, never an oracle.
        let outcome = publish(&registry, &room, second, offer_hex, "Demo");
        assert_eq!(outcome.unwrap_err().code(), "OFFER_CHANGED");
        assert_eq!(room.state.lock().unwrap().offers.len(), 1);
    }

    #[tokio::test]
    async fn identical_publish_is_idempotent_at_saturated_caps() {
        let offer_hex = "dddddddddddddddddddddddddddddddd";
        let value = manifest_json(offer_hex, "Saturated");
        let limits_for_parse = WebTransferLimits::default();
        let manifest = parse_manifest(&value, &limits_for_parse).unwrap();
        let canonical: std::sync::Arc<[u8]> = canonical_json(&manifest_value(&manifest))
            .unwrap()
            .into_bytes()
            .into();
        let charge = offer_charge(canonical.len()).unwrap();
        let registry = WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits {
                    max_offers_per_peer: 1,
                    max_metadata_per_room_bytes: charge,
                    max_metadata_total_bytes: charge,
                    ..WebTransferLimits::default()
                },
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let (_lease, room, first, _second, _guards) = open_peer_room(&registry);
        let offer_id: OfferId = offer_hex.parse().unwrap();
        assert_eq!(
            registry
                .publish_offer(
                    &room,
                    first,
                    offer_id,
                    &manifest,
                    std::sync::Arc::clone(&canonical),
                    [0xee; 32],
                )
                .unwrap(),
            PublishOutcome::Created
        );
        assert_eq!(room.state.lock().unwrap().metadata_bytes, charge);
        assert_eq!(registry.current_metadata_bytes(), charge);

        assert_eq!(
            registry
                .publish_offer(
                    &room,
                    first,
                    offer_id,
                    &manifest,
                    std::sync::Arc::clone(&canonical),
                    [0xee; 32],
                )
                .unwrap(),
            PublishOutcome::Idempotent
        );

        let changed_value = manifest_json(offer_hex, "Changed");
        let changed_manifest = parse_manifest(&changed_value, &limits_for_parse).unwrap();
        let changed_canonical: std::sync::Arc<[u8]> =
            canonical_json(&manifest_value(&changed_manifest))
                .unwrap()
                .into_bytes()
                .into();
        assert_eq!(
            registry
                .publish_offer(
                    &room,
                    first,
                    offer_id,
                    &changed_manifest,
                    changed_canonical,
                    [0xee; 32],
                )
                .unwrap_err()
                .code(),
            "OFFER_CHANGED"
        );
        assert_eq!(room.state.lock().unwrap().offers.len(), 1);
        assert_eq!(room.state.lock().unwrap().metadata_bytes, charge);
        assert_eq!(registry.current_metadata_bytes(), charge);
    }

    #[tokio::test]
    async fn room_revision_overflow_never_wraps_or_leaks_state() {
        let registry = offer_registry();
        let (_lease, room, first, _second, mut guards) = open_peer_room(&registry);
        let guard_a = guards.remove(0);
        let guard_b = guards.remove(0);
        room.state.lock().unwrap().revision = u64::MAX;

        let extra = generate_peer_id();
        assert_eq!(
            registry
                .join_peer(&room, extra, Some("Extra".to_string()))
                .unwrap_err()
                .code(),
            "INTERNAL"
        );
        assert!(!room.state.lock().unwrap().peers.contains_key(&extra));
        assert_eq!(registry.current_peers(), 2);

        let original_name = room.state.lock().unwrap().peers[&first]
            .display_name
            .clone();
        assert_eq!(
            registry
                .rename_peer(&room, first, "Changed")
                .unwrap_err()
                .code(),
            "INTERNAL"
        );
        assert_eq!(
            room.state.lock().unwrap().peers[&first].display_name,
            original_name
        );

        let offer_hex = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        assert_eq!(
            publish(&registry, &room, first, offer_hex, "Overflow")
                .unwrap_err()
                .code(),
            "INTERNAL"
        );
        assert!(room.state.lock().unwrap().offers.is_empty());
        assert_eq!(registry.current_metadata_bytes(), 0);

        room.state.lock().unwrap().revision = u64::MAX - 1;
        assert_eq!(
            publish(&registry, &room, first, offer_hex, "Overflow").unwrap(),
            PublishOutcome::Created
        );
        let offer_id: OfferId = offer_hex.parse().unwrap();
        let charge = room.state.lock().unwrap().metadata_bytes;
        assert!(charge > 0);
        assert_eq!(room.state.lock().unwrap().revision, u64::MAX);
        assert_eq!(
            registry
                .withdraw_offer(&room, first, offer_id)
                .unwrap_err()
                .code(),
            "INTERNAL"
        );
        assert!(room.state.lock().unwrap().offers.contains_key(&offer_id));
        assert_eq!(registry.current_metadata_bytes(), charge);

        let mut events = room.events.subscribe();
        drop(guard_a);
        {
            let state = room.state.lock().unwrap();
            assert!(!state.peers.contains_key(&first));
            assert!(!state.offers.contains_key(&offer_id));
            assert_eq!(state.metadata_bytes, 0);
            assert_eq!(state.revision, u64::MAX);
        }
        assert_eq!(registry.current_metadata_bytes(), 0);
        assert_eq!(registry.current_peers(), 1);
        assert!(room.is_destroyed());
        assert!(room.cancel.is_cancelled());
        assert!(registry.room(room.id).is_none());
        match events.try_recv().unwrap() {
            RoomEvent::RoomClosed { reason } => assert_eq!(reason, "revision-exhausted"),
            event => panic!("revision exhaustion emitted incremental event {event:?}"),
        }
        assert!(events.try_recv().is_err());

        drop(guard_b);
        assert_eq!(registry.current_peers(), 0);
        assert!(room.state.lock().unwrap().peers.is_empty());
    }

    #[tokio::test]
    async fn only_owner_withdraws() {
        let registry = offer_registry();
        let (_lease, room, first, second, _guards) = open_peer_room(&registry);
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let offer_id: OfferId = offer_hex.parse().unwrap();
        publish(&registry, &room, first, offer_hex, "Demo").unwrap();
        // Stranger hears NOT_PARTICIPANT; the offer survives.
        assert_eq!(
            registry
                .withdraw_offer(&room, second, offer_id)
                .unwrap_err()
                .code(),
            "NOT_PARTICIPANT"
        );
        assert!(room.state.lock().unwrap().offers.contains_key(&offer_id));
        // Owner removes; repeat and unknown IDs are terminal acks.
        assert_eq!(
            registry.withdraw_offer(&room, first, offer_id).unwrap(),
            WithdrawOutcome::Removed
        );
        assert!(!room.state.lock().unwrap().offers.contains_key(&offer_id));
        assert_eq!(
            registry.withdraw_offer(&room, first, offer_id).unwrap(),
            WithdrawOutcome::AlreadyGone
        );
        assert_eq!(
            registry
                .withdraw_offer(
                    &room,
                    first,
                    "dddddddddddddddddddddddddddddddd".parse().unwrap()
                )
                .unwrap(),
            WithdrawOutcome::AlreadyGone
        );
    }

    #[tokio::test]
    async fn withdraw_and_peer_drop_release_exact_metadata_once() {
        let registry = offer_registry();
        let (_lease, room, first, _second, _guards) = open_peer_room(&registry);
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let offer_id: OfferId = offer_hex.parse().unwrap();
        let (_, canonical) = publish_canonical(&registry, &room, first, offer_hex, "Demo");
        let charge = offer_charge(canonical.len()).unwrap();
        assert_eq!(room.state.lock().unwrap().metadata_bytes, charge);
        assert_eq!(registry.current_metadata_bytes(), charge);
        registry.withdraw_offer(&room, first, offer_id).unwrap();
        assert_eq!(room.state.lock().unwrap().metadata_bytes, 0);
        assert_eq!(registry.current_metadata_bytes(), 0);
        // Republish, then drop the owner guard: the drop path releases too.
        let (_member, _owner, member_hash, owner_hash) = member_owner_pair();
        let lease = OwnerLease::create(&registry, member_hash, owner_hash).unwrap();
        let room = lease.room().clone();
        let peer = generate_peer_id();
        let guard = registry.join_peer(&room, peer, None).unwrap();
        publish(&registry, &room, peer, offer_hex, "Demo").unwrap();
        assert_eq!(registry.current_metadata_bytes(), charge);
        drop(guard);
        assert_eq!(room.state.lock().unwrap().metadata_bytes, 0);
        assert_eq!(registry.current_metadata_bytes(), 0);
        // Withdrawing after the drop is a terminal ack, never a double free.
        assert_eq!(
            registry.withdraw_offer(&room, peer, offer_id).unwrap(),
            WithdrawOutcome::AlreadyGone
        );
        assert_eq!(registry.current_metadata_bytes(), 0);
    }

    #[tokio::test]
    async fn snapshot_reuses_manifest_arc_and_orders_offers() {
        use crate::web_transfer_protocol::{parse_server_envelope, SnapshotOffer};
        let registry = offer_registry();
        let (_lease, room, first, _second, _guards) = open_peer_room(&registry);
        // Publish in reverse hex order; snapshots must still sort ascending.
        publish(
            &registry,
            &room,
            first,
            "ffffffffffffffffffffffffffffffff",
            "Zed",
        )
        .unwrap();
        let (_, canonical) = publish_canonical(
            &registry,
            &room,
            first,
            "11111111111111111111111111111111",
            "Ay",
        );
        let _ = canonical;
        let views = offer_views(&room).unwrap();
        assert_eq!(views.len(), 2);
        assert!(views[0].offer.to_string() < views[1].offer.to_string());
        // No copy in state: the parts reference the very Arc the record holds.
        {
            let state = room.state.lock().unwrap();
            for view in &views {
                let record = state.offers.get(&view.offer).unwrap();
                assert!(std::sync::Arc::ptr_eq(&view.manifest, &record.manifest));
            }
        }
        let (revision, peers) = snapshot_parts(&room).unwrap();
        let parsed: Vec<(PeerId, OfferId, serde_json::Value, String)> = views
            .iter()
            .map(|view| {
                let value: serde_json::Value = serde_json::from_slice(&view.manifest).unwrap();
                (view.peer, view.offer, value, hex::encode(view.mac))
            })
            .collect();
        let refs: Vec<SnapshotOffer> = parsed
            .iter()
            .map(|(peer, offer, manifest, mac_hex)| SnapshotOffer {
                peer: *peer,
                offer: *offer,
                manifest,
                mac_hex,
            })
            .collect();
        let messages = crate::web_transfer_protocol::snapshot_messages(revision, &peers, &refs);
        // Begin + 2 peers + 2 offers + end, offers sorted.
        assert_eq!(messages.len(), 6);
        let types: Vec<String> = messages
            .iter()
            .map(|m| parse_server_envelope(m).unwrap().typ)
            .collect();
        assert_eq!(
            types,
            vec![
                "snapshot.begin",
                "snapshot.peer",
                "snapshot.peer",
                "snapshot.offer",
                "snapshot.offer",
                "snapshot.end"
            ]
        );
        let first_offer = parse_server_envelope(&messages[3]).unwrap();
        let second_offer = parse_server_envelope(&messages[4]).unwrap();
        assert!(first_offer.body["offerId"].as_str() < second_offer.body["offerId"].as_str());
    }

    #[tokio::test]
    async fn offer_logs_do_not_include_private_metadata() {
        let registry = offer_registry();
        let (_lease, room, first, second, _guards) = open_peer_room(&registry);
        let canary_label = "SECRET-CANARY-LABEL";
        let canary_path = "secret-canary-dir/evil.txt";
        let mut value = manifest_json("cccccccccccccccccccccccccccccccc", canary_label);
        value["entries"][0]["path"] = serde_json::Value::String(canary_path.to_string());
        // Structural failure (lying root) carries no label or path.
        value["entries"][0]["root"] = serde_json::Value::String("0".repeat(64));
        let structural = parse_manifest(&value, &registry.config().limits).unwrap_err();
        // Ownership conflict carries no label or path.
        publish(
            &registry,
            &room,
            first,
            "cccccccccccccccccccccccccccccccc",
            "Demo",
        )
        .unwrap();
        let conflict = publish(
            &registry,
            &room,
            first,
            "cccccccccccccccccccccccccccccccc",
            canary_label,
        )
        .unwrap_err();
        let stranger = registry
            .withdraw_offer(
                &room,
                second,
                "cccccccccccccccccccccccccccccccc".parse().unwrap(),
            )
            .unwrap_err();
        let texts = [
            format!("{structural}"),
            format!("{} {}", conflict.code(), conflict),
            format!("{} {}", stranger.code(), stranger),
        ];
        for text in texts {
            assert!(!text.contains(canary_label), "label leaked: {text}");
            assert!(!text.contains(canary_path), "path leaked: {text}");
            assert!(!text.contains("evil"), "path fragment leaked: {text}");
        }
    }
}

#[cfg(test)]
mod peer_permission_tests {
    use super::*;

    fn permission_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits::default(),
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn three_peer_room(
        registry: &WebTransferRegistry,
    ) -> (
        OwnerLease,
        Arc<WebTransferRoom>,
        [PeerId; 3],
        Vec<PeerGuard>,
    ) {
        let member = MemberToken::from_bytes([0x71u8; 32]);
        let owner = OwnerToken::from_bytes([0x72u8; 32]);
        let lease =
            OwnerLease::create(registry, member.sha256_hash(), owner.sha256_hash()).unwrap();
        let room = lease.room().clone();
        let ids = [generate_peer_id(), generate_peer_id(), generate_peer_id()];
        let guards = ids
            .iter()
            .map(|id| registry.join_peer(&room, *id, None).unwrap())
            .collect();
        (lease, room, ids, guards)
    }

    fn single_file_offer(
        registry: &WebTransferRegistry,
        offer_hex: &str,
    ) -> (crate::web_transfer_protocol::Manifest, std::sync::Arc<[u8]>) {
        use crate::web_transfer_protocol::{
            canonical_json, file_root, manifest_value, parse_manifest,
        };
        let leaf: [u8; 32] = [0x11u8; 32];
        let root = hex::encode(file_root(1, &[leaf]).unwrap());
        let value = serde_json::json!({
            "offer": offer_hex,
            "mode": "single",
            "label": "Item",
            "kind": "file",
            "chunkSize": "1048576",
            "createdAt": "2026-09-14T12:00:00Z",
            "entries": [{
                "id": "0",
                "path": "item.bin",
                "size": "17",
                "mtime": "1757779200",
                "chunks": [hex::encode(leaf)],
                "chunkCount": "1",
                "root": root,
            }],
        });
        let manifest = parse_manifest(&value, &registry.config().limits).unwrap();
        let canonical: std::sync::Arc<[u8]> = canonical_json(&manifest_value(&manifest))
            .unwrap()
            .into_bytes()
            .into();
        (manifest, canonical)
    }

    #[tokio::test]
    async fn peer_permissions_matrix_is_symmetric() {
        let registry = permission_registry();
        let (_lease, room, ids, _guards) = three_peer_room(&registry);
        // Every peer publishes its own offer and withdraws it; strangers are
        // refused everywhere; the catalog always shows exactly the live set.
        for (index, peer) in ids.iter().enumerate() {
            let offer_hex = format!("{index:032x}");
            let (wrapped, canonical) = single_file_offer(&registry, &offer_hex);
            let offer_id: OfferId = offer_hex.parse().unwrap();
            assert_eq!(
                registry
                    .publish_offer(&room, *peer, offer_id, &wrapped, canonical, [0xeeu8; 32])
                    .unwrap(),
                PublishOutcome::Created
            );
        }
        assert_eq!(room.state.lock().unwrap().offers.len(), 3);
        for (index, peer) in ids.iter().enumerate() {
            let stranger = ids[(index + 1) % 3];
            let offer_id: OfferId = format!("{index:032x}").parse().unwrap();
            assert_eq!(
                registry
                    .withdraw_offer(&room, stranger, offer_id)
                    .unwrap_err()
                    .code(),
                "NOT_PARTICIPANT"
            );
            assert_eq!(
                registry.withdraw_offer(&room, *peer, offer_id).unwrap(),
                WithdrawOutcome::Removed
            );
        }
        assert!(room.state.lock().unwrap().offers.is_empty());
        assert_eq!(registry.current_metadata_bytes(), 0);
    }

    #[tokio::test]
    async fn member_cannot_close_room_or_cancel_placeholder_for_others() {
        let registry = permission_registry();
        let (_lease, room, ids, guards) = three_peer_room(&registry);
        // One live offer owned by the first peer.
        let (wrapped, canonical) = single_file_offer(&registry, "dddddddddddddddddddddddddddddddd");
        let offer_id: OfferId = "dddddddddddddddddddddddddddddddd".parse().unwrap();
        registry
            .publish_offer(&room, ids[0], offer_id, &wrapped, canonical, [0xeeu8; 32])
            .unwrap();
        // A stranger cannot cancel it, and peer churn never destroys the
        // room or touches transfers.
        let stranger = generate_peer_id();
        let _guard = registry.join_peer(&room, stranger, None).unwrap();
        assert_eq!(
            registry
                .withdraw_offer(&room, stranger, offer_id)
                .unwrap_err()
                .code(),
            "NOT_PARTICIPANT"
        );
        assert!(room.state.lock().unwrap().offers.contains_key(&offer_id));
        drop(guards);
        assert!(!room.is_destroyed());
        assert!(registry.room(room.id).is_some());
        assert!(room.state.lock().unwrap().transfers.is_empty());
        assert_eq!(registry.current_peers(), 1);
        // Withdrawing the reaped offer afterwards is a terminal ack, and the
        // room still stands: only OwnerLease closes rooms.
        assert_eq!(
            registry.withdraw_offer(&room, stranger, offer_id).unwrap(),
            WithdrawOutcome::AlreadyGone
        );
        assert!(!room.is_destroyed());
    }

    #[tokio::test]
    async fn disconnect_publish_race_leaves_no_orphan_offer() {
        use tokio::sync::Barrier;
        for _ in 0..20 {
            let registry = permission_registry();
            let member = MemberToken::from_bytes([0x73u8; 32]);
            let owner = OwnerToken::from_bytes([0x74u8; 32]);
            let lease =
                OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash()).unwrap();
            let room = lease.room().clone();
            let peer = generate_peer_id();
            let guard = registry.join_peer(&room, peer, None).unwrap();
            let barrier = std::sync::Arc::new(Barrier::new(2));
            // Task A publishes while task B disconnects: the scheduler picks
            // the order, and both linearizations must converge on empty.
            let task_registry = registry.clone();
            let task_room = Arc::clone(&room);
            let task_barrier = Arc::clone(&barrier);
            let publisher = tokio::spawn(async move {
                task_barrier.wait().await;
                let (wrapped, canonical) =
                    single_file_offer(&task_registry, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
                task_registry.publish_offer(
                    &task_room,
                    peer,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
                    &wrapped,
                    canonical,
                    [0xeeu8; 32],
                )
            });
            let dropper = tokio::spawn(async move {
                barrier.wait().await;
                drop(guard);
            });
            let published = publisher.await.unwrap();
            dropper.await.unwrap();
            // Either the publish landed (then the drop reaped it) or the
            // drop won (then the publish found no peer).
            assert!(published.is_ok() || published.unwrap_err().code() == "INVALID_MESSAGE");
            assert!(!room.state.lock().unwrap().peers.contains_key(&peer));
            assert!(room.state.lock().unwrap().offers.is_empty());
            assert_eq!(room.state.lock().unwrap().metadata_bytes, 0);
            assert_eq!(registry.current_metadata_bytes(), 0);
            assert_eq!(registry.current_peers(), 0);
        }
    }

    #[tokio::test]
    async fn room_close_peer_cleanup_is_idempotent() {
        use tokio::sync::Barrier;
        for _ in 0..20 {
            let registry = permission_registry();
            let member = MemberToken::from_bytes([0x75u8; 32]);
            let owner = OwnerToken::from_bytes([0x76u8; 32]);
            let lease =
                OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash()).unwrap();
            let room = lease.room().clone();
            let peer = generate_peer_id();
            let guard = registry.join_peer(&room, peer, None).unwrap();
            let barrier = std::sync::Arc::new(Barrier::new(2));
            // A publisher races the explicit close: stored offers die with
            // the entry, and the guard drop afterwards still releases.
            let task_registry = registry.clone();
            let task_room = Arc::clone(&room);
            let task_barrier = Arc::clone(&barrier);
            let publisher = tokio::spawn(async move {
                task_barrier.wait().await;
                let (wrapped, canonical) =
                    single_file_offer(&task_registry, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
                let _ = task_registry.publish_offer(
                    &task_room,
                    peer,
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap(),
                    &wrapped,
                    canonical,
                    [0xeeu8; 32],
                );
            });
            barrier.wait().await;
            lease.close_explicit(&registry);
            publisher.await.unwrap();
            drop(guard);
            assert!(registry.room(room.id).is_none());
            assert!(!room.state.lock().unwrap().peers.contains_key(&peer));
            assert!(room.state.lock().unwrap().offers.is_empty());
            assert_eq!(room.state.lock().unwrap().metadata_bytes, 0);
            assert_eq!(registry.current_metadata_bytes(), 0);
            assert_eq!(registry.current_peers(), 0);
        }
    }

    #[tokio::test]
    async fn frontend_instrumentation_is_inert_without_test_hook() {
        // The hook contract lives entirely in the frontend (fixtures.js +
        // main.js): without `globalThis.__BORE_TEST__` the app installs no
        // recorder. This test pins the Rust side of the same rule — member
        // control paths never consult ambient state — by asserting the
        // session primitives take every input explicitly.
        let registry = permission_registry();
        let (_lease, room, ids, _guards) = three_peer_room(&registry);
        // join/rename/publish/withdraw signatures all name their peer: no
        // ambient identity, so no hook could change who acts.
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let (wrapped, canonical) = single_file_offer(&registry, offer_hex);
        assert_eq!(
            registry
                .publish_offer(
                    &room,
                    ids[0],
                    offer_hex.parse().unwrap(),
                    &wrapped,
                    canonical,
                    [0xeeu8; 32]
                )
                .unwrap(),
            PublishOutcome::Created
        );
        assert!(registry.rename_peer(&room, ids[0], "Visible").is_ok());
        let parts = snapshot_parts(&room).unwrap();
        assert_eq!(parts.1.len(), 3);
    }
}

#[cfg(test)]
mod transfer_state_tests {
    use super::*;
    use crate::web_transfer_protocol::{
        canonical_json, file_root, manifest_value, parse_manifest, selection_digest,
        DirectFailedBody, RtcIceBody, RtcSdpBody,
    };

    fn busy_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits {
                    max_relays_global: 1,
                    ..WebTransferLimits::default()
                },
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn transfer_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits::default(),
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn tight_transfer_registry() -> WebTransferRegistry {
        WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits {
                    max_transfers_per_peer: 1,
                    ..WebTransferLimits::default()
                },
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn transfer_room(registry: &WebTransferRegistry) -> (OwnerLease, Arc<WebTransferRoom>) {
        let member = MemberToken::from_bytes([0x81u8; 32]);
        let owner = OwnerToken::from_bytes([0x82u8; 32]);
        let lease =
            OwnerLease::create(registry, member.sha256_hash(), owner.sha256_hash()).unwrap();
        let room = lease.room().clone();
        (lease, room)
    }

    /// Joins a peer AND registers a live session queue, returning the
    /// receiver so tests assert targeted delivery without sockets.
    fn live_peer(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        name: Option<&str>,
    ) -> (PeerId, PeerGuard, mpsc::Receiver<String>) {
        let id = generate_peer_id();
        let guard = registry
            .join_peer(room, id, name.map(str::to_string))
            .unwrap();
        let (tx, rx) = mpsc::channel(64);
        room.sessions.lock().unwrap().insert(id, tx);
        (id, guard, rx)
    }

    fn offer_fixture(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        owner: PeerId,
        offer_hex: &str,
    ) -> [u8; 32] {
        let leaf: [u8; 32] = [0x11u8; 32];
        let root = hex::encode(file_root(1, &[leaf]).unwrap());
        let value = serde_json::json!({
            "offer": offer_hex,
            "mode": "single",
            "label": "Item",
            "kind": "file",
            "chunkSize": "1048576",
            "createdAt": "2026-09-14T12:00:00Z",
            "entries": [{
                "id": "0",
                "path": "item.bin",
                "size": "17",
                "mtime": "1757779200",
                "chunks": [hex::encode(leaf)],
                "chunkCount": "1",
                "root": root,
            }],
        });
        let manifest = parse_manifest(&value, &registry.config().limits).unwrap();
        let canonical: std::sync::Arc<[u8]> = canonical_json(&manifest_value(&manifest))
            .unwrap()
            .into_bytes()
            .into();
        let mac = [0xeeu8; 32];
        let offer_id: OfferId = offer_hex.parse().unwrap();
        registry
            .publish_offer(room, owner, offer_id, &manifest, canonical, mac)
            .unwrap();
        mac
    }

    /// A FOLDER offer: one directory entry and two files, which is the
    /// smallest shape a `zip` selection can be asked for.
    fn folder_offer_fixture(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        owner: PeerId,
        offer_hex: &str,
    ) -> [u8; 32] {
        let leaf: [u8; 32] = [0x22u8; 32];
        let root = hex::encode(file_root(1, &[leaf]).unwrap());
        let value = serde_json::json!({
            "offer": offer_hex,
            "mode": "multi",
            "label": "tree",
            "kind": "folder",
            "chunkSize": "1048576",
            "createdAt": "2026-09-16T12:00:00Z",
            "entries": [
                {
                    "id": "0",
                    "path": "tree",
                    "size": "0",
                    "mtime": "1757779200",
                    "chunks": [],
                    "chunkCount": "0",
                    "root": serde_json::Value::Null,
                },
                {
                    "id": "1",
                    "path": "tree/a.bin",
                    "size": "9",
                    "mtime": "1757779200",
                    "chunks": [hex::encode(leaf)],
                    "chunkCount": "1",
                    "root": root,
                },
                {
                    "id": "2",
                    "path": "tree/b.bin",
                    "size": "11",
                    "mtime": "1757779200",
                    "chunks": [hex::encode(leaf)],
                    "chunkCount": "1",
                    "root": root,
                },
            ],
        });
        let manifest = parse_manifest(&value, &registry.config().limits).unwrap();
        let canonical: std::sync::Arc<[u8]> = canonical_json(&manifest_value(&manifest))
            .unwrap()
            .into_bytes()
            .into();
        let mac = [0xabu8; 32];
        let offer_id: OfferId = offer_hex.parse().unwrap();
        registry
            .publish_offer(room, owner, offer_id, &manifest, canonical, mac)
            .unwrap();
        mac
    }

    fn request_fixture(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        recipient: PeerId,
        offer_hex: &str,
        mac: &[u8; 32],
    ) -> (TransferId, AttemptId) {
        let offer_id: OfferId = offer_hex.parse().unwrap();
        let digest = selection_digest(&offer_id, mac, &["0".to_string()], "raw");
        let (id, _) = registry
            .request_transfer(
                room,
                recipient,
                offer_id,
                vec!["0".to_string()],
                digest,
                "raw",
                None,
            )
            .unwrap();
        let attempt = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .attempt_id
            .unwrap();
        (id, attempt)
    }

    /// Phase 4 made `source_ready` open the DIRECT attempt, so the relay is
    /// reached only through the one automatic fallback. This is the shape a
    /// browser with no usable DataChannel produces, and it is what every
    /// relay assertion below goes through.
    ///
    /// The two `transfer.direct_start` envelopes and the counterpart's
    /// `transfer.direct_failed` are dropped rather than delivered: they are
    /// the direct step's own traffic, asserted by the direct tests, and
    /// delivering them here would only shift every `recv_text` by two.
    /// Returns the relay outcome, the FRESH attempt ID and the relay notices.
    fn ready_then_relay(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        source: PeerId,
        id: TransferId,
        attempt: AttemptId,
        digest: [u8; 32],
    ) -> (ReadyOutcome, AttemptId, TransferOutbox) {
        let (outcome, starts) = registry
            .source_ready(room, source, id, attempt, digest)
            .unwrap();
        assert_eq!(outcome, ReadyOutcome::Negotiating);
        assert_eq!(starts.len(), 2, "both peers hear transfer.direct_start");
        let body = crate::web_transfer_protocol::DirectFailedBody {
            transfer_id: id,
            attempt_id: attempt,
            reason: "unsupported",
            verified_ranges: Vec::new(),
        };
        let (relay, mut outbox) = registry.direct_failed(room, source, &body).unwrap();
        if !outbox.is_empty() {
            // Index 0 is the counterpart's `transfer.direct_failed` notice.
            outbox.remove(0);
        }
        let next = registry.current_attempt(room, id).unwrap();
        (relay, next, outbox)
    }

    async fn recv_text(rx: &mut mpsc::Receiver<String>) -> (String, serde_json::Value) {
        let raw = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        (
            value["type"].as_str().unwrap().to_string(),
            value["body"].clone(),
        )
    }

    /// `per_peer_transfer_cap_counts_raw_and_zip_together`: the budget is a
    /// property of the PEER, not of a mode. An archive costs a source the
    /// same as a file — it reads every file in the offer, so if anything it
    /// costs more — and counting the two in separate budgets would let a
    /// peer hold twice the transfers the operator configured by alternating
    /// between them. The check is in `request_transfer`, before a record is
    /// created, so neither ordering can slip past the other.
    #[tokio::test]
    async fn per_peer_transfer_cap_counts_raw_and_zip_together() {
        let file_hex = "cccccccccccccccccccccccccccccccc";
        let folder_hex = "dddddddddddddddddddddddddddddddd";
        // Both orderings, against a registry whose budget is exactly one.
        for zip_first in [false, true] {
            let registry = tight_transfer_registry();
            let (_lease, room) = transfer_room(&registry);
            let (source, _guard_a, _rx_a) = live_peer(&registry, &room, Some("A"));
            let (recipient, _guard_b, _rx_b) = live_peer(&registry, &room, Some("B"));
            let file_mac = offer_fixture(&registry, &room, source, file_hex);
            let folder_mac = folder_offer_fixture(&registry, &room, source, folder_hex);
            let file_id: OfferId = file_hex.parse().unwrap();
            let folder_id: OfferId = folder_hex.parse().unwrap();
            let raw_ids = vec!["0".to_string()];
            let zip_ids = vec!["0".to_string(), "1".to_string(), "2".to_string()];
            let raw = (
                file_id,
                raw_ids.clone(),
                selection_digest(&file_id, &file_mac, &raw_ids, "raw"),
                "raw",
            );
            let zip = (
                folder_id,
                zip_ids.clone(),
                selection_digest(&folder_id, &folder_mac, &zip_ids, "zip"),
                "zip",
            );
            let (first, second) = if zip_first {
                (zip.clone(), raw.clone())
            } else {
                (raw.clone(), zip.clone())
            };
            registry
                .request_transfer(&room, recipient, first.0, first.1, first.2, first.3, None)
                .unwrap_or_else(|error| {
                    panic!("the first request must be admitted, got {error:?}")
                });
            let refused = registry
                .request_transfer(
                    &room, recipient, second.0, second.1, second.2, second.3, None,
                )
                .expect_err("the second request must exhaust the shared budget");
            assert_eq!(
                refused.code(),
                "LIMIT_EXCEEDED",
                "a {} request after a {} one must hit the SAME per-peer budget",
                second.3,
                first.3
            );
        }
    }

    /// `cli_owner_has_no_browser_privileges_or_peer_id`: the CLI that opens
    /// the room holds a LEASE, not a seat. There is no owner `PeerId`
    /// anywhere in the room state — `OwnerState` carries an epoch and a
    /// deadline and nothing else — so the room's peer list is exactly the
    /// browsers that joined, and the first browser to arrive is an ordinary
    /// peer like every other. The two halves matter separately: a room whose
    /// peer list counted its owner would let a `--web-transfer-max-peers-per-room`
    /// of N admit N-1 browsers, and an "owner browser" would be a privilege
    /// the fragment cannot express (every peer holds the same member token).
    /// 6.1 — the pending-handshake bound, at the level of the thing that
    /// holds it. The wiring (a generic 503, the rest of the surface still
    /// serving, the slot back after the upgrade) is `t_web_handshake_admission`
    /// over a real socket; this pins the bound itself and, above all, that a
    /// released permit is released EXACTLY once — a double release would widen
    /// the semaphore permanently and silently.
    #[test]
    fn pending_handshake_semaphore_and_timeout_release_exactly_once() {
        let registry = transfer_registry();
        assert_eq!(
            registry.handshake_slots_available(),
            WEB_TRANSFER_PENDING_HANDSHAKES
        );
        // The deadline is a property of the upgrade, not of a session: it has
        // to be short enough that a silent caller cannot hold a slot for
        // long, and it is quoted here so a change has to be deliberate.
        assert_eq!(WEB_TRANSFER_HANDSHAKE_TIMEOUT, Duration::from_secs(10));

        let mut held = Vec::new();
        while let Some(permit) = registry.try_acquire_handshake() {
            held.push(permit);
        }
        assert_eq!(held.len(), WEB_TRANSFER_PENDING_HANDSHAKES);
        assert_eq!(registry.handshake_slots_available(), 0);
        // Saturated means REFUSED, never queued: a queue would turn a burst
        // into a pile of waiting tasks, which is the state this bound exists
        // to refuse.
        assert!(registry.try_acquire_handshake().is_none());

        // One release gives back exactly one slot.
        drop(held.pop().expect("a permit to release"));
        assert_eq!(registry.handshake_slots_available(), 1);
        let again = registry.try_acquire_handshake().expect("the freed slot");
        assert_eq!(registry.handshake_slots_available(), 0);
        drop(again);
        assert_eq!(registry.handshake_slots_available(), 1);

        // And releasing the rest returns exactly the initial count — no more.
        drop(held);
        assert_eq!(
            registry.handshake_slots_available(),
            WEB_TRANSFER_PENDING_HANDSHAKES
        );
    }

    /// 6.1 — the pre-auth limiter is bounded in SIZE as well as in rate.
    ///
    /// The rate alone would still let a source-address scan grow the map
    /// without bound, which is the allocation the limiter is supposed to
    /// prevent. Past the cap an unseen address shares ONE overflow bucket,
    /// and — the part that matters — a tracked address keeps its own.
    #[test]
    fn preauth_lru_caps_at_8192_and_uses_overflow_bucket() {
        let mut limiter = PreAuthLimiter::default();
        let now = Instant::now();
        let ip = |n: u32| IpAddr::from(std::net::Ipv4Addr::from(n));

        // A tracked address that must survive the scan below.
        assert!(limiter.check(ip(1), now));
        for n in 2..=(WEB_TRANSFER_PRE_AUTH_MAX_IPS as u32) {
            assert!(limiter.check(ip(n), now), "first attempt from {n}");
        }
        assert_eq!(limiter.len(), WEB_TRANSFER_PRE_AUTH_MAX_IPS);

        // Past the cap the map does not grow, and the newcomers share one
        // bucket: its burst is spent once, by all of them together.
        let mut allowed = 0;
        for n in 0..1000u32 {
            if limiter.check(ip(1_000_000 + n), now) {
                allowed += 1;
            }
        }
        assert_eq!(limiter.len(), WEB_TRANSFER_PRE_AUTH_MAX_IPS, "map grew");
        assert_eq!(
            allowed, WEB_TRANSFER_PRE_AUTH_BURST as i32,
            "the overflow bucket is one bucket, not one per address"
        );

        // The scan cost the tracked address nothing: it still has its own
        // bucket, with its own remaining burst.
        assert!(limiter.check(ip(1), now), "a tracked address was evicted");

        // Idle entries age out, which is what keeps the cap from being
        // reached by addresses nobody has seen for ten minutes.
        let mut short = PreAuthLimiter::with_ttl(Duration::from_millis(1));
        assert!(short.check(ip(7), now));
        assert_eq!(short.len(), 1);
        assert!(short.check(ip(8), now + Duration::from_millis(5)));
        assert_eq!(short.len(), 1, "the idle entry was not purged");
    }

    /// 6.1 — a capacity is never taken from a number a peer chose.
    ///
    /// `Vec::with_capacity(n)` allocates `n` before a single element exists,
    /// so a peer-controlled `n` is a one-message allocation primitive (the
    /// transfer receiver's own B1). The rule is a tripwire and not a review
    /// because it has to hold for code written later: every `with_capacity`
    /// on this surface is either clamped with `.min(` or derived from bytes
    /// already received.
    #[test]
    fn every_protocol_length_is_checked_before_allocation() {
        let call = concat!("with_", "capacity(");
        let assoc = format!("::{call}");
        let method = format!(".{call}");
        for (name, source) in [
            ("web_transfer.rs", include_str!("web_transfer.rs")),
            (
                "web_transfer_protocol.rs",
                include_str!("web_transfer_protocol.rs"),
            ),
            ("web_transfer_http.rs", include_str!("web_transfer_http.rs")),
        ] {
            for (number, line) in source.lines().enumerate() {
                // A comment mentioning the rule is not an allocation: this
                // very doc comment is the first thing the scan would flag.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                // Matched on the CALL, and the needle is assembled from two
                // pieces on purpose: written whole it would appear in this
                // file and the scan would flag its own source line.
                let Some(rest) = line
                    .split_once(assoc.as_str())
                    .or_else(|| line.split_once(method.as_str()))
                else {
                    continue;
                };
                let argument = rest.1;
                let clamped = argument.contains(".min(");
                // `x.len()` is the length of something already in memory, so
                // it is bounded by whatever bounded that. A bare parsed
                // count is not, and that is what this refuses.
                let from_existing = argument.contains(".len()");
                let literal = argument.chars().next().is_some_and(|c| c.is_ascii_digit());
                assert!(
                    clamped || from_existing || literal,
                    "{name}:{}: with_capacity from an unclamped value: {}",
                    number + 1,
                    line.trim()
                );
            }
        }
    }

    /// 6.1 — one room's throttle is one room's problem.
    ///
    /// The relay pump takes the room's throttle lock, computes a delay and
    /// RELEASES it before sleeping; the throttle itself lives on the room and
    /// not on the registry. Both halves matter: a throttle held across the
    /// await would stall the room's other relays, and a shared throttle would
    /// let a rate-limited room slow every other room on the server. This test
    /// holds one room's throttle and then does, on the SAME thread, what the
    /// pump of another room does — a shared lock would deadlock here rather
    /// than fail, which is the honest shape of this defect.
    #[tokio::test]
    async fn throttled_room_does_not_hold_registry_or_other_room_lock() {
        let registry = transfer_registry();
        let (_lease_a, room_a) = transfer_room(&registry);
        let member = MemberToken::from_bytes([0x91u8; 32]);
        let owner = OwnerToken::from_bytes([0x92u8; 32]);
        let lease_b =
            OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash()).unwrap();
        let room_b = lease_b.room().clone();
        assert_ne!(room_a.id, room_b.id);

        let held = room_a.relay_throttle.lock().expect("room A throttle");
        // The registry is still readable: the throttle is not on it.
        assert!(registry.room(room_b.id).is_some());
        // And room B's own throttle is a different lock, which its pump can
        // take while room A's is held.
        let delay = room_b
            .relay_throttle
            .lock()
            .expect("room B throttle")
            .delay_for(Instant::now(), 1024);
        assert_eq!(delay, Duration::ZERO, "room B paid for room A's budget");
        drop(held);
    }

    /// 6.1 — a refusal leaves nothing behind.
    ///
    /// Every admission on this surface is an RAII guard, and the failure
    /// shape that matters is the one where a guard is taken and the operation
    /// then fails: the counter must come back on its own, or a server slowly
    /// runs out of a resource nobody is using. Checked across the three
    /// counted things — rooms, peers and relays — plus a refusal that has to
    /// leave the live count untouched.
    #[tokio::test]
    async fn all_guards_release_on_panic_free_error_paths() {
        let registry = transfer_registry();
        let rooms_before = registry.current_rooms();
        let peers_before = registry.current_peers();

        let (lease, room) = transfer_room(&registry);
        assert_eq!(registry.current_rooms(), rooms_before + 1);
        {
            let (_peer, _guard, _rx) = live_peer(&registry, &room, Some("A"));
            assert_eq!(registry.current_peers(), peers_before + 1);
            // A relay permit taken and dropped without ever attaching.
            let permit = registry.try_acquire_relay().expect("a relay slot");
            drop(permit);
        }
        // The peer guard and the permit went out of scope: their counts come
        // back on their own, with nothing to reap.
        assert_eq!(registry.current_peers(), peers_before);
        // The ROOM is deliberately not in that set: dropping the lease
        // DETACHES with the owner grace, so the room stays counted until the
        // grace expires or the owner closes it explicitly. Closing it is what
        // returns the count, and a room count that fell on a dropped lease
        // would be the bug (a reconnecting owner would find nothing).
        assert_eq!(registry.current_rooms(), rooms_before + 1);
        lease.close_explicit(&registry);
        // The permit lives ON the room, so the count comes back when the last
        // `Arc` goes — which is the property worth pinning: a permit released
        // at close while a pump still held the room would over-admit.
        assert_eq!(registry.current_rooms(), rooms_before + 1);
        drop(room);
        assert_eq!(registry.current_rooms(), rooms_before);

        // Exhausting a budget refuses instead of leaking: taking every relay
        // slot, failing to take one more, and releasing them all returns the
        // budget exactly.
        let mut held = Vec::new();
        while let Ok(permit) = registry.try_acquire_relay() {
            held.push(permit);
        }
        assert!(registry.try_acquire_relay().is_err());
        let count = held.len();
        drop(held);
        let mut again = Vec::new();
        while let Ok(permit) = registry.try_acquire_relay() {
            again.push(permit);
        }
        assert_eq!(again.len(), count, "the relay budget did not come back");
    }

    /// 6.2: every counter the admin surface publishes moves with the GUARD
    /// that owns the resource, and never on its own schedule.
    ///
    /// A gauge is only worth reading if it goes back down, and a total is only
    /// worth reading if it never does. The distinction is the whole contract
    /// of the metrics endpoint, so it is pinned per counter here rather than
    /// inferred from an end-to-end flow.
    #[tokio::test]
    async fn web_metrics_counters_follow_guard_lifecycle_exactly() {
        let registry = transfer_registry();
        let rooms_before = registry.current_rooms();
        let peers_before = registry.current_peers();
        let relays_before = registry.relay_slots_available();
        let rejected_before = registry.rejected_total();
        let bytes_before = registry.relay_ciphertext_bytes();

        let (lease, room) = transfer_room(&registry);
        assert_eq!(registry.current_rooms(), rooms_before + 1);
        {
            let (_peer, _guard, _rx) = live_peer(&registry, &room, Some("A"));
            assert_eq!(registry.current_peers(), peers_before + 1);
            let permit = registry.try_acquire_relay().expect("a relay slot");
            assert_eq!(registry.relay_slots_available(), relays_before - 1);
            drop(permit);
            assert_eq!(registry.relay_slots_available(), relays_before);
        }
        assert_eq!(registry.current_peers(), peers_before);

        // A REFUSAL is counted once per refusal, and only on a refusal: the
        // counter is what tells an operator a limit is the reason, so a
        // successful acquisition must leave it alone.
        let mut held = Vec::new();
        while let Ok(permit) = registry.try_acquire_relay() {
            held.push(permit);
        }
        // The loop EXITS on a refusal, so that refusal is already counted:
        // the baseline for the assertions below is taken here, after it.
        assert_eq!(
            registry.rejected_total(),
            rejected_before + 1,
            "exhausting the budget refuses exactly once"
        );
        let rejected_saturated = registry.rejected_total();
        assert!(registry.try_acquire_relay().is_err());
        assert_eq!(registry.rejected_total(), rejected_saturated + 1);
        assert!(registry.try_acquire_relay().is_err());
        assert_eq!(registry.rejected_total(), rejected_saturated + 2);
        drop(held);

        // A cumulative total NEVER comes back: the ciphertext counter is not
        // touched by any of the releases above, and the gauges are.
        assert_eq!(registry.relay_ciphertext_bytes(), bytes_before);
        lease.close_explicit(&registry);
        drop(room);
        assert_eq!(registry.current_rooms(), rooms_before);
        assert_eq!(registry.current_offers(), 0);
        assert_eq!(registry.current_transfers(), 0);
        assert_eq!(registry.current_metadata_bytes(), 0);
        assert_eq!(registry.relay_slots_available(), relays_before);
        // The totals survived the whole lifecycle, which is what makes them
        // totals and not gauges.
        assert_eq!(registry.rejected_total(), rejected_saturated + 2);
        assert_eq!(registry.relay_ciphertext_bytes(), bytes_before);
    }

    #[tokio::test]
    async fn cli_owner_has_no_browser_privileges_or_peer_id() {
        let registry = transfer_registry();
        let (lease, room) = transfer_room(&registry);
        // A live lease, and not one peer: nobody is in the room yet.
        assert!(matches!(
            room.state.lock().unwrap().owner,
            OwnerState::Attached { .. }
        ));
        assert!(
            room.state.lock().unwrap().peers.is_empty(),
            "the owner lease must not occupy a peer slot"
        );

        // Two browsers join. The peer list is exactly those two.
        let (first, _guard_a, _rx_a) = live_peer(&registry, &room, Some("A"));
        let (second, _guard_b, _rx_b) = live_peer(&registry, &room, Some("B"));
        {
            let state = room.state.lock().unwrap();
            let mut ids: Vec<PeerId> = state.peers.keys().copied().collect();
            ids.sort_by_key(|id| id.to_string());
            let mut want = vec![first, second];
            want.sort_by_key(|id| id.to_string());
            assert_eq!(ids, want, "the room holds the browsers and nobody else");
        }

        // The FIRST browser has no extra right over the second's offer: the
        // only thing that decides a withdraw is who published it.
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let _mac = offer_fixture(&registry, &room, second, offer_hex);
        let offer_id: OfferId = offer_hex.parse().unwrap();
        let refused = registry
            .withdraw_offer(&room, first, offer_id)
            .expect_err("a peer cannot withdraw another peer's offer");
        assert_eq!(refused.code(), "NOT_PARTICIPANT");
        // And the publisher still can.
        registry.withdraw_offer(&room, second, offer_id).unwrap();

        // Closing the lease is what ends the room — the peers do not own it.
        lease.close_explicit(&registry);
        assert!(registry.room(room.id).is_none());
    }

    /// `concurrent_transfer_cleanup_isolated_by_transfer_id`: two transfers
    /// live at once, and cancelling one must reach exactly one record and
    /// exactly one counterpart. The cleanup path is keyed by transfer ID and
    /// not by peer or offer, which is the property this pins: the two
    /// transfers here deliberately SHARE their source peer and differ in
    /// mode, so a cleanup that keyed on either would take both down.
    #[tokio::test]
    async fn concurrent_transfer_cleanup_isolated_by_transfer_id() {
        let file_hex = "cccccccccccccccccccccccccccccccc";
        let folder_hex = "dddddddddddddddddddddddddddddddd";
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, _rx_a) = live_peer(&registry, &room, Some("A"));
        let (b, _guard_b, _rx_b) = live_peer(&registry, &room, Some("B"));
        let (c, _guard_c, _rx_c) = live_peer(&registry, &room, Some("C"));
        let file_mac = offer_fixture(&registry, &room, source, file_hex);
        let folder_mac = folder_offer_fixture(&registry, &room, source, folder_hex);
        let file_id: OfferId = file_hex.parse().unwrap();
        let folder_id: OfferId = folder_hex.parse().unwrap();

        let raw_ids = vec!["0".to_string()];
        let (raw_transfer, _) = registry
            .request_transfer(
                &room,
                b,
                file_id,
                raw_ids.clone(),
                selection_digest(&file_id, &file_mac, &raw_ids, "raw"),
                "raw",
                None,
            )
            .unwrap();
        let zip_ids = vec!["0".to_string(), "1".to_string(), "2".to_string()];
        let (zip_transfer, _) = registry
            .request_transfer(
                &room,
                c,
                folder_id,
                zip_ids.clone(),
                selection_digest(&folder_id, &folder_mac, &zip_ids, "zip"),
                "zip",
                None,
            )
            .unwrap();
        assert_ne!(raw_transfer, zip_transfer);
        assert_eq!(room.state.lock().unwrap().transfers.len(), 2);

        // B cancels its own. Exactly one notice, and it goes to the SOURCE —
        // C is a stranger to this transfer and hears nothing.
        let (outcome, outbox) = registry.cancel_transfer(&room, b, raw_transfer).unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].0, source);

        {
            let state = room.state.lock().unwrap();
            let cancelled = state.transfers.get(&raw_transfer).unwrap();
            assert!(!cancelled.state.is_live(), "the cancelled one is terminal");
            let survivor = state.transfers.get(&zip_transfer).unwrap();
            assert!(survivor.state.is_live(), "the other one is untouched");
            assert_eq!(survivor.recipient, c);
            assert_eq!(survivor.source, source);
        }

        // A stranger still cannot cancel the survivor, and the survivor's own
        // recipient still can — the cancel of the first changed neither.
        assert_eq!(
            registry
                .cancel_transfer(&room, b, zip_transfer)
                .expect_err("a stranger cannot cancel")
                .code(),
            "NOT_PARTICIPANT"
        );
        let (outcome, outbox) = registry.cancel_transfer(&room, c, zip_transfer).unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].0, source);
    }

    #[tokio::test]
    async fn request_requires_recipient_click_source_online_and_single_file() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, _rx_b) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let offer_id: OfferId = offer_hex.parse().unwrap();
        let digest = selection_digest(&offer_id, &mac, &["0".to_string()], "raw");
        // Unknown offer.
        assert_eq!(
            registry
                .request_transfer(
                    &room,
                    recipient,
                    "dddddddddddddddddddddddddddddddd".parse().unwrap(),
                    vec!["0".to_string()],
                    digest,
                    "raw",
                    None,
                )
                .unwrap_err()
                .code(),
            "OFFER_NOT_FOUND"
        );
        // Source joined but without a live session.
        let ghost = generate_peer_id();
        let _guard_ghost = registry.join_peer(&room, ghost, None).unwrap();
        let ghost_offer = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        offer_fixture(&registry, &room, ghost, ghost_offer);
        let ghost_digest = selection_digest(
            &ghost_offer.parse().unwrap(),
            &mac,
            &["0".to_string()],
            "raw",
        );
        assert_eq!(
            registry
                .request_transfer(
                    &room,
                    recipient,
                    ghost_offer.parse().unwrap(),
                    vec!["0".to_string()],
                    ghost_digest,
                    "raw",
                    None,
                )
                .unwrap_err()
                .code(),
            "SOURCE_OFFLINE"
        );
        // Mode, entry count and entry identity.
        for (ids, mode) in [
            (vec!["0".to_string(), "1".to_string()], "raw"),
            (vec!["7".to_string()], "raw"),
            (vec!["0".to_string()], "zip"),
        ] {
            assert!(registry
                .request_transfer(&room, recipient, offer_id, ids, digest, mode, None)
                .is_err());
        }
        // Lying digest.
        let mut bad_digest = digest;
        bad_digest[0] ^= 1;
        assert_eq!(
            registry
                .request_transfer(
                    &room,
                    recipient,
                    offer_id,
                    vec!["0".to_string()],
                    bad_digest,
                    "raw",
                    None
                )
                .unwrap_err()
                .code(),
            "SOURCE_CHANGED"
        );
        // Valid request: recorded plus incoming on the source queue.
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let (typ, body) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.incoming");
        assert_eq!(body["transferId"].as_str(), Some(id.to_string()).as_deref());
        assert_eq!(body["offerId"].as_str(), Some(offer_hex));
        assert_eq!(
            body["fromPeerId"].as_str(),
            Some(recipient.to_string()).as_deref()
        );
        assert_eq!(
            body["attemptId"].as_str(),
            Some(attempt.to_string()).as_deref()
        );
        let state = room.state.lock().unwrap().transfers.get(&id).unwrap().state;
        assert_eq!(state, TransferState::WaitingSource);
        assert_eq!(registry.current_transfers(), 1);
    }

    #[tokio::test]
    async fn source_ready_must_match_source_and_selection_digest() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, _rx_b) = live_peer(&registry, &room, Some("B"));
        let (stranger, _guard_c, _rx_c) = live_peer(&registry, &room, Some("C"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        // Wrong role and strangers.
        assert_eq!(
            registry
                .source_ready(&room, recipient, id, attempt, digest)
                .unwrap_err()
                .code(),
            "INVALID_MESSAGE"
        );
        assert_eq!(
            registry
                .source_ready(&room, stranger, id, attempt, digest)
                .unwrap_err()
                .code(),
            "NOT_PARTICIPANT"
        );
        // Stale attempt and lying digest.
        assert!(registry
            .source_ready(&room, source, id, generate_attempt_id(), digest)
            .is_err());
        let mut bad = digest;
        bad[1] ^= 1;
        assert_eq!(
            registry
                .source_ready(&room, source, id, attempt, bad)
                .unwrap_err()
                .code(),
            "SOURCE_CHANGED"
        );
        // Correct ready admits inline (free budget) with distinct tickets.
        let (outcome, _attempt, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let (typ, source_ticket) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.relay_ticket");
        assert_eq!(
            source_ticket["transferId"].as_str(),
            Some(id.to_string()).as_deref()
        );
        let ticket_a = source_ticket["ticket"].as_str().unwrap().to_string();
        assert_eq!(ticket_a.len(), 32);
        assert!(
            room.state.lock().unwrap().transfers.get(&id).unwrap().state
                == TransferState::WaitingRelay
        );
        assert_eq!(registry.current_relays(), 1);
        let _ = ticket_a;
    }

    #[tokio::test]
    async fn active_caps_reserve_both_peers_and_roll_back() {
        let registry = tight_transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (a, _ga, _rx_a) = live_peer(&registry, &room, Some("A"));
        let (b, _gb, _rx_b) = live_peer(&registry, &room, Some("B"));
        let (c, _gc, _rx_c) = live_peer(&registry, &room, Some("C"));
        offer_fixture(&registry, &room, a, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        offer_fixture(&registry, &room, c, "cccccccccccccccccccccccccccccccc");
        let mac = [0xeeu8; 32];
        let digest_a = selection_digest(
            &"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
            &mac,
            &["0".to_string()],
            "raw",
        );
        request_fixture(
            &registry,
            &room,
            b,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &mac,
        );
        assert_eq!(registry.current_transfers(), 1);
        // Source at cap.
        let digest_c = selection_digest(
            &"cccccccccccccccccccccccccccccccc".parse().unwrap(),
            &mac,
            &["0".to_string()],
            "raw",
        );
        assert_eq!(
            registry
                .request_transfer(
                    &room,
                    c,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
                    vec!["0".to_string()],
                    digest_a,
                    "raw",
                    None,
                )
                .unwrap_err()
                .code(),
            "LIMIT_EXCEEDED"
        );
        // Recipient at cap.
        assert_eq!(
            registry
                .request_transfer(
                    &room,
                    b,
                    "cccccccccccccccccccccccccccccccc".parse().unwrap(),
                    vec!["0".to_string()],
                    digest_c,
                    "raw",
                    None,
                )
                .unwrap_err()
                .code(),
            "LIMIT_EXCEEDED"
        );
        assert_eq!(registry.current_transfers(), 1);
        assert_eq!(room.state.lock().unwrap().transfers.len(), 1);
    }

    #[tokio::test]
    async fn relay_busy_is_bounded_and_retryable() {
        let registry = WebTransferRegistry::new(
            WebTransferConfig::new(
                WebTransferBaseUrl::parse("http://127.0.0.1:8080/").unwrap(),
                WebTransferLimits {
                    max_relays_global: 1,
                    ..WebTransferLimits::default()
                },
                IceServerConfig {
                    servers: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        registry.set_relay_admit_timeout(Duration::from_millis(50));
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        // Saturate the single relay slot outside the transfer.
        let _held = registry.try_acquire_relay().unwrap();
        let (id, attempt1) = request_fixture(&registry, &room, recipient, offer_hex, &mac);

        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, attempt1, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt1, digest);
        assert_eq!(outcome, ReadyOutcome::Queued);
        assert!(outbox.is_empty());
        // Admission times out fast: busy notice, still resumable, no permit.
        registry.admit_relay(&room, id, attempt1).await;
        let (typ, body) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "error");
        assert_eq!(body["code"].as_str(), Some("RELAY_BUSY"));
        assert_eq!(body["message"].as_str(), Some(id.to_string()).as_deref());
        let (state_now, has_permit) = {
            let guard = room.state.lock().unwrap();
            let record = guard.transfers.get(&id).unwrap();
            (
                record.state,
                record.attempt.as_ref().unwrap().relay_permit.is_some(),
            )
        };
        assert_eq!(state_now, TransferState::WaitingRelay);
        assert!(!has_permit);
        // Free the slot and retry with a fresh request: new attempt, tickets.
        drop(_held);
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (retry_id, _) = registry
            .request_transfer(
                &room,
                recipient,
                offer_hex.parse().unwrap(),
                vec!["0".to_string()],
                digest,
                "raw",
                None,
            )
            .unwrap();
        assert_eq!(retry_id, id);
        let attempt2 = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .attempt_id
            .unwrap();
        assert_ne!(attempt1, attempt2);
        let _ = recv_text(&mut source_rx).await;
        let (outcome, _attempt2, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt2, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.relay_ticket");
        let (typ, _) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.relay_ticket");
        assert_eq!(registry.current_relays(), 1);
    }

    #[tokio::test]
    async fn tickets_are_distinct_role_bound_hashed_one_use_and_expire() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, attempt, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let (_, source_body) = recv_text(&mut source_rx).await;
        let (_, recipient_body) = recv_text(&mut recipient_rx).await;
        let ticket_a = source_body["ticket"].as_str().unwrap().to_string();
        let ticket_b = recipient_body["ticket"].as_str().unwrap().to_string();
        assert_ne!(ticket_a, ticket_b);
        let now = Instant::now();
        // Hash-only storage: keys are digests, values carry no raw ticket.
        let parsed_a: RelayTicket = ticket_a.parse().unwrap();
        assert!(registry.inner.tickets.contains_key(&parsed_a.sha256_hash()));
        // Correct use grants once; replay is unknown (burned on first use).
        let grant = registry
            .consume_relay_ticket(&ticket_a, RelayRole::Source, source, now)
            .unwrap();
        assert_eq!(grant.transfer_id, id);
        assert_eq!(grant.attempt_id, attempt);
        assert_eq!(
            registry
                .consume_relay_ticket(&ticket_a, RelayRole::Source, source, now)
                .unwrap_err(),
            TicketDeny::Unknown
        );
        // Wrong leg burns the ticket too.
        assert_eq!(
            registry
                .consume_relay_ticket(&ticket_b, RelayRole::Source, recipient, now)
                .unwrap_err(),
            TicketDeny::RoleMismatch
        );
        assert_eq!(
            registry
                .consume_relay_ticket(&ticket_b, RelayRole::Recipient, recipient, now)
                .unwrap_err(),
            TicketDeny::Unknown
        );
        // Expiry is a pure clock comparison.
        let offer2 = "dddddddddddddddddddddddddddddddd";
        let (id2, attempt2) = {
            offer_fixture(&registry, &room, source, offer2);
            request_fixture(&registry, &room, recipient, offer2, &mac)
        };
        let _ = recv_text(&mut source_rx).await;
        let digest2 = selection_digest(&offer2.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, _attempt2, outbox) =
            ready_then_relay(&registry, &room, source, id2, attempt2, digest2);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let (_, fresh_body) = recv_text(&mut source_rx).await;
        let fresh = fresh_body["ticket"].as_str().unwrap().to_string();
        assert_eq!(
            registry
                .consume_relay_ticket(
                    &fresh,
                    RelayRole::Source,
                    source,
                    now + Duration::from_secs(31)
                )
                .unwrap_err(),
            TicketDeny::Expired
        );
    }

    #[tokio::test]
    async fn attempt_ids_never_repeat_and_increment_checked() {
        assert!(next_attempt_number(u64::MAX).is_err());
        assert_eq!(next_attempt_number(1).unwrap(), 2);
        let registry = busy_registry();
        registry.set_relay_admit_timeout(Duration::from_millis(20));
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let _held = registry.try_acquire_relay().unwrap();
        // Attempt 1 is the DIRECT attempt; its fallback mints attempt 2,
        // which queues behind the held budget (256 default would admit).
        let (id, attempt1) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, attempt2, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt1, digest);
        assert_eq!(outcome, ReadyOutcome::Queued);
        assert!(outbox.is_empty());
        assert_ne!(attempt1, attempt2);
        // Drain the busy notice so later reads stay aligned.
        registry.admit_relay(&room, id, attempt2).await;
        let (typ, _) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "error");
        drop(_held);
        // Retry mints attempt 3 with a fresh ID; both older ones are stale.
        let (retry_id, _) = registry
            .request_transfer(
                &room,
                recipient,
                offer_hex.parse().unwrap(),
                vec!["0".to_string()],
                digest,
                "raw",
                None,
            )
            .unwrap();
        assert_eq!(retry_id, id);
        // Neither stale attempt readies.
        assert!(registry
            .source_ready(&room, source, id, attempt1, digest)
            .is_err());
        assert!(registry
            .source_ready(&room, source, id, attempt2, digest)
            .is_err());
        let (attempt_number, attempt_now) = {
            let guard = room.state.lock().unwrap();
            let record = guard.transfers.get(&id).unwrap();
            (record.attempt_number, record.attempt_id.unwrap())
        };
        assert_eq!(attempt_number, 3);
        assert_ne!(attempt_now, attempt1);
        assert_ne!(attempt_now, attempt2);
    }

    #[tokio::test]
    async fn only_participants_cancel() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let (stranger, _guard_c, _rx_c) = live_peer(&registry, &room, Some("C"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, _attempt, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        let _ = recv_text(&mut recipient_rx).await;
        // Stranger hears NOT_PARTICIPANT and nothing moves.
        assert_eq!(
            registry
                .cancel_transfer(&room, stranger, id)
                .unwrap_err()
                .code(),
            "NOT_PARTICIPANT"
        );
        assert!(recipient_rx.try_recv().is_err());
        // Source cancels: recipient is told exactly once.
        let (outcome, outbox) = registry.cancel_transfer(&room, source, id).unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        drain_transfer_outbox(&room, outbox);
        let (typ, body) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.cancelled");
        assert_eq!(
            body["byPeerId"].as_str(),
            Some(source.to_string()).as_deref()
        );
        assert_eq!(registry.current_transfers(), 0);
        assert_eq!(registry.current_relays(), 0);
    }

    #[tokio::test]
    async fn withdraw_disconnect_and_room_close_cancel_related_transfers() {
        // Withdraw cancels the offer's transfers.
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, attempt, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        let _ = recv_text(&mut recipient_rx).await;
        registry
            .withdraw_offer(&room, source, offer_hex.parse().unwrap())
            .unwrap();
        let (typ, body) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.cancelled");
        assert_eq!(
            body["byPeerId"].as_str(),
            Some(source.to_string()).as_deref()
        );
        assert_eq!(registry.current_transfers(), 0);
        assert_eq!(registry.current_relays(), 0);
        // Disconnect cancels too: fresh room, live transfer, drop the source.
        let (_lease, room) = transfer_room(&registry);
        let (source, guard_a, _source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let _ = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        drop(guard_a);
        let (typ, _) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.cancelled");
        assert_eq!(registry.current_transfers(), 0);
        assert_eq!(registry.current_relays(), 0);
        let _ = (id, attempt);
        // Room destroy plus guard drops converge on released counters.
        // Earlier sub-cases still hold their guards, so assert the delta
        // against the live baseline, not absolute zero.
        let peers_before = registry.current_peers();
        assert_eq!(registry.current_metadata_bytes(), 0);
        assert_eq!(registry.current_transfers(), 0);
        let (_lease, room) = transfer_room(&registry);
        let (source, guard_a, _source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, guard_b, _recipient_rx) = live_peer(&registry, &room, Some("B"));
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let _ = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        room.destroy("owner-close");
        drop(guard_a);
        drop(guard_b);
        assert_eq!(registry.current_transfers(), 0);
        assert_eq!(registry.current_metadata_bytes(), 0);
        assert_eq!(registry.current_peers(), peers_before);
    }

    #[tokio::test]
    async fn terminal_transitions_release_permits_exactly_once() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, _attempt, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        let _ = recv_text(&mut recipient_rx).await;
        assert_eq!(registry.current_relays(), 1);
        // Cancel releases once; the repeat acks without resending.
        let (outcome, outbox) = registry.cancel_transfer(&room, recipient, id).unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        drain_transfer_outbox(&room, outbox);
        assert_eq!(registry.current_transfers(), 0);
        assert_eq!(registry.current_relays(), 0);
        assert!(registry.inner.tickets.is_empty());
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.cancelled");
        let (outcome, outbox) = registry.cancel_transfer(&room, recipient, id).unwrap();
        assert_eq!(outcome, CancelOutcome::AlreadyTerminal);
        assert!(outbox.is_empty());
        assert!(source_rx.try_recv().is_err());
        assert_eq!(registry.current_transfers(), 0);
        assert_eq!(registry.current_relays(), 0);
        // Reject path releases the same way.
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let outbox = registry.source_reject(&room, source, id).unwrap();
        drain_transfer_outbox(&room, outbox);
        assert_eq!(registry.current_transfers(), 0);
        let (typ, _) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.cancelled");
        let _ = attempt;
        // Direct failure releases without notices (3.2 owns failure notices).
        let (id, _) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        assert!(registry.fail_transfer(&room, id));
        assert!(!registry.fail_transfer(&room, id));
        assert_eq!(registry.current_transfers(), 0);
        // Complete before Active is rejected, state untouched (3.2 owns it).
        let (id, attempt) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let root = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .entry_root
            .expect("a raw transfer carries the manifest root");
        assert!(registry
            .complete_transfer(&room, recipient, id, attempt, root)
            .is_err());
        assert!(room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .state
            .is_live());
    }

    #[tokio::test]
    async fn stale_attempt_messages_do_not_change_current_state() {
        let registry = busy_registry();
        registry.set_relay_admit_timeout(Duration::from_millis(20));
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let _held = registry.try_acquire_relay().unwrap();
        let (id, attempt1) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        // The direct attempt falls back to a relay attempt that queues.
        let (outcome, relay_attempt, outbox) =
            ready_then_relay(&registry, &room, source, id, attempt1, digest);
        assert_eq!(outcome, ReadyOutcome::Queued);
        assert!(outbox.is_empty());
        registry.admit_relay(&room, id, relay_attempt).await;
        let _ = recv_text(&mut recipient_rx).await;
        drop(_held);
        // Retry upgrades to a fresh attempt; both older ones are now stale.
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (retry_id, _) = registry
            .request_transfer(
                &room,
                recipient,
                offer_hex.parse().unwrap(),
                vec!["0".to_string()],
                digest,
                "raw",
                None,
            )
            .unwrap();
        assert_eq!(retry_id, id);
        let before = format!("{:?}", room.state.lock().unwrap().transfers.get(&id));
        for stale in [attempt1, relay_attempt] {
            assert!(registry
                .source_ready(&room, source, id, stale, digest)
                .is_err());
            assert!(registry
                .complete_transfer(&room, recipient, id, stale, [0u8; 32])
                .is_err());
        }
        // A fresh ready for the current attempt still works afterwards.
        let current = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .attempt_id
            .unwrap();
        assert_ne!(attempt1, current);
        assert_ne!(relay_attempt, current);
        let (outcome, _next, outbox) =
            ready_then_relay(&registry, &room, source, id, current, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        assert_eq!(outbox.len(), 2);
        drain_transfer_outbox(&room, outbox);
        let after = format!("{:?}", room.state.lock().unwrap().transfers.get(&id));
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn terminal_cache_is_bounded_and_idempotent() {
        // Covered at the actor layer (same rid replays bytes); here the
        // registry proves terminal repeats stay side-effect free.
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _guard_a, mut source_rx) = live_peer(&registry, &room, Some("A"));
        let (recipient, _guard_b, mut recipient_rx) = live_peer(&registry, &room, Some("B"));
        let offer_hex = "cccccccccccccccccccccccccccccccc";
        let mac = offer_fixture(&registry, &room, source, offer_hex);
        let (id, _) = request_fixture(&registry, &room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let (outcome, outbox) = registry.cancel_transfer(&room, recipient, id).unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        // Repeating the terminal cancel changes nothing and resends nothing.
        let (outcome, outbox) = registry.cancel_transfer(&room, recipient, id).unwrap();
        assert_eq!(outcome, CancelOutcome::AlreadyTerminal);
        assert!(outbox.is_empty());
        assert!(source_rx.try_recv().is_err());
        assert!(recipient_rx.try_recv().is_err());
        assert_eq!(registry.current_transfers(), 0);
    }
    // ---- Phase 3.2 relay tests: MemSocket halves + live-pair helper ----

    use futures_util::{Sink, Stream};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// In-memory relay half: programmable inbound, recorded outbox, polled
    /// counters and controllable sink readiness. Both boxed halves may share
    /// one state; tests read it back after the pump drops its boxes.
    #[derive(Clone, Default)]
    struct MemSocket {
        state: std::sync::Arc<std::sync::Mutex<MemState>>,
    }

    #[derive(Default)]
    struct MemState {
        inbound: VecDeque<Message>,
        polls: usize,
        outbox: Vec<Message>,
        sink_ready: bool,
        send_polls: usize,
    }

    impl MemSocket {
        fn with_inbound(messages: Vec<Message>) -> Self {
            Self {
                state: std::sync::Arc::new(std::sync::Mutex::new(MemState {
                    inbound: messages.into(),
                    ..Default::default()
                })),
            }
        }

        fn ready_sink() -> Self {
            Self {
                state: std::sync::Arc::new(std::sync::Mutex::new(MemState {
                    sink_ready: true,
                    ..Default::default()
                })),
            }
        }

        fn boxed_pair(self) -> (RelaySink, RelayStream) {
            (
                Box::pin(self.clone()) as RelaySink,
                Box::pin(self) as RelayStream,
            )
        }

        fn polls(&self) -> usize {
            self.state.lock().unwrap().polls
        }

        fn outbox(&self) -> Vec<Message> {
            self.state.lock().unwrap().outbox.clone()
        }

        fn inbound_len(&self) -> usize {
            self.state.lock().unwrap().inbound.len()
        }
    }

    impl Stream for MemSocket {
        type Item = Result<Message, WsError>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let mut state = self.state.lock().unwrap();
            state.polls += 1;
            match state.inbound.pop_front() {
                Some(message) => Poll::Ready(Some(Ok(message))),
                None => Poll::Pending,
            }
        }
    }

    impl Sink<Message> for MemSocket {
        type Error = WsError;

        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            let mut state = self.state.lock().unwrap();
            state.send_polls += 1;
            if state.sink_ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), WsError> {
            self.state.lock().unwrap().outbox.push(item);
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            self.state.lock().unwrap().outbox.push(Message::Close(None));
            Poll::Ready(Ok(()))
        }
    }

    /// Builds one encrypted-frame wire image (header valid, body opaque
    /// filler — the pump never decrypts, so filler proves the no-decrypt
    /// property when the frame still forwards).
    fn relay_frame(seq: u32, body_len: usize, frame_type: u8) -> Vec<u8> {
        let mut frame = Vec::with_capacity(16 + body_len);
        frame.extend_from_slice(&RELAY_FRAME_MAGIC.to_be_bytes());
        frame.extend_from_slice(&RELAY_FRAME_VERSION.to_be_bytes());
        frame.push(frame_type);
        frame.push(0);
        frame.extend_from_slice(&seq.to_be_bytes());
        frame.extend_from_slice(&(body_len as u32).to_be_bytes());
        frame.extend(std::iter::repeat_n(0xABu8, body_len));
        frame
    }

    /// Drives one transfer to admitted-with-tickets over the real registry,
    /// returning both control receivers so tests drain the exact envelopes.
    async fn relay_live_pair(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        offer_hex: &str,
    ) -> (
        PeerId,
        PeerGuard,
        PeerId,
        PeerGuard,
        mpsc::Receiver<String>,
        mpsc::Receiver<String>,
        TransferId,
        AttemptId,
        String,
        String,
    ) {
        let (source, guard_a, mut source_rx) = live_peer(registry, room, Some("A"));
        let (recipient, guard_b, mut recipient_rx) = live_peer(registry, room, Some("B"));
        let mac = offer_fixture(registry, room, source, offer_hex);
        let (id, attempt) = request_fixture(registry, room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, attempt, outbox) =
            ready_then_relay(registry, room, source, id, attempt, digest);
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(room, outbox);
        let (_, source_body) = recv_text(&mut source_rx).await;
        let (_, recipient_body) = recv_text(&mut recipient_rx).await;
        let source_ticket = source_body["ticket"].as_str().unwrap().to_string();
        let recipient_ticket = recipient_body["ticket"].as_str().unwrap().to_string();
        (
            source,
            guard_a,
            recipient,
            guard_b,
            source_rx,
            recipient_rx,
            id,
            attempt,
            source_ticket,
            recipient_ticket,
        )
    }

    /// Asserts a denied join (boxed halves are not `Debug` by design, so
    /// no `unwrap_err` on the join result).
    fn assert_join_denied(result: Result<RelayJoin, WebTransferError>) {
        match result {
            Ok(_) => panic!("denied join unexpectedly parked or paired"),
            Err(error) => assert_eq!(error.code(), "TRANSFER_NOT_FOUND"),
        }
    }

    fn relay_attach_body(
        peer: PeerId,
        transfer: TransferId,
        attempt: AttemptId,
        role: &str,
        ticket: &str,
    ) -> crate::web_transfer_protocol::RelayAttach {
        let raw = serde_json::json!({
            "v": 1,
            "peerId": peer.to_string(),
            "transferId": transfer.to_string(),
            "attemptId": attempt.to_string(),
            "role": role,
            "ticket": ticket,
        })
        .to_string();
        crate::web_transfer_protocol::parse_relay_attach(&raw).unwrap()
    }

    #[tokio::test]
    async fn relay_attach_requires_first_text_message_and_exact_identity() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, _srx, _rrx, id, attempt, src_ticket, rcpt_ticket) =
            relay_live_pair(&registry, &room, "cccccccccccccccccccccccccccccccc").await;
        // Happy path first: park the source leg, pair the recipient leg.
        let attach = relay_attach_body(source, id, attempt, "source", &src_ticket);
        let (park_sink, park_stream) = MemSocket::default().boxed_pair();
        let join = registry
            .join_relay_pair(&room, &attach, &src_ticket, park_sink, park_stream)
            .unwrap();
        assert!(matches!(join, RelayJoin::Park { .. }));
        let attach = relay_attach_body(recipient, id, attempt, "recipient", &rcpt_ticket);
        let (sink, stream) = MemSocket::default().boxed_pair();
        let join = registry
            .join_relay_pair(&room, &attach, &rcpt_ticket, sink, stream)
            .unwrap();
        assert!(matches!(join, RelayJoin::Pair { .. }));
        // The waiter is gone with the pair: no third leg can follow.
        assert!(!registry.abandon_relay_waiter(id, attempt));
        // The pre-pair phase accepts a TEXT attach and nothing else: the
        // parser the loop calls refuses anything that is not a JSON attach
        // object, so a binary or garbage first message can never produce one
        // (the transport-level "text only" branch is pinned end to end by
        // `t_web_relay_opaque`).
        for hostile in [
            String::from_utf8_lossy(&relay_frame(0, 32, 1)).into_owned(),
            String::new(),
            "not json".to_string(),
            serde_json::json!({ "v": 1, "role": "source" }).to_string(),
        ] {
            assert!(
                crate::web_transfer_protocol::parse_relay_attach(&hostile).is_err(),
                "a non-attach first message must never parse"
            );
        }
        // Deny shapes, one fresh live pair each (a failed presentment burns
        // its ticket, so cases cannot share a pair).
        deny_replay(&registry, &room).await;
        deny_swapped_role(&registry, &room).await;
        deny_incoherent_body(&registry, &room).await;
        deny_stranger(&registry, &room).await;
    }

    /// Same ticket twice: the atomic consume burned it on first use.
    async fn deny_replay(registry: &WebTransferRegistry, room: &Arc<WebTransferRoom>) {
        let (source, _ga, _rcpt, _gb, _srx, _rrx, id, attempt, src_ticket, _rt) =
            relay_live_pair(registry, room, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").await;
        let attach = relay_attach_body(source, id, attempt, "source", &src_ticket);
        let (sink, stream) = MemSocket::default().boxed_pair();
        assert!(matches!(
            registry
                .join_relay_pair(room, &attach, &src_ticket, sink, stream)
                .unwrap(),
            RelayJoin::Park { .. }
        ));
        let attach = relay_attach_body(source, id, attempt, "source", &src_ticket);
        let (sink, stream) = MemSocket::default().boxed_pair();
        assert_join_denied(registry.join_relay_pair(room, &attach, &src_ticket, sink, stream));
        assert!(registry.abandon_relay_waiter(id, attempt));
    }

    /// Presenting the recipient ticket as the source denies (role-bound).
    async fn deny_swapped_role(registry: &WebTransferRegistry, room: &Arc<WebTransferRoom>) {
        let (source, _ga, _rcpt, _gb, _srx, _rrx, id, attempt, _st, rcpt_ticket) =
            relay_live_pair(registry, room, "dddddddddddddddddddddddddddddddd").await;
        let attach = relay_attach_body(source, id, attempt, "source", &rcpt_ticket);
        let (sink, stream) = MemSocket::default().boxed_pair();
        assert_join_denied(registry.join_relay_pair(room, &attach, &rcpt_ticket, sink, stream));
    }

    /// A body disagreeing with its ticket denies even with a live ticket:
    /// right peer and role, but an attempt ID the ticket was not cut for.
    async fn deny_incoherent_body(registry: &WebTransferRegistry, room: &Arc<WebTransferRoom>) {
        let (_source, _ga, recipient, _gb, _srx, _rrx, id, _attempt, _st, rcpt_ticket) =
            relay_live_pair(registry, room, "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").await;
        let attach = relay_attach_body(
            recipient,
            id,
            generate_attempt_id(),
            "recipient",
            &rcpt_ticket,
        );
        let (sink, stream) = MemSocket::default().boxed_pair();
        assert_join_denied(registry.join_relay_pair(room, &attach, &rcpt_ticket, sink, stream));
    }

    /// A stranger presenting a stolen ticket hex denies as unknown.
    async fn deny_stranger(registry: &WebTransferRegistry, room: &Arc<WebTransferRoom>) {
        let (_source, _ga, _rcpt, _gb, _srx, _rrx, id, attempt, src_ticket, _rt) =
            relay_live_pair(registry, room, "ffffffffffffffffffffffffffffffff").await;
        let stranger = generate_peer_id();
        let attach = relay_attach_body(stranger, id, attempt, "source", &src_ticket);
        let (sink, stream) = MemSocket::default().boxed_pair();
        assert_join_denied(registry.join_relay_pair(room, &attach, &src_ticket, sink, stream));
    }

    #[tokio::test]
    async fn relay_ticket_is_consumed_atomically() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, _srx, _recipient, _gb, _rrx, id, attempt, src_ticket, _rcpt) =
            relay_live_pair(&registry, &room, "cccccccccccccccccccccccccccccccc").await;
        let attach = relay_attach_body(source, id, attempt, "source", &src_ticket);
        let raw = serde_json::json!({
            "v": 1,
            "peerId": source.to_string(),
            "transferId": id.to_string(),
            "attemptId": attempt.to_string(),
            "role": "source",
            "ticket": src_ticket,
        })
        .to_string();
        let _ = attach;
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let parsed = crate::web_transfer_protocol::parse_relay_attach(&raw).unwrap();
                let (sink, stream) = MemSocket::default().boxed_pair();
                registry
                    .join_relay_pair(&room, &parsed, &parsed.ticket.to_string(), sink, stream)
                    .map(|join| matches!(join, RelayJoin::Park { .. }))
            });
            let second = scope.spawn(|| {
                let parsed = crate::web_transfer_protocol::parse_relay_attach(&raw).unwrap();
                let (sink, stream) = MemSocket::default().boxed_pair();
                registry
                    .join_relay_pair(&room, &parsed, &parsed.ticket.to_string(), sink, stream)
                    .map(|join| matches!(join, RelayJoin::Park { .. }))
            });
            let (first, second) = (first.join().unwrap(), second.join().unwrap());
            // Exactly one thread parks; the loser meets the burned ticket.
            assert_ne!(
                first.is_ok(),
                second.is_ok(),
                "atomic consume grants exactly once"
            );
            for result in [first, second] {
                match result {
                    Ok(true) => {}
                    Err(e) => assert_eq!(e.code(), "TRANSFER_NOT_FOUND"),
                    Ok(false) => panic!("both threads paired with one ticket"),
                }
            }
        });
        assert!(registry.abandon_relay_waiter(id, attempt));
    }

    #[test]
    fn relay_checks_magic_and_sequence_without_decrypting() {
        // Filler body is invalid AEAD input, yet the header still passes:
        // the pump never holds a key.
        assert_eq!(check_relay_frame(None, &relay_frame(0, 1, 1)), Ok(0));
        assert_eq!(check_relay_frame(Some(0), &relay_frame(1, 24576, 1)), Ok(1));
        assert_eq!(check_relay_frame(Some(41), &relay_frame(42, 16, 2)), Ok(42));
        // First frame must be 0; replays, gaps and wraps refuse.
        assert_eq!(
            check_relay_frame(None, &relay_frame(1, 1, 1)),
            Err(RelayFrameReject::StaleSeq)
        );
        assert_eq!(
            check_relay_frame(Some(7), &relay_frame(7, 1, 1)),
            Err(RelayFrameReject::StaleSeq)
        );
        assert_eq!(
            check_relay_frame(Some(7), &relay_frame(9, 1, 1)),
            Err(RelayFrameReject::StaleSeq)
        );
        assert_eq!(
            check_relay_frame(Some(u32::MAX), &relay_frame(0, 1, 1)),
            Err(RelayFrameReject::StaleSeq)
        );
        // Shape faults, each distinct.
        assert_eq!(
            check_relay_frame(None, &[0u8; 16]),
            Err(RelayFrameReject::TooShort)
        );
        assert_eq!(
            check_relay_frame(None, &vec![0u8; 32769]),
            Err(RelayFrameReject::Oversize)
        );
        let mut bad = relay_frame(0, 1, 1);
        bad[0] ^= 1;
        assert_eq!(
            check_relay_frame(None, &bad),
            Err(RelayFrameReject::BadMagic)
        );
        let mut bad = relay_frame(0, 1, 1);
        bad[4] ^= 1;
        assert_eq!(
            check_relay_frame(None, &bad),
            Err(RelayFrameReject::BadVersion)
        );
        let mut bad = relay_frame(0, 1, 1);
        bad[6] = 3;
        assert_eq!(
            check_relay_frame(None, &bad),
            Err(RelayFrameReject::BadType)
        );
        let mut bad = relay_frame(0, 1, 1);
        bad[7] = 1;
        assert_eq!(
            check_relay_frame(None, &bad),
            Err(RelayFrameReject::BadFlags)
        );
        let mut bad = relay_frame(0, 1, 1);
        bad[12] ^= 1;
        assert_eq!(
            check_relay_frame(None, &bad),
            Err(RelayFrameReject::LengthMismatch)
        );
    }

    #[tokio::test]
    async fn room_token_bucket_rate_and_burst_are_exact() {
        // Default 100 MiB/s: burst is exactly 2×rate (200 MiB cap not hit).
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let burst = room.relay_throttle.lock().unwrap().burst();
        assert_eq!(burst, 2.0 * 104857600.0);
        // Unit math on a small bucket: full burst covers, then exact delay.
        let mut bucket = TokenBucket::new(1000.0, 2000.0);
        let now = Instant::now();
        assert_eq!(bucket.take_bytes(now, 1500), Duration::ZERO);
        assert_eq!(bucket.take_bytes(now, 1000), Duration::from_millis(500));
        // Debt clamps at one burst: a huge frame waits 2 s, not longer.
        assert_eq!(bucket.take_bytes(now, 10000), Duration::from_secs(2));
        assert_eq!(bucket.take_bytes(now, 10000), Duration::from_secs(2));
    }

    #[test]
    fn zero_rate_disables_throttling() {
        let mut throttle = RelayThrottle::new(0);
        assert_eq!(throttle.burst(), 0.0);
        assert_eq!(throttle.delay_for(Instant::now(), u64::MAX), Duration::ZERO);
    }

    #[tokio::test]
    async fn relay_holds_at_most_one_application_frame() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, _srx, _rrx, id, attempt, _st, _rt) =
            relay_live_pair(&registry, &room, "cccccccccccccccccccccccccccccccc").await;
        let token = registry.activate_relay(&room, id, attempt).unwrap();
        // Two frames queued, recipient never drains: the pump must read the
        // first, park on the blocked send, and never touch the second.
        let source_stream = MemSocket::with_inbound(vec![
            Message::Binary(relay_frame(0, 64, 1).into()),
            Message::Binary(relay_frame(1, 64, 1).into()),
        ]);
        let source_sink = MemSocket::ready_sink();
        let recipient_stream = MemSocket::default();
        let recipient_sink = MemSocket::default();
        let pair = RelayPair {
            transfer_id: id,
            attempt_id: attempt,
            source,
            recipient,
            source_sink: Box::pin(source_sink) as RelaySink,
            source_stream: Box::pin(source_stream.clone()) as RelayStream,
            recipient_sink: Box::pin(recipient_sink.clone()) as RelaySink,
            recipient_stream: Box::pin(recipient_stream) as RelayStream,
            cancel: token,
            send_timeout: Duration::from_secs(30),
            close_grace: Duration::from_millis(20),
        };
        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            registry.run_relay_pair(&room, pair),
        )
        .await;
        assert!(outcome.is_err(), "blocked recipient parks the pump");
        assert_eq!(source_stream.polls(), 1, "second frame never read");
        assert_eq!(source_stream.inbound_len(), 1, "second frame still queued");
        assert!(recipient_sink.outbox().is_empty());
    }

    /// Every runtime refusal of the pump, in one table: an oversize frame,
    /// a text message on the payload leg, and any upstream message on the
    /// recipient leg all end the pair as a violation — plus the transport
    /// config that keeps compression off and the bounds small.
    #[tokio::test]
    async fn relay_rejects_oversize_text_binary_compression_and_recipient_binary() {
        for (label, source_inbound, recipient_inbound) in [
            (
                "oversize binary",
                vec![Message::Binary(relay_frame(0, 32 * 1024, 1).into())],
                vec![],
            ),
            (
                "text on the payload leg",
                vec![Message::Text("relay.attach".into())],
                vec![],
            ),
            (
                "binary from the recipient",
                vec![],
                vec![Message::Binary(relay_frame(0, 64, 1).into())],
            ),
            (
                "text from the recipient",
                vec![],
                vec![Message::Text("hello".into())],
            ),
        ] {
            let registry = transfer_registry();
            let (_lease, room) = transfer_room(&registry);
            let (source, _ga, recipient, _gb, _srx, _rrx, id, attempt, _st, _rt) =
                relay_live_pair(&registry, &room, "cccccccccccccccccccccccccccccccc").await;
            let token = registry.activate_relay(&room, id, attempt).unwrap();
            let recipient_sink = MemSocket::ready_sink();
            let pair = RelayPair {
                transfer_id: id,
                attempt_id: attempt,
                source,
                recipient,
                source_sink: Box::pin(MemSocket::ready_sink()) as RelaySink,
                source_stream: Box::pin(MemSocket::with_inbound(source_inbound)) as RelayStream,
                recipient_sink: Box::pin(recipient_sink.clone()) as RelaySink,
                recipient_stream: Box::pin(MemSocket::with_inbound(recipient_inbound))
                    as RelayStream,
                cancel: token,
                send_timeout: Duration::from_secs(30),
                close_grace: Duration::from_millis(20),
            };
            let (end, stats) =
                tokio::time::timeout(Duration::from_secs(5), registry.run_relay_pair(&room, pair))
                    .await
                    .unwrap_or_else(|_| panic!("{label}: the pump must end, not park"));
            assert_eq!(end, RelayEnd::Violation, "{label}");
            assert_eq!(stats.bytes, 0, "{label}: no payload crossed");
            assert_eq!(stats.frames, 0, "{label}");
        }
        // The transport itself never negotiates compression and never lets a
        // message past the frame bound.
        let config = crate::web_transfer_http::relay_websocket_config();
        assert_eq!(
            config.max_message_size,
            Some(WEB_TRANSFER_MAX_RELAY_MESSAGE_BYTES)
        );
        assert_eq!(
            config.max_frame_size,
            Some(WEB_TRANSFER_MAX_RELAY_FRAME_BYTES)
        );
    }

    #[tokio::test]
    async fn blocked_recipient_times_out_and_releases_permit() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _st, _rt) =
            relay_live_pair(&registry, &room, "cccccccccccccccccccccccccccccccc").await;
        let token = registry.activate_relay(&room, id, attempt).unwrap();
        let source_stream =
            MemSocket::with_inbound(vec![Message::Binary(relay_frame(0, 64, 1).into())]);
        let pair = RelayPair {
            transfer_id: id,
            attempt_id: attempt,
            source,
            recipient,
            source_sink: Box::pin(MemSocket::ready_sink()) as RelaySink,
            source_stream: Box::pin(source_stream) as RelayStream,
            recipient_sink: Box::pin(MemSocket::default()) as RelaySink,
            recipient_stream: Box::pin(MemSocket::default()) as RelayStream,
            cancel: token,
            send_timeout: Duration::from_millis(50),
            close_grace: Duration::from_millis(20),
        };
        let (end, stats) = registry.run_relay_pair(&room, pair).await;
        assert_eq!(end, RelayEnd::SendTimeout);
        assert_eq!((stats.bytes, stats.frames), (0, 0));
        // Failed exactly once: permit gone, second release is a no-op.
        assert_eq!(registry.current_relays(), 0);
        assert!(!registry.release_relay_permit(&room, id, attempt));
        assert!(registry.activate_relay(&room, id, attempt).is_none());
        // Both control legs hear path_commit then one retryable failure.
        for rx in [&mut source_rx, &mut recipient_rx] {
            let (typ, _) = recv_text(rx).await;
            assert_eq!(typ, "transfer.path_commit");
            let (typ, body) = recv_text(rx).await;
            assert_eq!(typ, "error");
            assert_eq!(body["code"].as_str(), Some("DIRECT_FAILED"));
            assert_eq!(body["message"].as_str(), Some(id.to_string()).as_deref());
        }
    }

    #[tokio::test]
    async fn relay_cancel_closes_both_sides() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _st, _rt) =
            relay_live_pair(&registry, &room, "cccccccccccccccccccccccccccccccc").await;
        // Control cancels first: terminal already, notices already drained.
        let (outcome, outbox) = registry.cancel_transfer(&room, source, id).unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        let token = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .cancel
            .clone();
        let source_sink = MemSocket::ready_sink();
        let recipient_sink = MemSocket::ready_sink();
        let pair = RelayPair {
            transfer_id: id,
            attempt_id: attempt,
            source,
            recipient,
            source_sink: Box::pin(source_sink.clone()) as RelaySink,
            source_stream: Box::pin(MemSocket::default()) as RelayStream,
            recipient_sink: Box::pin(recipient_sink.clone()) as RelaySink,
            recipient_stream: Box::pin(MemSocket::default()) as RelayStream,
            cancel: token,
            send_timeout: Duration::from_secs(10),
            close_grace: Duration::from_millis(20),
        };
        let (end, _) = registry.run_relay_pair(&room, pair).await;
        assert_eq!(end, RelayEnd::Cancelled);
        // Both legs got their Close; nothing further was notified.
        for socket in [&source_sink, &recipient_sink] {
            assert!(
                socket
                    .outbox()
                    .iter()
                    .any(|m| matches!(m, Message::Close(_))),
                "leg closed"
            );
        }
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.path_commit");
        let (typ, _) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.path_commit");
        assert!(source_rx.try_recv().is_err());
        assert!(recipient_rx.try_recv().is_err());
        assert_eq!(registry.current_relays(), 0);
    }

    #[tokio::test]
    async fn relay_logs_only_opaque_aggregates() {
        #[derive(Clone)]
        struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for LogSink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = LogSink(std::sync::Arc::clone(&buffer));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let _guard =
            tracing::dispatcher::set_default(&tracing::dispatcher::Dispatch::new(subscriber));
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, _srx, _rrx, id, attempt, _st, _rt) =
            relay_live_pair(&registry, &room, "cccccccccccccccccccccccccccccccc").await;
        let token = registry.activate_relay(&room, id, attempt).unwrap();
        // Ciphertext carries a canary the server must never repeat.
        let mut body = relay_frame(0, 64, 1);
        let canary = b"CANARY-RELAY-9f2c";
        body[16..16 + canary.len()].copy_from_slice(canary);
        let source_stream =
            MemSocket::with_inbound(vec![Message::Binary(body.into()), Message::Close(None)]);
        let recipient_sink = MemSocket::ready_sink();
        let pair = RelayPair {
            transfer_id: id,
            attempt_id: attempt,
            source,
            recipient,
            source_sink: Box::pin(MemSocket::ready_sink()) as RelaySink,
            source_stream: Box::pin(source_stream) as RelayStream,
            recipient_sink: Box::pin(recipient_sink.clone()) as RelaySink,
            recipient_stream: Box::pin(MemSocket::default()) as RelayStream,
            cancel: token,
            send_timeout: Duration::from_secs(10),
            close_grace: Duration::from_millis(20),
        };
        let (end, stats) = registry.run_relay_pair(&room, pair).await;
        assert_eq!(end, RelayEnd::Clean);
        assert_eq!((stats.bytes, stats.frames), (80, 1));
        assert_eq!(
            recipient_sink
                .outbox()
                .iter()
                .filter(|m| matches!(m, Message::Binary(_)))
                .count(),
            1
        );
        let logs = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();
        assert!(logs.contains("relay pair closed clean"));
        assert!(logs.contains(&id.to_string()));
        assert!(
            !logs.contains("CANARY-RELAY"),
            "payload leaked to logs:\n{logs}"
        );
    }

    // -----------------------------------------------------------------
    // Phase 4.1: direct-path negotiation, signaling and relay fallback.
    // Every test below asserts the SERVER's half: role order, bounds,
    // exactly-once commit, exactly-once fallback and the fact that a
    // direct attempt never touches the relay semaphore.
    // -----------------------------------------------------------------

    /// Opens a transfer and readies it, leaving it negotiating. Returns the
    /// participants, their queues, the transfer and the attempt, with both
    /// `transfer.direct_start` envelopes already delivered.
    async fn negotiating_fixture_for(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
        offer_hex: &str,
    ) -> (
        PeerId,
        PeerGuard,
        PeerId,
        PeerGuard,
        mpsc::Receiver<String>,
        mpsc::Receiver<String>,
        TransferId,
        AttemptId,
        [u8; 32],
    ) {
        let (source, guard_a, mut source_rx) = live_peer(registry, room, Some("A"));
        let (recipient, guard_b, recipient_rx) = live_peer(registry, room, Some("B"));
        let mac = offer_fixture(registry, room, source, offer_hex);
        let (id, attempt) = request_fixture(registry, room, recipient, offer_hex, &mac);
        let _ = recv_text(&mut source_rx).await;
        let digest = selection_digest(&offer_hex.parse().unwrap(), &mac, &["0".to_string()], "raw");
        let (outcome, outbox) = registry
            .source_ready(room, source, id, attempt, digest)
            .unwrap();
        assert_eq!(outcome, ReadyOutcome::Negotiating);
        drain_transfer_outbox(room, outbox);
        (
            source,
            guard_a,
            recipient,
            guard_b,
            source_rx,
            recipient_rx,
            id,
            attempt,
            digest,
        )
    }

    /// The common case: one offer, the room's default fixture offer ID.
    #[allow(clippy::type_complexity)]
    async fn negotiating_fixture(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
    ) -> (
        PeerId,
        PeerGuard,
        PeerId,
        PeerGuard,
        mpsc::Receiver<String>,
        mpsc::Receiver<String>,
        TransferId,
        AttemptId,
        [u8; 32],
    ) {
        negotiating_fixture_for(registry, room, "cccccccccccccccccccccccccccccccc").await
    }

    fn sdp_body(id: TransferId, attempt: AttemptId, sdp: &str) -> RtcSdpBody {
        RtcSdpBody {
            transfer_id: id,
            attempt_id: attempt,
            sdp: sdp.to_string(),
        }
    }

    fn ice_body(id: TransferId, attempt: AttemptId, candidate: Option<&str>) -> RtcIceBody {
        RtcIceBody {
            transfer_id: id,
            attempt_id: attempt,
            candidate: candidate.map(str::to_string),
            sdp_mid: candidate.map(|_| "0".to_string()),
            sdp_m_line_index: candidate.map(|_| 0u16),
        }
    }

    fn failed_body(
        id: TransferId,
        attempt: AttemptId,
        reason: &'static str,
        ranges: Vec<(u64, u64)>,
    ) -> DirectFailedBody {
        DirectFailedBody {
            transfer_id: id,
            attempt_id: attempt,
            reason,
            verified_ranges: ranges,
        }
    }

    #[tokio::test]
    async fn recipient_is_fixed_offerer_and_source_fixed_answerer() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (_s, _ga, _r, _gb, mut source_rx, mut recipient_rx, id, attempt, _digest) =
            negotiating_fixture(&registry, &room).await;
        // The recipient hears first: it creates the one DataChannel.
        let (typ, body) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.direct_start");
        assert_eq!(body["role"].as_str(), Some("offerer"));
        assert_eq!(body["transferId"].as_str(), Some(id.to_string()).as_deref());
        assert_eq!(
            body["attemptId"].as_str(),
            Some(attempt.to_string()).as_deref()
        );
        assert_eq!(body["attemptNumber"].as_u64(), Some(1));
        assert_eq!(body["deadlineMs"].as_u64(), Some(10_000));
        assert!(body["iceServers"].is_array());
        let (typ, body) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.direct_start");
        assert_eq!(body["role"].as_str(), Some("answerer"));
        assert_eq!(body["deadlineMs"].as_u64(), Some(10_000));
        // Every advertised ICE server is a STUN URL: a TURN credential the
        // server does not have must never appear here.
        for url in body["iceServers"].as_array().unwrap() {
            assert!(url.as_str().unwrap().starts_with("stun:"));
        }
        assert!(room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .state
            .is_negotiating_direct());
    }

    #[tokio::test]
    async fn sdp_order_roles_sizes_and_singletons_are_enforced() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let (stranger, _gc, _rx_c) = live_peer(&registry, &room, Some("C"));
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let offer = sdp_body(id, attempt, "v=0 offer");
        let answer = sdp_body(id, attempt, "v=0 answer");
        // The answer cannot precede the offer it answers.
        assert_eq!(
            registry
                .forward_rtc_answer(&room, source, &answer)
                .unwrap_err()
                .code(),
            "INVALID_MESSAGE"
        );
        // Roles are fixed: the source never offers, the recipient never
        // answers, and a stranger is neither.
        assert_eq!(
            registry
                .forward_rtc_offer(&room, source, &offer)
                .unwrap_err()
                .code(),
            "INVALID_MESSAGE"
        );
        assert_eq!(
            registry
                .forward_rtc_offer(&room, stranger, &offer)
                .unwrap_err()
                .code(),
            "NOT_PARTICIPANT"
        );
        let outbox = registry
            .forward_rtc_offer(&room, recipient, &offer)
            .unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].0, source);
        drain_transfer_outbox(&room, outbox);
        let (typ, body) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "rtc.offer");
        assert_eq!(body["sdp"].as_str(), Some("v=0 offer"));
        // Exactly once, each.
        assert!(registry
            .forward_rtc_offer(&room, recipient, &offer)
            .is_err());
        assert_eq!(
            registry
                .forward_rtc_answer(&room, recipient, &answer)
                .unwrap_err()
                .code(),
            "INVALID_MESSAGE"
        );
        let outbox = registry.forward_rtc_answer(&room, source, &answer).unwrap();
        assert_eq!(outbox[0].0, recipient);
        drain_transfer_outbox(&room, outbox);
        let (typ, body) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "rtc.answer");
        assert_eq!(body["sdp"].as_str(), Some("v=0 answer"));
        assert!(registry.forward_rtc_answer(&room, source, &answer).is_err());
        // A stale attempt is refused whatever the role.
        let stale = sdp_body(id, generate_attempt_id(), "v=0 offer");
        assert!(registry
            .forward_rtc_offer(&room, recipient, &stale)
            .is_err());
        // Size is enforced by the PARSER, which is the only place an SDP is
        // ever looked at, and the bound counts bytes.
        let too_big = "x".repeat(WEB_TRANSFER_MAX_SDP_BYTES + 1);
        let raw = serde_json::json!({
            "v": 1,
            "type": "rtc.offer",
            "requestId": "dddddddddddddddddddddddddddddddd",
            "body": {"transferId": id.to_string(), "attemptId": attempt.to_string(), "sdp": too_big},
        })
        .to_string();
        let env = crate::web_transfer_protocol::parse_client_envelope(&raw).unwrap();
        assert!(crate::web_transfer_protocol::parse_rtc_sdp_body(&env, "rtc.offer").is_err());
    }

    #[tokio::test]
    async fn ice_candidates_are_bounded_per_side_and_end_marker_forwards() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        // Both sides gather from the first message: ICE needs no ordering
        // against the offer, and holding candidates back would only delay it.
        let cap = WEB_TRANSFER_MAX_ICE_CANDIDATES_PER_SIDE;
        for i in 0..cap {
            let body = ice_body(id, attempt, Some(&format!("candidate:{i}")));
            let outbox = registry.forward_rtc_ice(&room, recipient, &body).unwrap();
            assert_eq!(outbox[0].0, source);
            assert_eq!(
                registry
                    .forward_rtc_ice(&room, source, &body)
                    .unwrap()
                    .len(),
                1
            );
            if i == 0 {
                drain_transfer_outbox(&room, outbox);
                let (typ, forwarded) = recv_text(&mut source_rx).await;
                assert_eq!(typ, "rtc.ice");
                assert_eq!(forwarded["candidate"].as_str(), Some("candidate:0"));
                assert_eq!(forwarded["sdpMid"].as_str(), Some("0"));
                assert_eq!(forwarded["sdpMLineIndex"].as_u64(), Some(0));
            }
        }
        // The budget is per side and the 129th is refused on both.
        let extra = ice_body(id, attempt, Some("candidate:over"));
        assert_eq!(
            registry
                .forward_rtc_ice(&room, recipient, &extra)
                .unwrap_err()
                .code(),
            "LIMIT_EXCEEDED"
        );
        assert_eq!(
            registry
                .forward_rtc_ice(&room, source, &extra)
                .unwrap_err()
                .code(),
            "LIMIT_EXCEEDED"
        );
        // A fresh negotiation gets its own budget, and the end-of-candidates
        // marker travels as a null candidate with nothing else on it.
        let (_s2, _ga2, r2, _gb2, mut src_rx2, mut rcp_rx2, id2, attempt2, _d2) =
            negotiating_fixture_for(&registry, &room, "dddddddddddddddddddddddddddddddd").await;
        let _ = recv_text(&mut rcp_rx2).await;
        let _ = recv_text(&mut src_rx2).await;
        let marker = ice_body(id2, attempt2, None);
        assert!(marker.is_end_of_candidates());
        let outbox = registry.forward_rtc_ice(&room, r2, &marker).unwrap();
        drain_transfer_outbox(&room, outbox);
        let (typ, forwarded) = recv_text(&mut src_rx2).await;
        assert_eq!(typ, "rtc.ice");
        assert!(forwarded["candidate"].is_null());
        assert!(forwarded.get("sdpMid").is_none());
        assert!(forwarded.get("sdpMLineIndex").is_none());
    }

    #[tokio::test]
    async fn signaling_is_forward_only_and_never_logged() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (_source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let secret_sdp = "v=0 SECRET-SDP-MARKER";
        let secret_candidate = "candidate:SECRET-CANDIDATE-MARKER";
        let outbox = registry
            .forward_rtc_offer(&room, recipient, &sdp_body(id, attempt, secret_sdp))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let (_, forwarded) = recv_text(&mut source_rx).await;
        assert_eq!(forwarded["sdp"].as_str(), Some(secret_sdp));
        let outbox = registry
            .forward_rtc_ice(
                &room,
                recipient,
                &ice_body(id, attempt, Some(secret_candidate)),
            )
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        // The server keeps neither: the whole record's Debug — which is what
        // any log line could ever print — contains no part of them.
        let dump = format!("{:?}", room.state.lock().unwrap().transfers.get(&id));
        assert!(!dump.contains("SECRET-SDP-MARKER"), "{dump}");
        assert!(!dump.contains("SECRET-CANDIDATE-MARKER"), "{dump}");
        // A refusal names the step, never the value.
        let err = registry
            .forward_rtc_offer(&room, recipient, &sdp_body(id, attempt, secret_sdp))
            .unwrap_err();
        let text = format!("{err:?} {}", err.code());
        assert!(!text.contains("SECRET-SDP-MARKER"), "{text}");
        // The forwarded envelope carries no requestId: the peer's own
        // correlation ID belongs to its own ack and to nothing else.
        let envelope = crate::web_transfer_protocol::rtc_sdp_envelope(
            "rtc.offer",
            &sdp_body(id, attempt, secret_sdp),
        );
        let value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        assert!(value.get("requestId").is_none());
    }

    #[tokio::test]
    async fn both_ready_commit_direct_recipient_then_source() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        // Ready before that side's own signaling step is refused.
        assert!(registry
            .direct_ready(&room, recipient, id, attempt)
            .is_err());
        let outbox = registry
            .forward_rtc_offer(&room, recipient, &sdp_body(id, attempt, "v=0 offer"))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        assert!(registry.direct_ready(&room, source, id, attempt).is_err());
        let outbox = registry
            .forward_rtc_answer(&room, source, &sdp_body(id, attempt, "v=0 answer"))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        // First ready commits nothing and is idempotent.
        assert!(registry
            .direct_ready(&room, recipient, id, attempt)
            .unwrap()
            .is_empty());
        assert!(registry
            .direct_ready(&room, recipient, id, attempt)
            .unwrap()
            .is_empty());
        // The second distinct ready commits, recipient first.
        let outbox = registry.direct_ready(&room, source, id, attempt).unwrap();
        assert_eq!(outbox.len(), 2);
        assert_eq!(outbox[0].0, recipient);
        assert_eq!(outbox[1].0, source);
        drain_transfer_outbox(&room, outbox);
        for rx in [&mut recipient_rx, &mut source_rx] {
            let (typ, body) = recv_text(rx).await;
            assert_eq!(typ, "transfer.path_commit");
            assert_eq!(body["path"].as_str(), Some("direct"));
            assert_eq!(
                body["attemptId"].as_str(),
                Some(attempt.to_string()).as_deref()
            );
        }
        assert_eq!(
            room.state.lock().unwrap().transfers.get(&id).unwrap().state,
            TransferState::ActiveDirect
        );
        // A repeat after the commit does not commit twice.
        assert!(registry.direct_ready(&room, source, id, attempt).is_err());
        // Direct carried it, so completion is valid from ActiveDirect.
        assert_eq!(registry.current_relays(), 0);
    }

    #[tokio::test]
    async fn direct_timeout_falls_back_once_with_fresh_attempt() {
        let registry = transfer_registry();
        registry.set_direct_deadline(Duration::from_millis(40));
        let (_lease, room) = transfer_room(&registry);
        let (_s, _ga, _r, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let (_, start) = recv_text(&mut recipient_rx).await;
        assert_eq!(start["deadlineMs"].as_u64(), Some(40));
        let _ = recv_text(&mut source_rx).await;
        assert_eq!(registry.current_relays(), 0);
        // The real timer: armed the way the actor arms it, fired by time.
        registry.spawn_direct_deadline(&room, id, attempt);
        let (typ, body) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.direct_failed");
        assert_eq!(body["reason"].as_str(), Some("timeout"));
        let (typ, _) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.relay_ticket");
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.direct_failed");
        let (typ, ticket) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.relay_ticket");
        let next: AttemptId = ticket["attemptId"].as_str().unwrap().parse().unwrap();
        assert_ne!(next, attempt);
        let (state_now, number, current) = {
            let guard = room.state.lock().unwrap();
            let record = guard.transfers.get(&id).unwrap();
            (record.state, record.attempt_number, record.attempt_id)
        };
        assert_eq!(state_now, TransferState::WaitingRelay);
        assert_eq!(number, 2);
        assert_eq!(current, Some(next));
        assert_eq!(registry.current_relays(), 1);
        // Firing again changes nothing: one fallback, one permit.
        registry.direct_deadline_elapsed(&room, id, attempt).await;
        assert_eq!(registry.current_relays(), 1);
        assert_eq!(
            room.state
                .lock()
                .unwrap()
                .transfers
                .get(&id)
                .unwrap()
                .attempt_id,
            Some(next)
        );
        assert!(recipient_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn direct_failure_mid_active_preserves_transfer_and_ranges() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let outbox = registry
            .forward_rtc_offer(&room, recipient, &sdp_body(id, attempt, "v=0 offer"))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        let outbox = registry
            .forward_rtc_answer(&room, source, &sdp_body(id, attempt, "v=0 answer"))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        let _ = registry
            .direct_ready(&room, recipient, id, attempt)
            .unwrap();
        let outbox = registry.direct_ready(&room, source, id, attempt).unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        // The channel dies after two verified chunks: the RECIPIENT reports
        // what it holds, and the same transfer continues on the relay.
        let (outcome, outbox) = registry
            .direct_failed(
                &room,
                recipient,
                &failed_body(id, attempt, "channel-closed", vec![(0, 2)]),
            )
            .unwrap();
        assert_eq!(outcome, ReadyOutcome::Admitted);
        drain_transfer_outbox(&room, outbox);
        let (typ, notice) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.direct_failed");
        assert_eq!(notice["reason"].as_str(), Some("channel-closed"));
        assert_eq!(notice["resumeRanges"][0][0].as_u64(), Some(0));
        assert_eq!(notice["resumeRanges"][0][1].as_u64(), Some(2));
        let (typ, source_ticket) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.relay_ticket");
        let (typ, recipient_ticket) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.relay_ticket");
        assert_ne!(
            source_ticket["ticket"].as_str(),
            recipient_ticket["ticket"].as_str()
        );
        let (kept_id, ranges, state_now) = {
            let guard = room.state.lock().unwrap();
            let record = guard.transfers.get(&id).unwrap();
            (
                record.transfer_id,
                record
                    .resume
                    .as_ref()
                    .map(|r| r.verified_ranges.clone())
                    .unwrap_or_default(),
                record.state,
            )
        };
        assert_eq!(kept_id, id);
        assert_eq!(ranges, vec![(0, 2)]);
        assert_eq!(state_now, TransferState::WaitingRelay);
        // Only the recipient knows what it verified: a source-reported range
        // list is not believed, and a second failure does nothing at all.
        let (outcome, outbox) = registry
            .direct_failed(
                &room,
                source,
                &failed_body(id, attempt, "channel-closed", vec![(0, 9999)]),
            )
            .unwrap();
        assert_eq!(outcome, ReadyOutcome::Ignored);
        assert!(outbox.is_empty());
        assert_eq!(
            room.state
                .lock()
                .unwrap()
                .transfers
                .get(&id)
                .unwrap()
                .resume
                .as_ref()
                .unwrap()
                .verified_ranges,
            vec![(0, 2)]
        );
    }

    #[tokio::test]
    async fn relay_busy_after_direct_failure_is_retryable() {
        let registry = busy_registry();
        registry.set_relay_admit_timeout(Duration::from_millis(20));
        let (_lease, room) = transfer_room(&registry);
        let held = registry.try_acquire_relay().unwrap();
        let (_s, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let (outcome, outbox) = registry
            .direct_failed(
                &room,
                recipient,
                &failed_body(id, attempt, "ice-failed", vec![]),
            )
            .unwrap();
        assert_eq!(outcome, ReadyOutcome::Queued);
        // Only the counterpart notice: no ticket exists to send.
        assert_eq!(outbox.len(), 1);
        drain_transfer_outbox(&room, outbox);
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.direct_failed");
        let next = registry.current_attempt(&room, id).unwrap();
        assert_ne!(next, attempt);
        registry.admit_relay(&room, id, next).await;
        let (typ, body) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "error");
        assert_eq!(body["code"].as_str(), Some("RELAY_BUSY"));
        let (state_now, has_permit) = {
            let guard = room.state.lock().unwrap();
            let record = guard.transfers.get(&id).unwrap();
            (
                record.state,
                record.attempt.as_ref().unwrap().relay_permit.is_some(),
            )
        };
        // Retryable, not terminal, and still holding no permit.
        assert_eq!(state_now, TransferState::WaitingRelay);
        assert!(!has_permit);
        assert!(state_now.is_live());
        drop(held);
    }

    #[tokio::test]
    async fn stale_direct_timer_and_messages_cannot_touch_new_attempt() {
        // The relay is full, so the one fallback QUEUES without a permit —
        // the only shape `transfer.request` upgrades, which is how this test
        // gets a THIRD attempt to prove the first two cannot reach.
        let registry = busy_registry();
        registry.set_relay_admit_timeout(Duration::from_millis(20));
        let (_lease, room) = transfer_room(&registry);
        let held = registry.try_acquire_relay().unwrap();
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt1, digest) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let (outcome, outbox) = registry
            .direct_failed(
                &room,
                recipient,
                &failed_body(id, attempt1, "ice-failed", vec![]),
            )
            .unwrap();
        assert_eq!(outcome, ReadyOutcome::Queued);
        drain_transfer_outbox(&room, outbox);
        let relay_attempt = registry.current_attempt(&room, id).unwrap();
        assert_ne!(relay_attempt, attempt1);
        while source_rx.try_recv().is_ok() {}
        while recipient_rx.try_recv().is_ok() {}
        // A fresh click on a permit-less WaitingRelay upgrades the attempt.
        let (_, _) = registry
            .request_transfer(
                &room,
                recipient,
                "cccccccccccccccccccccccccccccccc".parse().unwrap(),
                vec!["0".to_string()],
                digest,
                "raw",
                None,
            )
            .unwrap();
        let attempt3 = registry.current_attempt(&room, id).unwrap();
        assert_ne!(attempt3, attempt1);
        assert_ne!(attempt3, relay_attempt);
        let _ = recv_text(&mut source_rx).await;
        let (outcome, outbox) = registry
            .source_ready(&room, source, id, attempt3, digest)
            .unwrap();
        assert_eq!(outcome, ReadyOutcome::Negotiating);
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        // Every stale message and the stale timer are inert.
        for stale in [attempt1, relay_attempt] {
            assert!(registry
                .forward_rtc_offer(&room, recipient, &sdp_body(id, stale, "v=0 offer"))
                .is_err());
            assert!(registry
                .forward_rtc_ice(&room, recipient, &ice_body(id, stale, Some("candidate:x")))
                .is_err());
            assert!(registry.direct_ready(&room, recipient, id, stale).is_err());
            let (outcome, outbox) = registry
                .direct_failed(
                    &room,
                    recipient,
                    &failed_body(id, stale, "ice-failed", vec![]),
                )
                .unwrap();
            assert_eq!(outcome, ReadyOutcome::Ignored);
            assert!(outbox.is_empty());
            registry.direct_deadline_elapsed(&room, id, stale).await;
        }
        let (state_now, current) = {
            let guard = room.state.lock().unwrap();
            let record = guard.transfers.get(&id).unwrap();
            (record.state, record.attempt_id)
        };
        assert!(state_now.is_negotiating_direct());
        assert_eq!(current, Some(attempt3));
        // The only permit in flight is the one this test holds by hand:
        // nothing the stale traffic did took a second.
        assert_eq!(registry.current_relays(), 1);
        assert!(recipient_rx.try_recv().is_err());
        drop(held);
    }

    #[tokio::test]
    async fn cancel_disconnect_withdraw_win_over_fallback() {
        // Cancel beats the timer.
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (_s, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let (outcome, outbox) = registry.cancel_transfer(&room, recipient, id).unwrap();
        assert_eq!(outcome, CancelOutcome::Cancelled);
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        registry.direct_deadline_elapsed(&room, id, attempt).await;
        let (_, outbox) = registry
            .direct_failed(
                &room,
                recipient,
                &failed_body(id, attempt, "ice-failed", vec![]),
            )
            .unwrap();
        assert!(outbox.is_empty());
        assert_eq!(
            room.state.lock().unwrap().transfers.get(&id).unwrap().state,
            TransferState::Cancelled
        );
        assert_eq!(registry.current_relays(), 0);

        // Withdrawing the offer beats it too.
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, _r, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        registry
            .withdraw_offer(
                &room,
                source,
                "cccccccccccccccccccccccccccccccc".parse().unwrap(),
            )
            .unwrap();
        registry.direct_deadline_elapsed(&room, id, attempt).await;
        assert!(!room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .state
            .is_live());
        assert_eq!(registry.current_relays(), 0);

        // So does the source's control session going away.
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (_s, guard_a, _r, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        drop(guard_a);
        registry.direct_deadline_elapsed(&room, id, attempt).await;
        let terminal = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .map(|record| !record.state.is_live())
            .unwrap_or(true);
        assert!(terminal);
        assert_eq!(registry.current_relays(), 0);
    }

    fn progress_body(
        id: TransferId,
        attempt: AttemptId,
        received: u64,
    ) -> crate::web_transfer_protocol::ProgressBody {
        crate::web_transfer_protocol::ProgressBody {
            transfer_id: id,
            attempt_id: attempt,
            received_bytes: received,
        }
    }

    /// Drives one transfer to `ActiveDirect`: both peers ready, both
    /// `path_commit direct` drained.
    #[allow(clippy::type_complexity)]
    async fn active_direct_fixture(
        registry: &WebTransferRegistry,
        room: &Arc<WebTransferRoom>,
    ) -> (
        PeerId,
        PeerGuard,
        PeerId,
        PeerGuard,
        mpsc::Receiver<String>,
        mpsc::Receiver<String>,
        TransferId,
        AttemptId,
    ) {
        let (source, guard_a, recipient, guard_b, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(registry, room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let outbox = registry
            .forward_rtc_offer(room, recipient, &sdp_body(id, attempt, "v=0 offer"))
            .unwrap();
        drain_transfer_outbox(room, outbox);
        let _ = recv_text(&mut source_rx).await;
        let outbox = registry
            .forward_rtc_answer(room, source, &sdp_body(id, attempt, "v=0 answer"))
            .unwrap();
        drain_transfer_outbox(room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        let _ = registry.direct_ready(room, recipient, id, attempt).unwrap();
        let outbox = registry.direct_ready(room, source, id, attempt).unwrap();
        drain_transfer_outbox(room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        (
            source,
            guard_a,
            recipient,
            guard_b,
            source_rx,
            recipient_rx,
            id,
            attempt,
        )
    }

    #[tokio::test]
    async fn progress_is_recipient_only_and_bounded_by_the_entry() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, _source_rx, _recipient_rx, id, attempt) =
            active_direct_fixture(&registry, &room).await;
        let entry_size = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .entry_size
            .expect("a raw transfer carries the manifest size");
        // The source verified nothing — it wrote the bytes. Letting it
        // report would put an unchecked count on the recipient's progress
        // bar and, worse, make the path a thing a peer can assert.
        assert_eq!(
            registry
                .report_progress(&room, source, &progress_body(id, attempt, 1))
                .unwrap_err()
                .code(),
            "INVALID_MESSAGE"
        );
        let (stranger, _gc, _rx) = live_peer(&registry, &room, Some("C"));
        assert_eq!(
            registry
                .report_progress(&room, stranger, &progress_body(id, attempt, 1))
                .unwrap_err()
                .code(),
            "NOT_PARTICIPANT"
        );
        assert_eq!(
            registry
                .report_progress(
                    &room,
                    recipient,
                    &progress_body(TransferId::from_bytes([7u8; 16]), attempt, 1)
                )
                .unwrap_err()
                .code(),
            "TRANSFER_NOT_FOUND"
        );
        // More than the entry holds cannot be true of any attempt.
        assert_eq!(
            registry
                .report_progress(
                    &room,
                    recipient,
                    &progress_body(id, attempt, entry_size + 1)
                )
                .unwrap_err()
                .code(),
            "INVALID_MESSAGE"
        );
        // Zero is the state before the first verified chunk: acked, and
        // nothing is forwarded, so no path is declared on the strength of it.
        assert!(registry
            .report_progress(&room, recipient, &progress_body(id, attempt, 0))
            .unwrap()
            .is_empty());
        // A stale attempt describes a world that has already ended.
        assert!(registry
            .report_progress(
                &room,
                recipient,
                &progress_body(id, AttemptId::from_bytes([9u8; 16]), 1)
            )
            .unwrap()
            .is_empty());
        // The whole entry is a legal report.
        assert_eq!(
            registry
                .report_progress(&room, recipient, &progress_body(id, attempt, entry_size))
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn forwarded_progress_path_is_the_servers_and_goes_only_to_the_source() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt) =
            active_direct_fixture(&registry, &room).await;
        let entry_size = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .entry_size
            .expect("a raw transfer carries the manifest size");
        let half = entry_size / 2;
        let outbox = registry
            .report_progress(&room, recipient, &progress_body(id, attempt, half))
            .unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].0, source, "the report goes to the source alone");
        drain_transfer_outbox(&room, outbox);
        let (typ, body) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.progress");
        assert_eq!(body["transferId"].as_str(), Some(id.to_string().as_str()));
        assert_eq!(
            body["attemptId"].as_str(),
            Some(attempt.to_string().as_str())
        );
        // A 64-bit quantity travels as a decimal string, like every other
        // one on this wire.
        assert_eq!(
            body["receivedBytes"].as_str(),
            Some(half.to_string().as_str())
        );
        assert_eq!(body["path"].as_str(), Some("direct"));
        // The path is DERIVED, never taken from the reporter: the same peer
        // sending the same body on the relay is told `relay`.
        {
            let mut guard = room.state.lock().unwrap();
            guard.transfers.get_mut(&id).unwrap().state = TransferState::Active;
        }
        let outbox = registry
            .report_progress(&room, recipient, &progress_body(id, attempt, entry_size))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let (typ, body) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.progress");
        assert_eq!(body["path"].as_str(), Some("relay"));
        assert_eq!(
            body["receivedBytes"].as_str(),
            Some(entry_size.to_string().as_str())
        );
        // Terminal: acked and dropped, so a late report cannot resurrect a
        // completed transfer's progress bar.
        {
            let mut guard = room.state.lock().unwrap();
            guard.transfers.get_mut(&id).unwrap().state = TransferState::Completed;
        }
        assert!(registry
            .report_progress(&room, recipient, &progress_body(id, attempt, entry_size))
            .unwrap()
            .is_empty());
        assert!(
            recipient_rx.try_recv().is_err(),
            "the recipient hears nothing about its own report"
        );
    }

    #[tokio::test]
    async fn direct_never_acquires_relay_permit() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, mut recipient_rx, id, attempt, _d) =
            negotiating_fixture(&registry, &room).await;
        let _ = recv_text(&mut recipient_rx).await;
        let _ = recv_text(&mut source_rx).await;
        let no_permit = |state: &Arc<WebTransferRoom>| {
            let guard = state.state.lock().unwrap();
            let record = guard.transfers.get(&id).unwrap();
            record
                .attempt
                .as_ref()
                .map(|attempt| attempt.relay_permit.is_none())
                .unwrap_or(true)
        };
        assert_eq!(registry.current_relays(), 0);
        assert!(no_permit(&room));
        let outbox = registry
            .forward_rtc_offer(&room, recipient, &sdp_body(id, attempt, "v=0 offer"))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut source_rx).await;
        let outbox = registry
            .forward_rtc_answer(&room, source, &sdp_body(id, attempt, "v=0 answer"))
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let _ = recv_text(&mut recipient_rx).await;
        assert_eq!(registry.current_relays(), 0);
        let _ = registry
            .direct_ready(&room, recipient, id, attempt)
            .unwrap();
        let outbox = registry.direct_ready(&room, source, id, attempt).unwrap();
        drain_transfer_outbox(&room, outbox);
        let (typ, _) = recv_text(&mut recipient_rx).await;
        assert_eq!(typ, "transfer.path_commit");
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.path_commit");
        // Committed direct and still holding nothing: the whole point.
        assert_eq!(
            room.state.lock().unwrap().transfers.get(&id).unwrap().state,
            TransferState::ActiveDirect
        );
        assert_eq!(registry.current_relays(), 0);
        assert!(no_permit(&room));
        // Completing on the direct path releases nothing because nothing was
        // taken, and the relay budget is untouched at the end.
        let root = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .entry_root
            .expect("a raw transfer carries the manifest root");
        let outbox = registry
            .complete_transfer(&room, recipient, id, attempt, root)
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.completed");
        assert_eq!(registry.current_relays(), 0);
    }
    /// 4.4 — the attempt counters follow the RECIPIENT's report and nothing
    /// else. Not the commit: F-12 measured that shape on the native side,
    /// where `direct_stream_opens` counted attempts and climbed 1 -> 12
    /// during a blackout that moved zero bytes, so an operator read a
    /// healthy direct path on a tunnel that was entirely on the relay. Not
    /// the source either — it wrote the bytes, it verified none of them.
    #[tokio::test]
    async fn direct_and_relay_attempt_counters_follow_recipient_report() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, _srx, _rrx, id, attempt) =
            active_direct_fixture(&registry, &room).await;
        // The direct path is COMMITTED here and nothing has been verified on
        // it, so neither counter may have moved.
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (0, 0)
        );
        // A report with no bytes on it describes no carried byte.
        assert!(registry
            .report_progress(&room, recipient, &progress_body(id, attempt, 0))
            .unwrap()
            .is_empty());
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (0, 0)
        );
        // The SOURCE cannot move it: a refused report counts nothing.
        assert_eq!(
            registry
                .report_progress(&room, source, &progress_body(id, attempt, 4))
                .unwrap_err()
                .code(),
            "INVALID_MESSAGE"
        );
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (0, 0)
        );
        // First verified byte: the direct attempt counts, exactly once, and
        // a second report on the SAME attempt adds nothing.
        let outbox = registry
            .report_progress(&room, recipient, &progress_body(id, attempt, 4))
            .unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (1, 0)
        );
        let _ = registry
            .report_progress(&room, recipient, &progress_body(id, attempt, 9))
            .unwrap();
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (1, 0)
        );

        // The relay half, on its own transfer between two other peers: the
        // same first-verified-byte rule, counted on the other side.
        let (_src2, _gc, rcpt2, _gd, _srx2, _rrx2, id2, attempt2, _st, _rt) =
            relay_live_pair(&registry, &room, "dddddddddddddddddddddddddddddddd").await;
        // Admitted with tickets and not yet carrying: still nothing.
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (1, 0)
        );
        let _token = registry.activate_relay(&room, id2, attempt2).unwrap();
        let _ = registry
            .report_progress(&room, rcpt2, &progress_body(id2, attempt2, 3))
            .unwrap();
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (1, 1)
        );
    }

    /// 4.4 — a recipient is authority over ITS OWN transfer and no other.
    /// The path a peer sees is the server's, derived from the record; a
    /// report naming someone else's transfer is refused and leaves that
    /// transfer's state, its counter and its path exactly as they were.
    #[tokio::test]
    async fn recipient_report_cannot_change_unrelated_transfer_path() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        // T1: A -> B on the direct path.
        let (_source, _ga, recipient, _gb, _srx, _rrx, id, attempt) =
            active_direct_fixture(&registry, &room).await;
        // T2: C -> D on the relay, live and carrying.
        let (_src2, _gc, rcpt2, _gd, _srx2, _rrx2, id2, attempt2, _st, _rt) =
            relay_live_pair(&registry, &room, "dddddddddddddddddddddddddddddddd").await;
        let _token = registry.activate_relay(&room, id2, attempt2).unwrap();

        // T1's recipient reporting on T2 is a stranger there, whichever
        // attempt id it names — its own or T2's.
        for named in [attempt, attempt2] {
            assert_eq!(
                registry
                    .report_progress(&room, recipient, &progress_body(id2, named, 5))
                    .unwrap_err()
                    .code(),
                "NOT_PARTICIPANT"
            );
        }
        // And T2's recipient cannot touch T1 either.
        assert_eq!(
            registry
                .report_progress(&room, rcpt2, &progress_body(id, attempt, 5))
                .unwrap_err()
                .code(),
            "NOT_PARTICIPANT"
        );
        // Nothing moved: neither counter, neither state, neither record's
        // carried attempt.
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (0, 0)
        );
        {
            let state = room.state.lock().unwrap();
            let t1 = state.transfers.get(&id).unwrap();
            let t2 = state.transfers.get(&id2).unwrap();
            assert_eq!(t1.state, TransferState::ActiveDirect);
            assert_eq!(t2.state, TransferState::Active);
            assert_eq!(t1.carried_attempt, None);
            assert_eq!(t2.carried_attempt, None);
        }
        // Each recipient reporting on its OWN transfer still works, and each
        // gets the path the SERVER committed for it — never the other's.
        let outbox = registry
            .report_progress(&room, recipient, &progress_body(id, attempt, 5))
            .unwrap();
        assert_eq!(path_of_progress(&outbox), "direct");
        let outbox = registry
            .report_progress(&room, rcpt2, &progress_body(id2, attempt2, 5))
            .unwrap();
        assert_eq!(path_of_progress(&outbox), "relay");
        assert_eq!(
            (registry.direct_carried(), registry.relay_carried()),
            (1, 1)
        );
    }

    /// 4.4 — receiving a file publishes nothing. The catalogue after a
    /// completed transfer is the catalogue before it: one offer, owned by
    /// the source. A recipient that seeded what it just downloaded would
    /// turn one click into a second source nobody chose to be.
    #[tokio::test]
    async fn downloaded_file_does_not_create_offer_or_seed() {
        let registry = transfer_registry();
        let (_lease, room) = transfer_room(&registry);
        let (source, _ga, recipient, _gb, mut source_rx, _rrx, id, attempt) =
            active_direct_fixture(&registry, &room).await;
        let before: Vec<(String, PeerId)> = {
            let state = room.state.lock().unwrap();
            let mut rows: Vec<_> = state
                .offers
                .iter()
                .map(|(offer, record)| (offer.to_string(), record.owner))
                .collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            rows
        };
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].1, source);

        let _ = registry
            .report_progress(&room, recipient, &progress_body(id, attempt, 17))
            .unwrap();
        let root = room
            .state
            .lock()
            .unwrap()
            .transfers
            .get(&id)
            .unwrap()
            .entry_root
            .expect("a raw transfer carries the manifest root");
        let outbox = registry
            .complete_transfer(&room, recipient, id, attempt, root)
            .unwrap();
        drain_transfer_outbox(&room, outbox);
        let (typ, _) = recv_text(&mut source_rx).await;
        assert_eq!(typ, "transfer.completed");

        // Same catalogue, same owner, and nothing owned by the recipient.
        let after: Vec<(String, PeerId)> = {
            let state = room.state.lock().unwrap();
            let mut rows: Vec<_> = state
                .offers
                .iter()
                .map(|(offer, record)| (offer.to_string(), record.owner))
                .collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            rows
        };
        assert_eq!(after, before);
        assert!(
            after.iter().all(|(_, owner)| *owner != recipient),
            "the recipient became a source by downloading"
        );
        // The room's own snapshot agrees — the peers see what the map holds.
        let (_revision, peers) = snapshot_parts(&room).unwrap();
        assert_eq!(peers.len(), 2);
    }

    /// The `path` a `transfer.progress` notice carries, for the assertions
    /// above: it is the SERVER's word, so the test reads it off the wire.
    fn path_of_progress(outbox: &TransferOutbox) -> String {
        assert_eq!(
            outbox.len(),
            1,
            "progress goes to the source and nowhere else"
        );
        let value: serde_json::Value = serde_json::from_str(&outbox[0].1).unwrap();
        assert_eq!(value["type"], "transfer.progress");
        value["body"]["path"].as_str().unwrap().to_string()
    }
}
