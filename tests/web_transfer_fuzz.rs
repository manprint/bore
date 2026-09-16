//! Time-budgeted property fuzzing of every web-transfer decoder (6.3).
//!
//! The corpus test in `src/web_transfer_protocol.rs` pins the cases we THOUGHT
//! of; this one spends a wall-clock budget on the ones we did not. It is
//! deterministic by construction — a fixed seed plus a counter, printed on
//! every failure — so a crash found in CI reproduces exactly on a workstation
//! with `BORE_WEB_FUZZ_SEED=<seed>`.
//!
//! Three properties, none of which depends on the input being valid:
//!
//!   * **no panic** — a decoder answers `Err`, never unwinds. A panic here
//!     fails the test process, which is the whole point;
//!   * **no hang** — every single call is timed and must stay under
//!     `MAX_CALL`, so a quadratic or unbounded loop shows up as a failure
//!     rather than as a CI timeout with no attribution;
//!   * **no allocation past the cap** — the input itself never exceeds the
//!     cap the real caller enforces, and a decoder that answers `Ok` must
//!     have produced a value bounded by the limits it was given.
//!
//! The budget is `BORE_WEB_FUZZ_SECS` **per decoder**, defaulting to one
//! second so an ordinary `cargo test` run stays fast; the plan's 60 s run is
//! the same test with the variable set, which is what makes it CI-safe.
use bore_cli::web_transfer::WebTransferLimits;
use bore_cli::web_transfer_protocol as proto;
use std::time::{Duration, Instant};

/// Wall clock a single decoder call may take on ONE input.
///
/// Generous by design: a loaded CI box is slow and a flaky gate is worse than
/// no gate. What it catches is a decoder whose cost is not bounded by its
/// input size at all — the shape that turns a 320 KiB control message into a
/// minute of CPU.
const MAX_CALL: Duration = Duration::from_millis(500);

/// Per-decoder budget, `BORE_WEB_FUZZ_SECS` seconds (default 1).
fn budget() -> Duration {
    let secs = std::env::var("BORE_WEB_FUZZ_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1);
    Duration::from_secs(secs)
}

/// Seed, `BORE_WEB_FUZZ_SEED` (default fixed). Printed on failure.
fn seed() -> u64 {
    std::env::var("BORE_WEB_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0x5715_2026_0916_0001)
}

/// xorshift64*, so the corpus is reproducible without a dev-dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Bytes a mutation may splice in: the characters that decide how a JSON
/// parser branches, plus a few that decide how a UTF-8 decoder does.
const INTERESTING: &[u8] = b"{}[]\",:0123456789.eE+-tfnul \\/\x00\x7f\xc3\xa9\xff\xf0\x9f";

/// One mutation of `seed`, capped at `cap` bytes.
fn mutate(rng: &mut Rng, seed: &[u8], cap: usize) -> Vec<u8> {
    let mut out = seed.to_vec();
    let rounds = 1 + rng.below(8);
    for _ in 0..rounds {
        if out.is_empty() {
            out.push(INTERESTING[rng.below(INTERESTING.len())]);
            continue;
        }
        match rng.below(5) {
            // Flip a byte.
            0 => {
                let at = rng.below(out.len());
                out[at] ^= 1 << rng.below(8);
            }
            // Splice in a byte the parser branches on.
            1 => {
                let at = rng.below(out.len() + 1);
                out.insert(at, INTERESTING[rng.below(INTERESTING.len())]);
            }
            // Truncate — every prefix of a valid message is a hostile one.
            2 => {
                let at = rng.below(out.len());
                out.truncate(at);
            }
            // Delete a byte.
            3 => {
                let at = rng.below(out.len());
                out.remove(at);
            }
            // Repeat a slice, which is how a decoder is asked to allocate.
            _ => {
                let at = rng.below(out.len());
                let len = 1 + rng.below(out.len() - at);
                let slice = out[at..at + len].to_vec();
                if out.len() + slice.len() <= cap {
                    out.extend_from_slice(&slice);
                }
            }
        }
        if out.len() > cap {
            out.truncate(cap);
        }
    }
    out
}

/// Runs `body` over mutated seeds until the budget is spent.
///
/// `cap` is the byte cap the REAL caller enforces before the decoder is ever
/// reached, so feeding past it would be testing a case production cannot
/// produce.
fn fuzz<F>(name: &str, seeds: &[&str], cap: usize, mut body: F)
where
    F: FnMut(&[u8]),
{
    let mut rng = Rng(seed() ^ name.bytes().map(u64::from).sum::<u64>());
    let deadline = Instant::now() + budget();
    let mut cases: u64 = 0;
    while Instant::now() < deadline {
        for base in seeds {
            let input = mutate(&mut rng, base.as_bytes(), cap);
            let started = Instant::now();
            body(&input);
            let took = started.elapsed();
            assert!(
                took < MAX_CALL,
                "{name} took {took:?} on case {cases} (seed {:#x}, {} bytes)",
                seed(),
                input.len(),
            );
            cases += 1;
        }
    }
    println!("{name}: {cases} cases in {:?}", budget());
    assert!(cases > 0, "{name} ran no cases");
}

const ENVELOPE_SEEDS: &[&str] = &[
    r#"{"v":1,"type":"ping","body":{}}"#,
    r#"{"v":1,"type":"peer.rename","requestId":"00112233445566778899aabbccddeeff","body":{"displayName":"a"}}"#,
    r#"{"v":1,"type":"transfer.progress","requestId":"00112233445566778899aabbccddeeff","body":{"transferId":"00112233445566778899aabbccddeeff","receivedBytes":"1"}}"#,
    r#"{"v":1,"type":"rtc.offer","requestId":"00112233445566778899aabbccddeeff","body":{"transferId":"00112233445566778899aabbccddeeff","attemptId":"00112233445566778899aabbccddeeff","sdp":"v=0"}}"#,
];

#[test]
fn envelope_decoder_is_panic_free_under_fuzzing() {
    fuzz(
        "parse_client_envelope",
        ENVELOPE_SEEDS,
        bore_cli::web_transfer::WEB_TRANSFER_MAX_CONTROL_BYTES,
        |input| {
            let Ok(text) = std::str::from_utf8(input) else {
                return;
            };
            // A decoded envelope is dispatched by type, exactly as the real
            // control loop does — the envelope parser alone would not reach
            // any of the body decoders.
            if let Ok(env) = proto::parse_client_envelope(text) {
                let _ = proto::parse_hello_body(&env);
                let _ = proto::parse_rename_body(&env);
                let _ = proto::parse_progress_body(&env);
                let _ = proto::parse_complete_body(&env);
                let _ = proto::parse_reject_body(&env);
                let _ = proto::parse_cancel_body(&env);
                let _ = proto::parse_withdraw_body(&env);
                let _ = proto::parse_source_ready_body(&env);
                let _ = proto::parse_direct_ready_body(&env);
                let _ = proto::parse_direct_failed_body(&env);
                let _ = proto::parse_rtc_ice_body(&env);
                let _ = proto::parse_rtc_sdp_body(&env, "rtc.offer");
                let _ = proto::parse_publish_body(&env);
                let _ = proto::parse_transfer_request_body(&env);
            }
        },
    );
}

const VALUE_SEEDS: &[&str] = &[
    r#"{"offer":"00112233445566778899aabbccddeeff","label":"a.bin","kind":"file","createdAt":"2026-09-16T00:00:00Z","totalBytes":"1","entries":[{"id":"0","path":"a.bin","size":"1","chunks":["00"]}]}"#,
    r#"{"verifiedRanges":[["0","1"]],"outputLength":"1"}"#,
    r#"[["0","1"],["2","3"]]"#,
];

#[test]
fn value_decoders_are_panic_free_under_fuzzing() {
    let limits = WebTransferLimits::default();
    fuzz(
        "manifest_and_resume_decoders",
        VALUE_SEEDS,
        bore_cli::web_transfer::WEB_TRANSFER_MAX_MANIFEST_BYTES,
        |input| {
            let Ok(text) = std::str::from_utf8(input) else {
                return;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                return;
            };
            if let Ok(manifest) = proto::parse_manifest(&value, &limits) {
                // A manifest that parsed must be bounded by the limits it was
                // parsed against, whatever the input claimed.
                assert!(manifest.entries.len() as u64 <= limits.max_entries_per_offer);
            }
            let _ = proto::parse_resume_descriptor(&value);
            if let Ok(ranges) = proto::parse_verified_ranges(&value) {
                assert!(ranges.len() <= proto::MAX_RESUME_RANGES);
            }
        },
    );
}

#[test]
fn relay_attach_decoder_is_panic_free_under_fuzzing() {
    fuzz(
        "parse_relay_attach",
        &[
            r#"{"v":1,"role":"source","transferId":"00112233445566778899aabbccddeeff","ticket":"00112233445566778899aabbccddeeff"}"#,
            r#"{"v":1,"role":"recipient","transferId":"00112233445566778899aabbccddeeff","ticket":"00112233445566778899aabbccddeeff"}"#,
        ],
        bore_cli::web_transfer::WEB_TRANSFER_MAX_CONTROL_BYTES,
        |input| {
            if let Ok(text) = std::str::from_utf8(input) {
                let _ = proto::parse_relay_attach(text);
            }
        },
    );
}

#[test]
fn sealed_frame_decoder_is_panic_free_under_fuzzing() {
    // A real sealed frame is the seed: mutating one exercises the header, the
    // length field, the sequence check and the AEAD in proportion, which
    // random bytes alone never would (they die on the magic).
    let key = [7u8; 32];
    let sealed = proto::seal_frame(&key, 0, proto::FrameType::Data, b"hello frame")
        .expect("seal a frame for the corpus");
    let seed_text: String = sealed.iter().map(|b| *b as char).collect();
    let mut rng = Rng(seed() ^ 0x_f00d);
    let deadline = Instant::now() + budget();
    let mut cases: u64 = 0;
    while Instant::now() < deadline {
        let input = mutate(
            &mut rng,
            seed_text.as_bytes(),
            bore_cli::web_transfer::WEB_TRANSFER_MAX_RELAY_FRAME_BYTES,
        );
        let started = Instant::now();
        let _ = proto::open_frame(&key, &input, 0);
        let took = started.elapsed();
        assert!(
            took < MAX_CALL,
            "open_frame took {took:?} on case {cases} (seed {:#x}, {} bytes)",
            seed(),
            input.len(),
        );
        cases += 1;
    }
    println!("open_frame: {cases} cases in {:?}", budget());
    assert!(cases > 0, "open_frame ran no cases");
}
