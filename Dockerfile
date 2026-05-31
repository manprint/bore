FROM rust:alpine AS builder
WORKDIR /home/rust/src
# build-base provides the C toolchain needed to compile `ring` (TLS) on musl.
RUN apk --no-cache add musl-dev build-base
COPY . .
RUN cargo install --path .

FROM scratch
COPY --from=builder /usr/local/cargo/bin/bore .
USER 1000:1000
ENTRYPOINT ["./bore"]
