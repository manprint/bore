//! Deployment-example gates (6.5): the compose files are part of the product.
//!
//! An operator copies one of these files, changes two values and runs it. So a
//! typo in an environment variable name is not a documentation bug — it is a
//! feature that silently stays off, which is exactly how the web-transfer
//! surface fails safe and exactly why nobody notices. These tests read the
//! shipped examples as DATA and check them against the binary's own flags.
//!
//! Three claims:
//!   1. every `BORE_WEB_TRANSFER_*` name in an example is a name the CLI
//!      actually reads;
//!   2. no example publishes a new UDP port for this feature — the direct
//!      path is browser-to-browser and the server opens no socket for it;
//!   3. no example mounts a volume for it — the server holds no payload, so a
//!      write-capable mount would be a place for one to appear.
use std::collections::BTreeSet;

const COMPOSE: &[&str] = &[
    "docker/docker-compose.server.yml",
    "docker/docker-compose.server.prod.yml",
    "docker/docker-compose-full-yml.yml",
];

/// Every `BORE_WEB_TRANSFER_*` the CLI declares, read from its own source.
fn declared_env_names() -> BTreeSet<String> {
    let main = std::fs::read_to_string("src/main.rs").expect("read src/main.rs");
    collect_names(&main)
}

fn collect_names(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = text.as_bytes();
    let needle = b"BORE_WEB_TRANSFER_";
    let mut i = 0;
    while let Some(found) = bytes[i..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + i)
    {
        let mut end = found;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        out.insert(text[found..end].to_string());
        i = end;
    }
    out
}

#[test]
fn deploy_examples_name_only_environment_variables_the_cli_reads() {
    let declared = declared_env_names();
    assert!(
        declared.contains("BORE_WEB_TRANSFER_BASE_URL"),
        "the CLI no longer declares a base URL variable: {declared:?}"
    );
    for path in COMPOSE {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let used = collect_names(&text);
        assert!(
            used.contains("BORE_WEB_TRANSFER_BASE_URL"),
            "{path} does not document the variable the feature is gated on"
        );
        for name in &used {
            assert!(
                declared.contains(name),
                "{path} names {name}, which the CLI does not read"
            );
        }
    }
}

#[test]
fn deploy_examples_add_no_udp_port_and_no_payload_volume_for_web_transfer() {
    // The UDP ports that existed BEFORE this feature: the control port's own
    // STUN responder and the vhost QUIC frontends. Web transfer must add
    // nothing here — its direct path is negotiated between two browsers.
    const KNOWN_UDP: &[&str] = &["7835", "80", "443"];
    for path in COMPOSE {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let doc: serde_yaml::Value = serde_yaml::from_str(&text).expect("compose parses");
        let services = doc
            .get("services")
            .and_then(|s| s.as_mapping())
            .expect("compose has services");
        for (name, service) in services {
            let name = name.as_str().unwrap_or("<service>");
            if let Some(ports) = service.get("ports").and_then(|p| p.as_sequence()) {
                for port in ports {
                    let Some(text) = port.as_str() else { continue };
                    if !text.ends_with("/udp") {
                        continue;
                    }
                    let host = text.split(':').next().unwrap_or("");
                    assert!(
                        KNOWN_UDP.iter().any(|known| host.starts_with(known)),
                        "{path}: service {name} publishes an unexpected UDP port {text}"
                    );
                }
            }
            if let Some(volumes) = service.get("volumes").and_then(|v| v.as_sequence()) {
                for volume in volumes {
                    let Some(text) = volume.as_str() else {
                        continue;
                    };
                    let lower = text.to_ascii_lowercase();
                    for forbidden in ["transfer", "upload", "payload", "tmp"] {
                        assert!(
                            !lower.contains(forbidden),
                            "{path}: service {name} mounts {text}; the server stores no payload"
                        );
                    }
                }
            }
        }
    }
}

/// A commented default in a deployment example is a CLAIM about the product.
///
/// An operator reads `# - BORE_WEB_TRANSFER_MAX_RELAYS=256` as "that is what
/// it does today", and sizes a host around it. When the shipped default moves
/// and the comment does not, the example becomes a quiet lie — and this test
/// caught exactly that while the examples were being written: five of the
/// twelve values were wrong on the first pass.
#[test]
fn deploy_examples_quote_the_shipped_defaults() {
    let limits = bore_cli::web_transfer::WebTransferLimits::default();
    let expected: &[(&str, u64)] = &[
        ("BORE_WEB_TRANSFER_MAX_ROOMS", limits.max_rooms),
        ("BORE_WEB_TRANSFER_MAX_PEERS", limits.max_peers_global),
        (
            "BORE_WEB_TRANSFER_MAX_PEERS_PER_ROOM",
            limits.max_peers_per_room,
        ),
        (
            "BORE_WEB_TRANSFER_MAX_OFFERS_PER_PEER",
            limits.max_offers_per_peer,
        ),
        (
            "BORE_WEB_TRANSFER_MAX_ENTRIES_PER_OFFER",
            limits.max_entries_per_offer,
        ),
        ("BORE_WEB_TRANSFER_MAX_OFFER_BYTES", limits.max_offer_bytes),
        (
            "BORE_WEB_TRANSFER_MAX_METADATA_PER_ROOM",
            limits.max_metadata_per_room_bytes,
        ),
        (
            "BORE_WEB_TRANSFER_MAX_METADATA_TOTAL",
            limits.max_metadata_total_bytes,
        ),
        (
            "BORE_WEB_TRANSFER_MAX_TRANSFERS_PER_PEER",
            limits.max_transfers_per_peer,
        ),
        ("BORE_WEB_TRANSFER_MAX_RELAYS", limits.max_relays_global),
        (
            "BORE_WEB_TRANSFER_RELAY_RATE",
            limits.relay_rate_bytes_per_s,
        ),
        ("BORE_WEB_TRANSFER_OWNER_GRACE", limits.owner_grace_secs),
    ];
    for path in COMPOSE {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for line in text.lines() {
            let line = line.trim().trim_start_matches('#').trim();
            let Some(rest) = line.strip_prefix("- ") else {
                continue;
            };
            let Some((name, value)) = rest.split_once('=') else {
                continue;
            };
            let Some((_, want)) = expected.iter().find(|(known, _)| *known == name) else {
                continue;
            };
            let got: u64 = value
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("{path}: {name} is not a number: {value:?}"));
            assert_eq!(
                got, *want,
                "{path}: {name} quotes {got}, the shipped default is {want}"
            );
        }
    }
}
