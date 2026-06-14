# syntax=docker/dockerfile:1

# ── chef base ────────────────────────────────────────────────────────────────
# cargo-chef caches dependency compilation in a layer keyed ONLY on the
# dependency "recipe" (derived from Cargo.toml/Cargo.lock). A source-only change
# reuses the cooked-deps layer from the build cache instead of recompiling
# ring/quinn/tokio/etc — which is the bulk of the build time. This base layer
# (toolchain + cargo-chef) is itself identical across builds, so it is cached
# and `cargo-chef` is compiled at most once per platform (cold cache only).
# build-base provides the C toolchain needed to compile `ring` (TLS) on musl.
FROM rust:alpine AS chef
RUN apk --no-cache add musl-dev build-base \
    && cargo install cargo-chef --locked
WORKDIR /home/rust/src

# ── planner ──────────────────────────────────────────────────────────────────
# Distill the dependency graph into recipe.json. The full source is copied here,
# but this stage's only OUTPUT is recipe.json, so editing src/ does not change
# the recipe (and thus does not bust the cook layer below) unless deps changed.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── builder ──────────────────────────────────────────────────────────────────
FROM chef AS builder
# Cook ONLY the dependencies FIRST. Invalidated solely by recipe.json (a real
# dependency change), NOT by editing src/ NOR by the per-commit git SHA below —
# so it cache-hits on every source-only build, per platform. `cargo build`
# (below) reuses this target dir; `cargo install` would build in a separate dir
# and defeat the cache.
COPY --from=planner /home/rust/src/recipe.json recipe.json
RUN cargo chef cook --release --locked --features vpn --recipe-path recipe.json
# Git metadata build args come AFTER the cook step on purpose: BORE_GIT_SHA
# changes on every commit, and an ENV before `cook` would bust the cooked-deps
# layer every build. Placed here it only invalidates the cheap app-compile
# layer. Passed explicitly in CI (docker/build-push-action `build-args`); when
# omitted the binary shows "unknown" for branch/SHA.
ARG BORE_GIT_BRANCH=unknown
ARG BORE_GIT_SHA=unknown
ENV BORE_GIT_BRANCH=${BORE_GIT_BRANCH}
ENV BORE_GIT_SHA=${BORE_GIT_SHA}
# Now the real source. Only the `bore` crate recompiles from here; the cooked
# dependencies are fingerprint-matched and skipped.
COPY . .
RUN cargo build --release --locked --features vpn

# ── runtime ──────────────────────────────────────────────────────────────────
FROM scratch
COPY --from=builder /home/rust/src/target/release/bore .
USER 1000:1000
ENTRYPOINT ["./bore"]
