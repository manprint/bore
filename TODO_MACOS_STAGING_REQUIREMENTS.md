# TODO — macOS Staging Requirements (before distributing `bore` to testers)

> Gate before shipping the macOS `bore vpn` build to testers. The macOS runtime
> is **CI-validated for compile + device primitives only** (macos-14: build,
> clippy, unit tests, utun create/teardown, `pfctl` accepts the PF grammar,
> single-host apply/revert + SIGKILL reclaim). **No real end-to-end tunnel
> traffic has ever crossed on macOS.** This doc lists what must be tested and how
> to sign/notarize the binary before staging.

Status legend: ☐ not done · ◐ partial · ☑ done (CI/manual)

---

## 0. Already validated (do NOT re-do — context)

- ☑ Compile + `clippy --all-targets -D warnings` + `cargo test --features vpn` on macos-14.
- ☑ utun create → kernel `utunN` read-back → teardown (`examples/macos_vpn_spike create-teardown`).
- ☑ `pfctl` **accepts** the `pf_ruleset` grammar (syntax only — NOT forwarding correctness).
- ☑ Single-host `NetConfig::apply` → RAII revert + SIGKILL `stale_reclaim` (`apply-revert`, `leak-then-reclaim`).
- ☑ Linux zero-regression: netns 161/0.

**Gap:** everything below moves bytes between two hosts / on real hardware — none of it is covered by CI.

---

## 1. Functional scenarios (two-host, REQUIRED before any distribution)

Run on a real Mac (Apple Silicon, macOS 13+) + a Linux peer. Base checklist:
`docs/vpn/VPN_MACOS_ACCEPTANCE.md` (T-MAC-MANUAL). Each must PASS with real traffic.

- ☐ **F1 Relay bring-up + ping** — connector comes up on relay, `utunN` + /30 set, Linux peer pings the macOS overlay addr and back. (Proves the data plane actually moves packets on macOS — the single biggest unproven thing.)
- ☐ **F2 Direct QUIC upgrade** — relay→direct switch within the retry grid (no `--relay-only`); ping continues across the switch (seamless, no sustained loss).
- ☐ **F3 Direct→relay fallback** — kill the direct path (block UDP / change network); link falls back to warm relay in place, traffic continues, TUN preserved.
- ☐ **F4 Gateway netmap (`binat`) real traffic** — macOS `--advertise 192.168.7.0/24@10.77.0.0/24 --nat-masquerade`; Linux peer reaches a REAL host at 192.168.7.x via virtual 10.77.0.x. (PF rule *loads* in CI; this proves it actually NATs/forwards.)
- ☐ **F5 Plain gateway (masquerade, no netmap)** — `--advertise <real-subnet>` without `@virtual`; peer reaches the LAN; return path works.
- ☐ **F6 `--forward-accept`** — on a Mac with a restrictive PF/filter policy, peers reach hosts behind the gateway (PF `pass`).
- ☐ **F7 Listener side on macOS** — Mac as `bore vpn listen` (not just connect); pairing + traffic both directions.
- ☐ **F8 RAII teardown (Ctrl-C)** — utun gone, anchor `bore_vpn/<id>` empty, `net.inet.ip.forwarding` back to pre-run value.
- ☐ **F9 SIGKILL recovery** — `kill -9`, restart: `stale_reclaim` flushed stale anchor + restored forwarding; no leaked PF rules.
- ☐ **F10 Auto-reconnect** — drop the server / network; `--auto-reconnect` re-establishes; no leaked utun/anchor/route across reconnects.
- ☐ **F11 Flag warnings** — `--tun-queues 4 --stun-server x` log the macOS advisories; link still comes up.
- ☐ **F12 Multi-client hub** (if offered to testers) — `--max-clients N>1` with a macOS spoke; spoke isolation holds (`block`); per-peer keys/nonces correct.

## 2. Environment matrix (REQUIRED — covers tester-machine variance)

CI is ONE GitHub VM. Real Macs differ. Test each functional pass (at least F1/F2/F4/F8/F9) on:

- ☐ **macOS versions**: 13 Ventura, 14 Sonoma, 15 Sequoia (PF/utun behavior can shift across releases).
- ☐ **Apple Silicon** (M1/M2/M3). (Intel x86_64 Macs: decide if in scope — `vpn-cross-build` only `cargo check`s aarch64; an Intel target is untested.)
- ☐ **SIP enabled** (default) — confirm utun + PF still work (they should; verify).
- ☐ **Corporate MDM / managed Mac** — MDM profiles can pre-own PF or restrict `pfctl`; verify bore's anchor coexists and reverts.
- ☐ **An existing VPN active** (corporate VPN / utun already present) — utun name collision avoided (kernel auto-assign), PF anchor doesn't clobber theirs, teardown doesn't disable PF globally.
- ☐ **NAT variety** — CGNAT, symmetric NAT, double-NAT: relay always works; direct upgrade succeeds or cleanly stays on relay.
- ☐ **Network types** — Wi-Fi and Ethernet; LAN-egress iface detection (`route -n get`) picks the right interface in each.

## 3. Robustness / edge (REQUIRED for a quality beta)

- ☐ **R1 Throughput** — sustained transfer over relay and over direct; compare vs Linux; record numbers. (macOS UDP socket buffers may clamp; no `SO_*BUFFORCE` equivalent applied — verify the cap.)
- ☐ **R2 MTU / MSS / PMTU** — large flows over the tunnel don't black-hole; the hardcoded PF `scrub max-mss 1310` is correct for the default 1350 MTU on the real path; test with a path that has a smaller PMTU.
- ☐ **R3 Long-run stability** — link up ≥ several hours; no FD/handle leak, no drift, heartbeats keep it alive.
- ☐ **R4 Sleep / wake** — Mac sleeps and wakes; link recovers (reconnect) and cleans up; no stranded utun/PF.
- ☐ **R5 Network change mid-session** — Wi-Fi↔Ethernet / IP change; reconnect path; no leaked state.
- ☐ **R6 Concurrent links** — two `bore vpn` links on one Mac (distinct ids); `/var/run` refcount keeps `ip.forwarding` correct (last-out restore); anchors independent.
- ☐ **R7 Port clash** — two direct-path tunnels sharing a `--nat-udp-preferred-port` on one Mac don't steal each other's socket (no `SO_REUSEADDR`; ephemeral fallback) — the Linux T-STRESS-PORTCLASH analogue.

## 4. Negative / permission

- ☐ **N1 No root** — `bore vpn` without sudo fails with a clear error (not a panic / not a confusing BSD-tool error).
- ☐ **N2 PF not enableable** — if `pfctl -e` is blocked, error is clear; no half-applied state left behind.
- ☐ **N3 Teardown idempotency** — running teardown twice / reclaiming a non-existent anchor is a no-op (no error spam).

---

## 5. Binary signing & notarization (REQUIRED to distribute without friction)

Unsigned binaries are blocked by Gatekeeper; testers would need `xattr -d com.apple.quarantine`. For a clean beta, sign + notarize.

### 5.1 Prerequisites
- ☐ Apple Developer Program membership (paid).
- ☐ **Developer ID Application** certificate in the keychain (for distribution OUTSIDE the App Store). `security find-identity -v -p codesigning` shows it.
- ☐ An app-specific password (or App Store Connect API key) for `notarytool`.
- ☐ Decide the team id / signing identity to use in CI vs locally.

### 5.2 Build
- ☐ Release build per target: `cargo build --release --features vpn` on Apple Silicon (and Intel if in scope). Consider a universal binary via `lipo` if supporting both arches.

### 5.3 Codesign (hardened runtime)
- ☐ Sign with hardened runtime + a timestamp:
  `codesign --force --options runtime --timestamp --sign "Developer ID Application: <NAME> (<TEAMID>)" target/release/bore`
- ☐ **Entitlements**: a root CLI opening utun via the `PF_SYSTEM`/`com.apple.net.utun_control` kernel control does NOT need a special entitlement when run as root — BUT with hardened runtime, verify empirically. If blocked, add a minimal entitlements plist and re-sign with `--entitlements`. Candidates to test only if needed: `com.apple.security.cs.allow-unsigned-executable-memory` (unlikely), networking entitlements (NetworkExtension is NOT used — bore uses raw utun, so the NE entitlement is most likely NOT required). **Confirm on hardware; do not add entitlements speculatively.**
- ☐ Verify: `codesign --verify --strict --verbose=2 target/release/bore` and `spctl -a -vvv -t install` (will say "rejected" until notarized — expected).

### 5.4 Notarize + staple
- ☐ Package for submission (notarytool takes a zip/dmg/pkg): `ditto -c -k --keepParent target/release/bore bore.zip`.
- ☐ Submit + wait: `xcrun notarytool submit bore.zip --apple-id <id> --team-id <TEAMID> --password <app-specific-pw> --wait`.
- ☐ On success, **staple** (note: a bare binary inside a zip can't be stapled directly — staple a `.dmg`/`.pkg`, or ship a `.app`/`.pkg` wrapper). Decide distribution format:
  - **Option A — `.pkg` installer** (recommended for a sudo CLI): build a component pkg, sign with **Developer ID Installer** cert (`productsign`), notarize the pkg, `xcrun stapler staple bore.pkg`. Installs to `/usr/local/bin` and is the cleanest for testers.
  - **Option B — zip of the binary**: notarize the zip; testers can't get a stapled ticket on the bare binary, so Gatekeeper does an online check on first run (works if online; flag this).
- ☐ Verify final artifact on a clean Mac: download via browser (gets quarantine xattr), run — must NOT be blocked by Gatekeeper.

### 5.5 Tester instructions to ship with the build
- ☐ Document: install via the `.pkg` (or `xattr -d com.apple.quarantine ./bore` for the zip path).
- ☐ Document: `bore vpn` needs **sudo** (signing/notarization does NOT remove the root requirement).
- ☐ Document: PF must be enableable; the tool loads a per-link PF anchor and reverts on exit / next-run reclaim.
- ☐ Provide the macOS quirks (advisory `--tun-name`, ignored `--tun-queues`/hole-punch flags) from `docs/vpn/VPN_MACOS.md`.

### 5.6 (Optional, later) automate in CI
- ☐ Add a signed+notarized release job (separate from the validation CI) using stored secrets (Developer ID cert in a keychain, notarytool creds). Keep it OFF feature branches.

---

## 6. Go / No-go gate for staging

**Minimum to distribute to testers (beta):**
- All of §1 (functional two-host) PASS — especially F1, F2, F4, F8, F9.
- §2 on at least: the testers' actual macOS major versions + Apple Silicon, SIP enabled, and one "existing-VPN/MDM" Mac if any tester has one.
- §5 signing+notarization done (or testers explicitly briefed on the quarantine workaround + sudo).
- §4 negative cases give clean errors (no panics, no leaked state).

**Defer-able to during-beta (document as known/under-test):** §3 R1/R3/R5, Intel arch, §1 F12 hub.

**Hard blockers (do NOT distribute if any fails):** F1 (no real traffic), F8/F9 (leaks host network state / doesn't restore forwarding), N1/N2 (panics or leaves the Mac's networking broken without root/PF).

---

## 7. Open risks to watch

- PF rule **correctness** (not just syntax) is unproven — F4/F5 are the real test of `binat`/`nat`/`scrub`.
- `route -n get` LAN-iface detection on multi-interface Macs (VPN + Wi-Fi + Ethernet) may pick the wrong egress.
- No macOS equivalent of Linux `SO_*BUFFORCE` → direct-path throughput may be capped by `net.inet.udp.*` / socket buffers (R1).
- Teardown must NEVER globally `pfctl -d` or leave forwarding on — verify it only flushes its own anchor and refcounts forwarding (F8/F9, R6).
- macOS 15+ / future SIP changes could restrict utun or PF for non-NE apps — re-verify per OS bump.
