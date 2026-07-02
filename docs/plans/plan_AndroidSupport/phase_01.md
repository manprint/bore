# Phase 1 — Build matrix completion (pure additive)

> Precondition: none. This phase changes CI/build tooling only — zero `src/`
> edits. Lands safely on its own (I-A2).
> Postcondition: both android targets build AND pass clippy `-D warnings` in
> CI with default features; the vpn-feature check keeps passing (module still
> cfg'd out until phase 3).

Context for the implementer (do not re-explore):

- `ci.yml` already has a job `vpn-cross-build` (around lines 60-110) that
  cross-checks `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`,
  `aarch64-linux-android`. The android leg installs `cargo-ndk` via
  `taiki-e/install-action@v2`, sets `CC_aarch64_linux_android=aarch64-linux-android${ANDROID_API}-clang`,
  and runs `cargo ndk -t arm64-v8a check ... --features vpn`.
- `Justfile:19` defines `android_api`; `Justfile:82` defines recipe
  `android-arm64` used by releases; `docker/Dockerfile.android` builds the
  released aarch64 binary with all features.
- GitHub-hosted `ubuntu-latest` runners preinstall the NDK and expose
  `ANDROID_NDK_HOME`.

---

### 1.1 — Add `x86_64-linux-android` to the CI cross matrix

**Model:** Haiku
**Files:** `.github/workflows/ci.yml` (job `vpn-cross-build`, android steps ~60-110)
**Change:**
1. Locate the existing android steps inside `vpn-cross-build`.
2. Add rustup target `x86_64-linux-android` next to `aarch64-linux-android`
   (same `dtolnay/rust-toolchain` / `rustup target add` mechanism already used).
3. Duplicate the android check step for the new target: `cargo ndk -t x86_64
   check ...` with env `CC_x86_64_linux_android=x86_64-linux-android${ANDROID_API}-clang`.
   Keep the same feature flags as the existing aarch64 step (both the default
   and the `--features vpn` invocation, exactly mirroring what exists).
4. Do NOT touch the windows/macos legs, the release workflows, or
   `Dockerfile.android`.
**Unit tests:** none (CI yaml).
**e2e tests:** **T-AND-B1** — push branch; `vpn-cross-build` green with both
android targets visible in the log.
**Done-criteria:** CI run green; `git diff` limited to `ci.yml`; aarch64 steps
byte-identical to before except any shared refactor that keeps behavior equal.

---

### 1.2 — Upgrade android CI from `check` to `clippy -D warnings`

**Model:** Haiku
**Files:** `.github/workflows/ci.yml` (same job)
**Change:**
1. Change both android invocations from `cargo ndk ... check` to
   `cargo ndk ... clippy --all-targets -- -D warnings` (mirror the flags the
   `macos-vpn-build` job uses for clippy; keep `--features vpn` variant).
2. If clippy surfaces existing warnings on android targets, FIX them only if
   trivial (unused import under cfg); otherwise report them — do not `#[allow]`
   blanket-style. Expected: none (module cfg'd out).
**Unit tests:** none.
**e2e tests:** **T-AND-B2** — CI green with clippy in the android legs.
**Done-criteria:** `-D warnings` enforced for both android targets, default +
vpn feature sets.

---

### 1.3 — Justfile emulator target + API-level consistency

**Model:** Haiku
**Files:** `Justfile`
**Change:**
1. Add recipe `android-x86_64` cloned from `android-arm64` (`Justfile:82`)
   with target `x86_64-linux-android` / ndk arch `x86_64`. Purpose: local and
   CI builds of the emulator binary in later phases.
2. Verify `android_api` (`Justfile:19`) is `24`; if different, align the CI
   `ANDROID_API` env and the Justfile to the same value (pick 24 — Termux
   minimum) in ONE commit, and state the old value in the commit message.
3. No new directories; recipe sits beside the existing android recipe.
**Unit tests:** none.
**e2e tests:** **T-AND-B3** — `just android-x86_64` on the dev box (NDK
present) produces `target/x86_64-linux-android/release/bore`.
**Done-criteria:** recipe works locally; API level consistent across Justfile,
ci.yml, Dockerfile.android (read-only check for the Dockerfile — change it
only if inconsistent).

---

## Phase gates

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` on the Linux host: green (nothing should have changed).
- Full CI matrix green (linux, macos, windows, cross-build, netns e2e).

**Phase done when:** T-AND-B1..B3 pass and gates green. Update `resume.md`.
