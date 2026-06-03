FROM rust:alpine AS builder
WORKDIR /home/rust/src
# build-base provides the C toolchain needed to compile `ring` (TLS) on musl.
RUN apk --no-cache add musl-dev build-base
# Git metadata build args: passed explicitly in CI (docker/build-push-action
# `build-args`). When omitted the binary shows "unknown" for branch/SHA.
ARG BORE_GIT_BRANCH=unknown
ARG BORE_GIT_SHA=unknown
ENV BORE_GIT_BRANCH=${BORE_GIT_BRANCH}
ENV BORE_GIT_SHA=${BORE_GIT_SHA}
COPY . .
RUN cargo install --path . --locked

FROM scratch
COPY --from=builder /usr/local/cargo/bin/bore .
USER 1000:1000
ENTRYPOINT ["./bore"]
