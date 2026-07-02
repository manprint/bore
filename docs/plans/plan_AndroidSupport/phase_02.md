# Phase 2 — Non-VPN runtime validation on the Android emulator

> Precondition: phase 1 done (x86_64-linux-android builds in CI).
> Postcondition: every non-VPN subcommand exercised end-to-end on Android
> (emulator) as non-root, via a new CI job; portability defects found here are
> fixed here.

Context for the implementer (do not re-explore):

- The emulator guest reaches the runner host at `10.0.2.2`. Host cannot reach
  guest ports directly — use `adb forward tcp:<host> tcp:<guest>` when the
  connection originates on the host.
- `adb shell` default user is `shell` (uid 2000): a good non-root oracle (no
  CAP_NET_ADMIN, no TUN, cannot bind <1024). `adb root` (works on AOSP
  `default` images) gives uid 0 — used only in phase 4.
- On-device writable dir: `/data/local/tmp`. There is NO `/tmp` on Android.
  This is why `cargo test` binaries are not pushed (tests hardcode `/tmp`) —
  D-A10. The e2e drive the release binary only.
- Emulator image ships toybox (`nc`, `sh`, `ping`, `ip` available in PATH).
- Existing script conventions: look at `scripts/secret_netns_test.sh` for the
  house style (numbered `T-*` cases, `set -u` care with ports, PASS/FAIL
  counters, cleanup traps). Follow it.

---

### 2.1 — `scripts/android_emu_test.sh` (host-side orchestrator)

**Model:** Sonnet
**Files:** new `scripts/android_emu_test.sh` (follows existing `scripts/`
conventions — no new directory)
**Change:** Write a bash script that assumes: an emulator is already running
and `adb` is on PATH; `$BORE_HOST_BIN` = linux host binary; `$BORE_ANDROID_BIN`
= x86_64-linux-android binary. Steps:
1. `adb push "$BORE_ANDROID_BIN" /data/local/tmp/bore && adb shell chmod 755 /data/local/tmp/bore`.
2. Start `"$BORE_HOST_BIN" server --min-port 40000 --max-port 40100` on the
   host (background, killed by trap).
3. Implement the cases below; each prints `PASS/FAIL T-AND-Ex`, script exits
   non-zero on any FAIL. Every `adb shell` bore process must be killed in the
   trap (`adb shell pkill -f /data/local/tmp/bore || true`).

Test cases (IDs stable, assert exactly this):
- **T-AND-E1 (public tunnel, guest→host server):** in guest run
  `nohup /data/local/tmp/bore local 8080 --to 10.0.2.2 --port 40010 &`; in
  guest run a one-shot HTTP responder:
  `printf 'HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello' | nc -l -p 8080` (toybox
  nc; adjust flag spelling to what the image's toybox accepts — probe with
  `nc --help` first and use `-s`/`-p` accordingly). On the HOST:
  `curl -sf http://127.0.0.1:40010` via the host server public port (server
  runs on the host, so the public port is host-local — no adb forward needed).
  Assert body == `hello`.
- **T-AND-E2 (transfer over public tunnel, integrity):** guest runs
  `bore transfer listener` with `--out /data/local/tmp/inbox` behind
  `bore local <port> --to 10.0.2.2 --port 40020`; host runs
  `bore transfer sender` targeting `127.0.0.1:40020` sending a 8 MiB random
  file. Assert exit 0 (BLAKE3 verify is built-in) and byte size matches via
  `adb shell stat`.
- **T-AND-E3 (secret tunnel, provider in guest):** guest:
  `bore local 8080 --to 10.0.2.2 --tcp-secret-id T_E3 ...` (reuse the E1 nc
  responder); host: `bore proxy --to 10.0.2.2 ... --tcp-secret-id T_E3
  --local-port 40031`, then `curl -sf http://127.0.0.1:40031` == `hello`.
  (Consult `scripts/secret_netns_test.sh` for exact proxy flag names.)
- **T-AND-E4 (test-udp diagnostic):** guest runs
  `/data/local/tmp/bore test-udp --to 10.0.2.2` (flag names per
  `bore test-udp --help`); assert exit 0 and output contains a NAT
  classification line. Reduced `procfs` info on Android is EXPECTED — do not
  assert interface details.
- **T-AND-E5 (public tunnel `--udp` direct path):** repeat E1 with `--udp` on
  both `bore server` and `bore local`. Assert the tunnel serves the request
  (direct OR fallback — either is a pass); grep client log for which path was
  taken and echo it (informational).
- **T-AND-E6 (non-root negative):** guest (uid shell):
  `/data/local/tmp/bore server --bind-addr 0.0.0.0 --control-port 80`
  must FAIL with a permission error; assert non-zero exit and stderr mentions
  the bind failure. (Exact flag names per `bore server --help`.)
**Unit tests:** none (script).
**e2e tests:** the script IS T-AND-E1..E6. Local dry-run possible with a
locally started emulator, otherwise validated by 2.2's CI run.
**Done-criteria:** script complete, shellcheck-clean, follows house
conventions, all six cases implemented with cleanup traps.

---

### 2.2 — CI job `android-emu-e2e`

**Model:** Sonnet
**Files:** `.github/workflows/ci.yml` (new job, mirror the placement/style of
the `macos-vpn-e2e` job)
**Change:**
1. Job on `ubuntu-latest`. Steps: checkout; rust toolchain; enable KVM
   (the standard udev snippet for hosted runners:
   `echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules`
   then `sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=kvm`);
   build host binary (`cargo build --release`); build android binary
   (`cargo ndk -t x86_64 -p ${ANDROID_API} build --release`); then
   `reactivecircus/android-emulator-runner@v2` with `api-level: 30`,
   `arch: x86_64`, `target: default`, and
   `script: BORE_HOST_BIN=... BORE_ANDROID_BIN=... scripts/android_emu_test.sh`.
2. Job must be REQUIRED (same needs/if pattern as macos-vpn-e2e — copy it).
3. Cache: reuse `Swatinem/rust-cache@v2` like sibling jobs.
**Unit tests:** none.
**e2e tests:** **T-AND-E-CI** — the job runs T-AND-E1..E6 green in CI.
**Done-criteria:** CI green including the new job; job wall-time < 20 min.

---

### 2.3 — Portability fixes surfaced by 2.2 (contingency)

**Model:** Sonnet
**Files:** whatever the failures point at (expected candidates: none — the
non-VPN binary already ships for aarch64; possible suspects are hardcoded
paths or missing `Host:` handling in the nc responder, i.e. script bugs).
**Change:** For each CI failure decide: (a) script/harness bug — fix the
script; (b) genuine `src/` portability defect — fix minimally WITHOUT touching
any `cfg(linux)` body (I-A1); add a regression unit test when the fix is in
Rust. If a defect is an Android platform limit instead (cannot fix), record it
in a `LIMITS` list in the script header AND carry it to the phase 5 docs;
adjust the test to assert the documented behavior (e.g. clear error), never
delete the test.
**Unit tests:** per-fix.
**e2e tests:** rerun T-AND-E-CI green.
**Done-criteria:** android-emu-e2e green 2 consecutive runs (flake check);
zero diffs inside existing cfg(linux) bodies; Linux gates green.

---

## Phase gates

- Linux host: `cargo fmt --check` && `cargo clippy --all-targets -- -D warnings`
  && `cargo test` — green.
- Full CI matrix green including new `android-emu-e2e`.
- Regression: `sudo -n /abs/path/scripts/vpn_netns_test.sh` NOT required this
  phase unless 2.3 touched `src/` — if it did, run it (exact-path sudo rule).

**Phase done when:** T-AND-E1..E6 + T-AND-E-CI green. Update `resume.md`.
