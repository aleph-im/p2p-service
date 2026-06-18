# Build stage: the full Rust toolchain, kept out of the final image.
FROM rust:1.88 AS builder
WORKDIR /usr/src/p2p-service
RUN apt-get update && apt-get install -y protobuf-compiler
COPY . .
RUN cargo install --path . --locked --root /usr/local

# Runtime stage: just the binary on a slim glibc base (same Debian
# generation as the builder, so the dynamic linkage matches). ca-certificates
# is needed for TLS when a Sentry DSN is configured.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/aleph-p2p-service /usr/local/bin/aleph-p2p-service
COPY ./scripts/docker/run-p2p-service.sh /opt/p2p-service/
ENTRYPOINT ["bash", "/opt/p2p-service/run-p2p-service.sh"]
