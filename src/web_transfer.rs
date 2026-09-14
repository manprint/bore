//! Web-transfer domain model: protocol constants, IDs, limits and validators.
//!
//! Phase 0 foundation only. No route, server flag, CLI command or runtime
//! registry lives here; those arrive in later phases. Browser/native wire
//! codecs live in [`crate::web_transfer_protocol`].

use std::{
    collections::HashMap,
    fmt,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

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
            for entry in custom {
                let target = entry.strip_prefix("stun:").ok_or_else(|| {
                    anyhow::anyhow!("STUN server must look like stun:HOST[:PORT], got {entry}")
                })?;
                if target.is_empty() {
                    bail!("STUN server must look like stun:HOST[:PORT], got {entry}");
                }
            }
            return Ok(Self {
                servers: custom.to_vec(),
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
        assert!(
            IceServerConfig::resolve(false, &["example.com".to_string()], true, "h", 1).is_err()
        );
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
    fn embedded_web_transfer_assets_have_shell_js_and_css() {
        let mut urls: Vec<&str> = WEB_TRANSFER_ASSETS.iter().map(|(url, _, _)| *url).collect();
        urls.sort_unstable();
        assert_eq!(
            urls,
            [
                "/transfer/assets/app.css",
                "/transfer/assets/app.js",
                "/transfer/assets/index.html",
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

/// Offer metadata placeholder (accounting only; Phase 2 adds the manifest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferRecord {
    /// Control/catalog bytes charged against the room metadata budget.
    pub metadata_bytes: u64,
}

/// Transfer metadata placeholder (accounting only; Phase 3 adds state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecord {
    /// Control bytes charged against the room metadata budget.
    pub metadata_bytes: u64,
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
}

/// Broadcast room lifecycle events (capacity 256).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    /// Room destroyed; `reason` is an opaque code (`owner-close`, `expired`).
    RoomClosed {
        /// Opaque reason code, safe for logs and control messages.
        reason: &'static str,
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
        if let Ok(mut state) = self.room.state.lock() {
            state.peers.remove(&self.peer_id);
        }
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
    peers_current: AtomicU64,
    metadata_current: AtomicU64,
    transfers_current: AtomicU64,
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
                peers_current: AtomicU64::new(0),
                metadata_current: AtomicU64::new(0),
                transfers_current: AtomicU64::new(0),
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
    #[allow(dead_code)]
    pub(crate) fn current_rooms(&self) -> u64 {
        self.inner
            .config
            .limits
            .max_rooms
            .saturating_sub(self.inner.room_permits.available_permits() as u64)
    }

    /// Live peer count across all rooms.
    #[allow(dead_code)]
    pub(crate) fn current_peers(&self) -> u64 {
        self.inner.peers_current.load(Ordering::Relaxed)
    }

    /// Live metadata bytes across all rooms.
    #[allow(dead_code)]
    pub(crate) fn current_metadata_bytes(&self) -> u64 {
        self.inner.metadata_current.load(Ordering::Relaxed)
    }

    /// Live transfer count across all rooms. First consumers arrive in Phase 3.
    #[allow(dead_code)]
    pub(crate) fn current_transfers(&self) -> u64 {
        self.inner.transfers_current.load(Ordering::Relaxed)
    }

    /// Live relay-pair count (derived from the semaphore).
    #[allow(dead_code)]
    pub(crate) fn current_relays(&self) -> u64 {
        self.inner
            .config
            .limits
            .max_relays_global
            .saturating_sub(self.inner.relay_permits.available_permits() as u64)
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
                .map_err(|_| WebTransferError::limit("web-transfer room budget exhausted"))?;
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
                }),
                events: broadcast::channel(256).0,
                cancel: CancellationToken::new(),
                epoch: AtomicU64::new(0),
                destroyed: AtomicBool::new(false),
                registry: Arc::downgrade(&self.inner),
                room_permit: permit,
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

    /// Admits a peer: global budget first, then the room lock, then a room
    /// budget recheck. Any rejection drops the global permit (rollback).
    /// Duplicate joins are rejected so one global slot never backs two guards.
    pub fn join_peer(
        &self,
        room: &Arc<WebTransferRoom>,
        peer_id: PeerId,
        display_name: Option<String>,
    ) -> Result<PeerGuard, WebTransferError> {
        let permit = Arc::clone(&self.inner.peer_permits)
            .try_acquire_owned()
            .map_err(|_| WebTransferError::limit("web-transfer peer budget exhausted"))?;
        {
            let mut state = room
                .state
                .lock()
                .map_err(|_| WebTransferError::internal("room state lock poisoned"))?;
            if state.peers.len() >= room.limits.max_peers_per_room as usize {
                return Err(WebTransferError::limit("room peer budget exhausted"));
            }
            if state.peers.contains_key(&peer_id) {
                return Err(WebTransferError::invalid("peer already joined"));
            }
            state.peers.insert(peer_id, PeerRecord { display_name });
        }
        self.inner.peers_current.fetch_add(1, Ordering::Relaxed);
        Ok(PeerGuard {
            room: Arc::clone(room),
            peer_id,
            spent: false,
            _permit: permit,
        })
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
                return Err(WebTransferError::limit(
                    "web-transfer metadata budget exhausted",
                ));
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
            .map_err(|_| WebTransferError::limit("web-transfer relay budget exhausted"))
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
            // Optional `:port` (a bare IPv6 literal keeps its colons: only a
            // single `:digits` suffix on a non-bracketed host counts).
            if !target.starts_with('[') && target.matches(':').count() == 1 {
                if let Some((_, port)) = target.rsplit_once(':') {
                    match port.parse::<u16>() {
                        Ok(p) if p != 0 => {}
                        _ => bail!("STUN server port must be 1..=65535 in {part:?}"),
                    }
                }
            } else if target.ends_with(':') {
                bail!("STUN server port must not be empty in {part:?}");
            }
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
    let stun = parse_stun_list(&args.stun)?;
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
    let ice = IceServerConfig::resolve(args.no_stun, &stun, server_udp, host_part, control_port)?;
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
            .map_err(|_| WebTransferError::limit("web-transfer room budget exhausted"))?;
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
            }),
            events: broadcast::channel(256).0,
            cancel: CancellationToken::new(),
            epoch: AtomicU64::new(0),
            destroyed: AtomicBool::new(false),
            registry: Arc::downgrade(&self.inner),
            room_permit: permit,
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
