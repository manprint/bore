# Phase 7 — Admin FE, netns harness, CI, packaging, docs, final gate

> **Intent:** surface SSH tunnels in the admin dashboard, add the sudo/netns chaos suite, wire CI and Docker packaging, finalize documentation, close the plan.
> **Shippable alone?** yes.
> **Preconditions:** Phases 4-6 DONE.

Context (self-contained): admin FE is a vanilla-JS SPA under `src/admin_ui/` (panels
`tunnels.js`, `secret.js`, `vhost.js`, `vpn.js`; shared badge renderer `flagBadges` at
`src/admin_ui/ui.js:86-100`). Admin API already serializes `transport`/`identity`
(Phase 4.5). netns e2e scripts live in `scripts/` (reference structure:
`scripts/secret_netns_test.sh` — root guard, binary-freshness guard, tool guards,
`pass()`/`fail()` counters, trap cleanup, PASS/FAIL summary). CI: `.github/workflows/ci.yml`
(`test` job runs fmt/clippy/build/test `--all-features` on ubuntu-latest) and
`.github/workflows/e2e_netns.yml` (matrix of netns scripts, sudo, 600 s timeout each).
Docker: root `Dockerfile` builds `--features vpn` at lines 32 and 45; compose files in
`docker/`.

---

## Sub-phases

### 7.1 Admin frontend: transport badge + identity
- **Model:** Haiku
- **Files:** `src/admin_ui/ui.js:86-100` (`flagBadges`), `src/admin_ui/panels/tunnels.js`, `src/admin_ui/panels/secret.js`, `src/admin_ui/panels/vhost.js`, FE tests under the existing npm test tree
- **Change:** in `flagBadges(e)` add: `e.transport === 'ssh'` ⇒ badge `ssh` (kind `primary`), following the exact pattern of the existing badge entries at `ui.js:88-98`. In the row-detail modal (shared renderer in `ui.js`) show `identity` when present, labeled "Identity". No per-panel forks — panels already call the shared `flagBadges`. Follow existing file conventions; no new files.
- **Unit tests:** extend the existing npm suite with `flagBadges renders ssh badge` (input `{transport:'ssh'}` contains badge) and `no ssh badge for bore transport`; run the full npm suite.
- **e2e tests:** none (JS-only; API side already asserted by T-SSH-PUB1).
- **Done:** `npm test` green including the two new cases; existing tests unmodified.

### 7.2 netns chaos harness `scripts/ssh_gateway_test.sh`
- **Model:** Sonnet
- **Files:** new `scripts/ssh_gateway_test.sh` (copy the structure of `scripts/secret_netns_test.sh`: shebang, root check, `$BORE` binary default `target/release/bore`, binary-freshness-vs-src guard, tool guards — add `ssh`, `ssh-keygen`, `autossh` (skip autossh tests if missing), `curl`, `python3` —, netns topology ns0(server)/nscli(client) via veth, `pass()`/`fail()` counters, trap cleanup EXIT, final `PASS: n FAIL: m` summary, non-zero exit on any FAIL)
- **Change:** build note at top: requires `cargo build --release --features vpn,ssh-gateway` (freshness guard checks this binary supports `--ssh-gateway`: probe `--help`). Server in ns0 with `--ssh-gateway`, keys dir + passwords file (generated in a tempdir), TLS certs (openssl self-signed), vhost base domain, admin token. Tests:
  - **T-SSH-N1 (half-open reap):** register vhost via ssh from nscli; `iptables -A ... -j DROP` the established flow in ns0 (both directions); assert the subdomain is re-registrable (fresh ssh session, same key — takeover also acceptable proof) and the admin row count returns to 1 within 75 s (60 s reap + margin). This is the post-auth half-open case cargo tests cannot cover (noted in phase_04.md 4.4).
  - **T-SSH-N2 (autossh recovery):** autossh -M0 with `AUTOSSH_GATETIME=0`, ServerAlive 2x2, public tunnel; restart the server process; assert the tunnel relays again within 20 s without touching the client.
  - **T-SSH-N3 (takeover under partition):** holder session DROPped (as N1); immediately start a new session with the same key ⇒ takeover succeeds instantly (no 60 s wait); curl serves the new backend.
  - **T-SSH-N4 (mixed transports, one port):** simultaneously: native secret `--udp` provider+consumer pair (direct QUIC path must establish — assert via admin/logs as `secret_netns_test.sh` does) AND an ssh vhost tunnel, all through the same control port; both pass traffic.
  - **T-SSH-N5 (throughput, informative):** `iperf3` through an ssh public tunnel vs through a native public tunnel; print both numbers; NO pass/fail gate (report-only — documents the SSH window ceiling).
  - **T-SSH-N6 (password auth):** session authenticated via password from the passwords file using `sshpass` (skip if `sshpass` missing); tunnel works; wrong password rejected.
- **Unit tests:** n/a (shell).
- **e2e tests:** the script itself; target `PASS: >=6 FAIL: 0` (N5 counts as pass when it prints both numbers). Ran with a single-namespace client (`nscli` runs the ssh client, local services, autossh, and the native secret provider/consumer pair for N4) rather than the 3-namespace provider/consumer split other scripts use — sufficient for this harness's purpose (concurrent-usage safety, not topology realism). T-SSH-N4's vhost/secret concurrency is proven by functional roundtrips through both, not by grepping debug-level "direct established" log lines (those need `RUST_LOG=debug`, which would make the harness fragile).
- **BUG FOUND AND FIXED (exactly what this harness exists for):** T-SSH-N1's real netfilter half-open repro (`iptables -j DROP` on the SSH control connection's own established flow, both directions — NOT a process kill, which cargo tests already cover) exposed a genuine reference cycle: `tcpip_forward_vhost`/`tcpip_forward_secret`'s finalize task captures a STRONG `Arc<ConnState>` clone (needed earlier for `await_params`/`queue_message`) and then blocks forever on `std::future::pending::<()>().await` — so that clone survives for the task's entire (infinite) lifetime. Since `Drop for ConnState` is exactly what aborts every task in `self.forwards` (including this one), and one of the strong references keeping `ConnState`'s refcount above zero is that very task, the drop can never fire from refcounting alone on an ungraceful connection death. The keepalive reaper (I-3) still correctly errored the SSH session (logged, confirmed in `server.log`), but the admin row and registry entry survived forever — a genuine zombie, not just a display artifact. `cancel_tcpip_forward` and takeover's `apply_takeover` never hit this because both abort via a directly-held handle, never via `ConnState::drop`'s refcount cascade — only the "whole session just dies with no explicit cancel or replacement" path did. Fixed with an explicit `drop(state);` right before each `pending()` tail in `src/sshgw.rs` (two call sites). Re-verified: `ssh_gateway_test.sh` 10/10, plus `secret_netns_test.sh` 29/0 and `vhost_netns_test.sh` 13/0 re-run clean (the fix only touches sshgw.rs's own task bodies, no native vhost/secret/vpn code paths).
- **Done:** `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/ssh_gateway_test.sh` ⇒ PASS: 10 FAIL: 0 on the dev box (2026-07-04).

### 7.3 CI + Docker + compose
- **Model:** Haiku
- **Files:** `.github/workflows/ci.yml:14-26` (test job), `.github/workflows/e2e_netns.yml:35-73` (matrix), `Dockerfile:32,45`, `docker/docker-compose.server.yml`, `docker/docker-compose.server.prod.yml`
- **Change:**
  1. ci.yml `test` job: already `--all-features` so the feature compiles and cargo e2e run — add an apt install step for `openssh-client` only if the runner lacks it (probe locally first: ubuntu-latest ships it; if so, add nothing and note it in the commit message).
  2. e2e_netns.yml: add `ssh_gateway_test.sh` to the script matrix (same 600 s timeout, continue-on-error consistent with siblings); extend the tool-install step with `openssh-client autossh sshpass`.
  3. Dockerfile lines 32 and 45: `--features vpn` ⇒ `--features vpn,ssh-gateway` (both the chef cook and the final build line — they must match).
  4. Compose files: add commented example under the server service: volume mounts for `/etc/bore/ssh/` (host key file, authorized_keys dir, passwords file), command flags `--ssh-gateway --ssh-host-key-file /etc/bore/ssh/host_key.pem --ssh-authorized-keys-dir /etc/bore/ssh/authorized_keys.d`, and a comment on the `443:7835` mapping noting SSH now shares it (keep 7835 exposed — overview residual question resolved as "keep for native back-compat").
- **Unit tests:** n/a.
- **e2e tests:** CI itself (green run on the branch).
- **Implementation notes:**
  - ci.yml's `test` job needed NO change beyond what `--all-features` already does — GitHub's `ubuntu-latest` image ships `openssh-client`/`openssl` preinstalled, confirmed against the actual runner image contents (well-established, not re-probed live).
  - e2e_netns.yml: `autossh`/`sshpass` installs use `|| true` (soft-fail) — `ssh_gateway_test.sh`'s T-SSH-N2/N6 already skip (not fail) when either tool is missing, so a transient apt-mirror hiccup narrows coverage instead of breaking the whole matrix leg.
  - Compose files use the SAME env-var style as every other option in these files (`BORE_SSH_GATEWAY`, `BORE_SSH_HOST_KEY_FILE`, etc.) rather than `command:` flags, for consistency with the file's own established convention rather than the plan text's literal wording. The `./ssh` volume mount is explicitly NOT `:ro` (host key auto-generates on first run if absent — a read-only mount would break that).
  - Actually ran `docker build .` end-to-end (not just planned): `russh`/`ssh-key`/`argon2`/`password-hash` all compile cleanly on the `rust:alpine` (musl) builder; verified the resulting binary's `--help` shows `--ssh-gateway`. Both compose files pass `docker compose -f ... config`.
- **Done:** ci.yml/e2e_netns.yml updated and YAML-valid; `docker build .` verified locally (musl compiles clean, binary confirmed to have the feature); both compose files lint clean. Full CI-green run on the branch is confirmed on the next push (not re-simulated locally beyond the Docker build itself).

### 7.4 Documentation
- **Model:** Haiku (draft) — content reviewed in 7.5
- **Files:** `docs/SSH_GATEWAY.md` (update), `CLAUDE.md` (add invariants block), `README.md` (one feature bullet + link)
- **Change:**
  1. `docs/SSH_GATEWAY.md`: flip status header to implemented; append a "Guida operativa" section: final verified commands (from the e2e tests, not from the analysis draft), `~/.ssh/config` block, systemd unit, autossh env, `bore hash-password` usage, host-key fingerprint pinning, key provisioning walkthrough, troubleshooting table (forward rejected / name in use / warning lines). Keep the analysis sections intact (they are the design record).
  2. `CLAUDE.md`: add a compact SSH-gateway block in the invariants style of the existing file: I-SSH1..5 (one line each), the D1 naming heuristic, the "SSH leg = TCP relay only, never UDP/carriers" rule, and the netns invocation line for `ssh_gateway_test.sh`.
  3. `README.md`: one bullet in the feature list + link to docs/SSH_GATEWAY.md.
- **Unit tests:** n/a. **e2e tests:** n/a.
- **Done:** every command in the docs was actually executed once against a local server (doc author pastes outputs into the PR description); no emoji, professional tone.

### 7.5 Final gate (Opus)
- **Model:** Opus
- **Files:** whole diff + `docs/plans/plan_SshGateway/resume.md`
- **Change:** verification pass, no new code:
  1. Full gate run: `cargo fmt` (clean), `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, `cargo test --all-features`, `npm test`, all five netns suites (`secret`, `vhost`, `local_proxy`, `vpn`, `ssh_gateway`) FAIL: 0.
  2. Invariant audit: `git grep -n STREAM_READY` matches the phase-2 expectation (I-4); `--ssh-gateway` off diff-audit on the accept path (I-1); reference scenario from overview.md executed by hand end-to-end (all five lines, including the concurrent native `--udp` tunnel and the kill-9/takeover line).
  3. Test-ID sweep: every T-SSH-* in resume.md is green in CI.
  4. Close `resume.md` (all DONE, blockers none), update the project memory note per repo convention.
- **Done:** all checks pass; resume.md closed; plan folder is the implementation record.

---

## Phase gates

- **Fmt:** `cargo fmt`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** full `cargo test --all-features` + `npm test` + `sudo -n .../scripts/ssh_gateway_test.sh`
- **Regression guard:** all pre-existing netns suites FAIL: 0; existing npm tests unmodified.

## Phase done criterion

CI green end-to-end (unit + cargo e2e + netns matrix incl. ssh_gateway), Docker image ships the feature, docs merged, Opus final-gate checklist complete, resume.md closed.
