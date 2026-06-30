# Phase 1 — WinTun adapter backend

> **Intent:** Provide Windows TUN creation and packet I/O behind the same shape expected by the existing VPN bridge.
> **Shippable alone?** yes — Windows can create/read/write a TUN in targeted tests; host routes/NAT/firewall may still be unsupported.
> **Preconditions:** phase_01 DONE

---

## Sub-phases

### 1.1 Opus driver/API decision gate
- **Model:** Opus 4.8 design review → Sonnet implements
- **Files:** `Cargo.toml:77`, `src/vpn.rs:4091`, `src/vpn.rs:4203`, `docs/vpn/VPN_WINDOWS.md` (new), `docs/plans/plan_WindowsSupport/overview.md`
- **Change:** Confirm WinTun crate choice before coding. Required acceptance: L3 TUN semantics, Windows 10/11 support, safe packet I/O wrapper, clear distribution story for `wintun.dll`, license compatible with bore distribution, no global TAP driver dependency. External facts to verify and record in `docs/vpn/VPN_WINDOWS.md`: WinTun is a Windows L3 TUN driver distributed as `wintun.dll`; API lifecycle includes adapter create/open, session start/end, receive/release, allocate/send. Rust binding session semantics must support receive and send; allocated send packets must always be sent or the queue stalls. Use official WinTun and Rust binding docs: [WireGuard WinTun](https://github.com/WireGuard/wintun), [wintun-bindings Session](https://docs.rs/wintun-bindings/latest/wintun_bindings/struct.Session.html). If chosen crate differs from `wintun-bindings`, update this plan and docs with exact method names before implementation. Follow existing docs/vpn structure; do not create a new docs directory.
- **Unit tests:** none (design gate).
- **e2e tests:** none.
- **Done:** Decision recorded as D-WT1 in `docs/vpn/VPN_WINDOWS.md`; dependency added only after license/API review; plan references updated if crate method names differ.

### 1.2 Add Windows TUN abstraction without touching Unix types
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:4091`, `src/vpn.rs:4203`, `src/vpn.rs:7211`, `src/vpn.rs:7304`, `Cargo.toml:77`
- **Change:** Add Windows-only TUN adapter wrapper that exposes the minimal read/write interface needed by bridge code. Keep Linux/macOS using `tun_rs::AsyncDevice` unchanged. If current bridge signatures require `Vec<tun_rs::AsyncDevice>`, introduce a platform enum/type alias under cfg so Unix signatures remain unchanged and Windows signatures compile. Do not refactor Linux/macOS bridge bodies. Windows wrapper requirements: create/open adapter by requested name or generated `bore{N}`/`bore-wintun-{id}`; start WinTun session with bounded ring capacity; read IPv4 packets; write IPv4 packets; expose resolved interface name/LUID for route commands; close session before adapter deletion. No `tokio::io::split` on yamux streams. Follow existing `hostcfg` placement in `src/vpn.rs`; create a new source file only if it follows existing module conventions and reduces `vpn.rs` size without moving Unix code.
- **Unit tests:** `test_windows_tun_name_default_mapping` — `auto`/`bore0` maps to generated adapter name; `test_windows_tun_explicit_name_preserved` — explicit name preserved; `test_windows_tun_rejects_invalid_name_chars` — invalid Windows interface names rejected before WinTun call; `test_windows_tun_no_offload_flag` — Windows create result reports `offload=false`.
- **e2e tests:** T-WIN-TUN1 — elevated Windows test creates adapter, reads resolved name/LUID, sends one synthetic IPv4 packet through loopback harness if possible, then deletes adapter; post-check: adapter no longer exists.
- **Done:** Windows `cargo test --features vpn windows_tun -- --nocapture` passes on elevated runner/manual host; Linux/macOS `create_tun` bodies unchanged; `--tun-queues > 1` warns/clamps on Windows.

### 1.3 Implement WinTun read/write bridge adapter
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:6096`, `src/vpn.rs:7211`, `src/vpn.rs:7304`, new Windows-only adapter module if approved by 1.2
- **Change:** Wire Windows packet source/sink into existing bridge single-packet path. If WinTun read is blocking, use a dedicated task/thread boundary that sends packets through bounded channels and applies backpressure. Writes must honor WinTun send ordering: every allocated send packet is sent before allocating unbounded future packets; no dropped allocated packet can hold queue. Do not modify Linux offload pumps except cfg signatures if unavoidable. Preserve relay/direct sender semantics and packet-atomic TUN writes.
- **Unit tests:** `test_wintun_send_order_guard` — mock session fails if allocated packet is dropped unsent; adapter code returns error and shuts down cleanly; `test_wintun_read_shutdown_unblocks` — shutdown cancels blocking receive and bridge exits; `test_windows_tun_backpressure_blocks` — full channel pauses read side rather than dropping silently.
- **e2e tests:** T-WIN-TUN2 — elevated Windows test injects ICMP/UDP packet into TUN and verifies bridge receives exact bytes; T-WIN-TUN3 — bridge writes packet to TUN and host sees it via route/ping test once Phase 2 route exists.
- **Done:** Windows bridge path compiles and passes mock tests; no packet drops on queue-full except existing typed `TooLarge` handling; no new unbounded channels in hot path.

### 1.4 Integrate `hostcfg::create_tun` Windows twin
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:4091`, `src/vpn.rs:4203`, `src/vpn.rs:4384`
- **Change:** Replace Phase 0 unsupported stub with real Windows `create_tun` implementation matching Linux/macOS return contract. Behavior: require admin/elevated process; create/open WinTun adapter; configure overlay address/prefix if supported by adapter API or leave address setup to Phase 2 `NetConfig::apply`; set MTU through Windows route/MTU builder if WinTun cannot set it directly; return `(devices, false, resolved_name)` where `false` means no offload. If `queues > 1`, log warning and use one queue. If explicit adapter exists from previous crashed run, either reuse safely by id/name or delete/recreate only after stale reclaim.
- **Unit tests:** `test_create_tun_windows_requires_admin_error` — non-admin path returns clear error; `test_create_tun_windows_queues_warn_and_clamp` — `queues=4` creates one session and warns; `test_create_tun_windows_resolved_name_used` — returned name equals actual adapter name used by host config.
- **e2e tests:** T-WIN-TUN4 — `bore vpn connect --relay-only --no-route-manage` on Windows creates TUN and exits cleanly on Ctrl-C; adapter removed/released after exit.
- **Done:** `cargo check --features vpn --target x86_64-pc-windows-msvc`; elevated Windows TUN tests pass; Linux/macOS `cargo test --features vpn test_macos_tun_request test_pick_tun_name -- --nocapture` still pass.

### 1.5 Package and locate `wintun.dll`
- **Model:** Sonnet 4.6
- **Files:** `build.rs:1`, `Cargo.toml:77`, `docs/vpn/VPN_WINDOWS.md` (new), `.github/workflows/ci.yml:60`
- **Change:** Define how Windows binary finds `wintun.dll`. Preferred order: same directory as `bore.exe`; optional `BORE_WINTUN_DLL` env var for tests/dev; documented install step. Do not embed unreviewed binary blobs in repo. If CI downloads `wintun.dll`, pin URL/hash and cache it. Add clear error: missing DLL -> installation remediation. Follow existing build/version script style in `build.rs`; avoid changing version string logic.
- **Unit tests:** `test_wintun_dll_lookup_env_override`; `test_wintun_dll_missing_error_mentions_path`; `test_wintun_dll_hash_pin_doc_matches_ci` if CI downloads binary.
- **e2e tests:** T-WIN-TUN5 — Windows CI/manual run with DLL colocated creates adapter; run without DLL fails with documented error and no partial adapter remains.
- **Done:** Packaging doc complete; no binary committed unless user explicitly approves; CI path deterministic.

---

## Phase gates

- **Fmt:** `cargo fmt --all`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings`
- **Test subset:** `cargo test --features vpn windows_tun wintun -- --nocapture`
- **Cross-check:** `cargo check --features vpn --target x86_64-pc-windows-msvc`
- **Elevated Windows check:** T-WIN-TUN1, T-WIN-TUN4, T-WIN-TUN5 on Windows 11 admin shell
- **Regression guard:** Linux/macOS `create_tun` and bridge offload bodies unchanged except cfg/type plumbing reviewed by Opus

## Phase done criterion

Phase 1 is done when Windows can create, use, and cleanly release a WinTun adapter through bore's VPN bridge abstraction, with single-queue/no-offload semantics and no Linux/macOS regression.
