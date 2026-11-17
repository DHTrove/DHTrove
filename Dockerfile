# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /source
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p dhtrove-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 dhtrove \
    && useradd --uid 10001 --gid dhtrove --no-create-home --shell /usr/sbin/nologin dhtrove \
    && mkdir /data && chown dhtrove:dhtrove /data
COPY --from=builder /source/target/release/dhtrove /usr/local/bin/dhtrove
USER dhtrove
ENV DHTROVE_DATA_DIR=/data \
    DHTROVE_BIND=0.0.0.0:8080 \
    RUST_LOG=info
EXPOSE 8080/tcp 6881/udp
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s \
    CMD curl -fsS http://127.0.0.1:8080/healthz >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/dhtrove"]
