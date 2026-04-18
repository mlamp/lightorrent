FROM rust:1.95-trixie AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
# build.rs needs git info
COPY .git/ .git/

RUN cargo build --release

FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user with a fixed UID so volume permissions are predictable.
RUN groupadd --system --gid 10001 lightorrent \
    && useradd  --system --uid 10001 --gid 10001 --home-dir /var/lib/lightorrent \
                --shell /usr/sbin/nologin lightorrent \
    && mkdir -p /var/lib/lightorrent /config \
    && chown -R lightorrent:lightorrent /var/lib/lightorrent /config

COPY --from=builder /build/target/release/lightorrent /usr/local/bin/lightorrent

LABEL org.opencontainers.image.title="lightorrent" \
      org.opencontainers.image.description="Lightweight torrent daemon with qBittorrent-compatible API" \
      org.opencontainers.image.source="https://github.com/mlamp/lightorrent"

USER lightorrent:lightorrent
WORKDIR /var/lib/lightorrent

EXPOSE 8080 8181

ENTRYPOINT ["lightorrent"]
CMD ["--config", "/config/config.toml"]

# Recommended runtime hardening (deploy-side, not enforceable here):
#   docker run --read-only --tmpfs /tmp \
#              --cap-drop=ALL --security-opt no-new-privileges \
#              -v lightorrent-data:/var/lib/lightorrent \
#              -v ./config.toml:/config/config.toml:ro \
#              ...
