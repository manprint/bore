# Contributing

Thanks for your interest in this fork of [bore](https://github.com/ekzhang/bore).

## Development

```shell
cargo build --all-features          # build (CI builds with --all-features)
cargo test                          # run all tests
cargo test basic_proxy              # run a single test by name
```

The crate ships the library (`bore_cli`) and a single `bore` binary that runs as
client, `proxy`, `server`, or `test-udp`. All code is safe Rust
(`#![forbid(unsafe_code)]`). Architecture notes live in `CLAUDE.md`; the UDP
direct path and NAT model are in `NAT_TRAVERSAL.md`.

## Before opening a PR

All three CI gates must pass locally (warnings are errors):

```shell
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings   # `udp` off
cargo test
```

Notes:
- **Integration tests bind real ports and run serially** via a shared
  `SERIAL_GUARD` mutex; tests that start a `Server` must take it. They fail if the
  control port (7835) is already in use.
- The **`udp`** feature is on by default and pulls `quinn`. Keep the server-side
  signaling (STUN, brokering) compiling **without** the feature — verify with
  `--no-default-features`.
- Keep `#![forbid(unsafe_code)]`.
- Add a test with any behavior change; UDP direct-path tests are in
  `tests/udp_test.rs` (loopback), relay/secret tests in `tests/secret_test.rs`.

## Commits & changelog

- Keep commits focused; explain the *why* in the body when it isn't obvious.
- Note user-facing changes in `CHANGELOG.md` under the unreleased / next version.

## Reporting NAT / hole-punch issues

Run `bore test-udp` (and `bore test-udp --to <your server>`) on **both** peers and
include the output — it classifies the NAT and is the fastest way to triage a
direct-path problem. See `NAT_TRAVERSAL.md` for how to read it.
