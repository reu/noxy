FROM rust:1-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools curl \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

ARG FEATURES=

WORKDIR /build
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl --features "cli,redis${FEATURES:+,${FEATURES}}"

FROM scratch

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/noxy /noxy

ENTRYPOINT ["/noxy"]
