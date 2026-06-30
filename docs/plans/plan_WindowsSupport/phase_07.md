# Phase 6 — Documentation, packaging, and release hardening

> **Intent:** Make Windows support installable, operable, documented, and release-ready.
> **Shippable alone?** yes — docs/packaging can land after runtime and test validation.
> **Preconditions:** phase_06 DONE

---

## Sub-phases

### 6.1 Write Windows VPN architecture doc
- **Model:** Haiku 4.5 draft → Opus 4.8 final read
- **Files:** `docs/vpn/VPN_WINDOWS.md` (new), `docs/vpn/VPN.md`, `docs/vpn/VPN_MACOS.md`, `docs/vpn/VPN_NAT_ASSESSMENT.md`
- **Change:** Add `docs/vpn/VPN_WINDOWS.md` mirroring macOS docs style. Required sections: support status; prerequisites; WinTun DLL/driver; admin/elevation; command examples; TUN naming; single-queue/no-offload limitation; route management; IP forwarding; firewall/`--forward-accept`; NAT masquerade; overlapping-subnet `real@virtual`; direct QUIC/UDP; carriers; hub mode; stale reclaim; troubleshooting; known performance limits. Update main VPN doc to link Windows and list Linux/macOS/Windows parity. Professional technical prose only.
- **Unit tests:** docs link check if repo has one; otherwise none.
- **e2e tests:** none.
- **Done:** Windows architecture doc can be used by operator without reading plan; Opus final read confirms no unsupported feature claimed without green T-ID.

### 6.2 Write Windows acceptance and operations checklist
- **Model:** Haiku 4.5
- **Files:** `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` (new), `docs/vpn/VPN_MACOS_ACCEPTANCE.md`
- **Change:** Create acceptance checklist with exact commands and result table. Include manual/elevated steps for: WinTun install/lookup; public/secret/vhost/server/transfer/test-udp; VPN relay; direct upgrade/fallback; carriers; hub; gateway; NAT masquerade; overlapping-subnet netmap; forward-accept; stale reclaim; admin/status; cleanup verification. Follow existing macOS acceptance doc style. Include commands for PowerShell admin shell and cleanup commands to list/remove leaked adapters/routes/firewall/NAT/rules/state.
- **Unit tests:** none.
- **e2e tests:** T-WIN-ACCEPTANCE — all checklist required rows filled with PASS, logs path, Windows version, bore version string.
- **Done:** Acceptance doc complete and current with test IDs; no open required rows for release.

### 6.3 Update install/package docs
- **Model:** Haiku 4.5
- **Files:** `docs/INSTALL_BORE.md`, `docs/vpn/VPN.md`, `README.md` if it lists supported platforms, release packaging workflow if present
- **Change:** Document Windows binary install and VPN prerequisites. Required: `bore.exe` placement; `wintun.dll` placement or env var; elevation requirement; Windows firewall prompt; server mode firewall rules; PowerShell examples; troubleshooting missing DLL/admin errors; uninstall/cleanup. Do not overstate automatic UAC prompt if implementation only errors.
- **Unit tests:** docs link check if available.
- **e2e tests:** T-WIN-INSTALL1 — fresh Windows VM follows install doc and passes T-WIN-LOCAL1 plus T-WIN-VPN-RELAY1.
- **Done:** Fresh install path verified or explicitly manual-tested; docs mention exact bore version string behavior.

### 6.4 Package/release workflow for Windows artifacts
- **Model:** Sonnet 4.6
- **Files:** `.github/workflows/ci.yml:1`, release workflow if present, `build.rs:1`, `Cargo.toml:1`
- **Change:** Add release artifact build for Windows MSVC. If `wintun.dll` is redistributed, pin official source/hash and include license/notice per upstream requirements. If not redistributed, package docs must require user to download/place it. Archive includes `bore.exe`, README/NOTICE snippets, checksums. Keep build.rs version embedding unchanged except Windows build compatibility fixes.
- **Unit tests:** `test_windows_release_archive_manifest` if packaging script is Rust/testable; otherwise CI artifact inspection step.
- **e2e tests:** T-WIN-PKG1 — download Windows artifact on clean Windows VM, run `bore --version`, run T-WIN-LOCAL1; with DLL installed/run T-WIN-VPN-RELAY1.
- **Done:** Windows release artifact reproducible and documented; no unpinned binary download in CI.

### 6.5 Security and cleanup review
- **Model:** Opus 4.8 review gate
- **Files:** `src/vpn.rs:3261`, `src/vpn.rs:4042`, `src/vpn.rs:4384`, `docs/vpn/VPN_WINDOWS.md` (new), `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` (new)
- **Change:** Review Windows-specific security posture. Required checks: no shell injection through PowerShell/netsh builders; no global firewall disable; no global NAT/forwarding left on after Drop; no world-writable state path attack; sanitized id/role/name used in rule names/files; admin check before side effects; stale reclaim cannot delete unrelated rules; missing DLL error cannot load untrusted path unless user explicitly sets env var; external binary hash pinned if downloaded.
- **Unit tests:** `test_windows_sanitize_*`; `test_windows_state_dir_permissions`; `test_windows_firewall_delete_exact_group_only`; `test_windows_dll_env_path_explicit_only`.
- **e2e tests:** T-WIN-SEC1 — malicious id/name containing shell metacharacters cannot alter command semantics; T-WIN-SEC2 — stale reclaim with similar rule names deletes only bore-owned exact group.
- **Done:** Security review notes recorded; all tests pass; no unresolved critical/high issues.

### 6.6 Final release-readiness gate
- **Model:** Opus 4.8 final read
- **Files:** `docs/plans/plan_WindowsSupport/overview.md`, `docs/plans/plan_WindowsSupport/resume.md`, `docs/vpn/VPN_WINDOWS.md`, `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md`, `.github/workflows/ci.yml:1`
- **Change:** Final sign-off. Verify every overview reference scenario item maps to green T-IDs. Verify docs match implementation. Verify resume statuses all DONE or approved deferral. Verify no production code written by plan skill is implied; implementation commits separate. Verify Linux/macOS zero-regression evidence: CI logs, netns where available, macOS e2e. Verify Windows feature support statement is accurate.
- **Unit tests:** full unit matrix.
- **e2e tests:** full acceptance matrix.
- **Done:** Release notes can state Windows support complete for public, secret, vhost, server, transfer/test-udp, and VPN with listed prerequisites.

---

## Phase gates

- **Fmt:** `cargo fmt --all`
- **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Test subset:** full Linux/macOS/Windows unit matrix
- **Docs:** all new docs reviewed; links valid if checker exists
- **Acceptance:** T-WIN-ACCEPTANCE, T-WIN-INSTALL1, T-WIN-PKG1, T-WIN-SEC1/2
- **Regression guard:** No required feature remains documented as unsupported unless user explicitly approves deferral

## Phase done criterion

Phase 6 is done when Windows support is documented, packaged, security-reviewed, acceptance-tested, and release-ready for all bore modes.
