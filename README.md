# lightorrent

[![CI](https://github.com/mlamp/lightorrent/actions/workflows/ci.yml/badge.svg)](https://github.com/mlamp/lightorrent/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A lightweight BitTorrent client with a qBittorrent-compatible API, designed as a drop-in backend for Sonarr, Radarr, and other *arr apps.

Built on [magpie-bt](https://crates.io/crates/magpie-bt) with persistent stats tracking, cookie-based auth, and a thin API surface that passes *arr detection probes.

## Status

Calendar-versioned (`YYYY.M.PATCH`). Current release tracks magpie-bt `0.1.x`. Known limitations:

- **No magnet links.** Rejected at the API layer until magpie implements BEP 9 (metadata exchange). Supply `.torrent` files or HTTP(S) URLs to `.torrent` files instead.
- **No DHT.** Tracker-only peer discovery.
- **Single-file torrents only** for now; multi-file lands with magpie M2.

See [CHANGELOG.md](CHANGELOG.md) for release notes and [TODO.md](TODO.md) for deferred work.

## Building

Requires Rust 1.87+.

```sh
cargo build --release
```

Binary is at `target/release/lightorrent`.

## Running

```sh
lightorrent --config config.toml
```

The `--config` flag is required. Copy [`config.example.toml`](config.example.toml) to `config.toml` and edit. All directories (`download_dir`, `persistence_dir`, parent of `state_db_path`) are created automatically on startup.

## Configuration

### config.toml

See [`config.example.toml`](config.example.toml) for a fully-commented example. Minimum:

```toml
download_dir = "/data/downloads"
```

Everything else has a sensible default (see the env-var table below).

### Environment variables

Every config field can be overridden via env var. Env vars take precedence over the config file.

| Variable                          | Config field          | Default                |
|-----------------------------------|-----------------------|------------------------|
| `LIGHTORRENT_DOWNLOAD_DIR`        | `download_dir`        | *(required)*           |
| `LIGHTORRENT_LISTEN_PORT`         | `listen_port`         | `8080`                 |
| `LIGHTORRENT_PERSISTENCE_DIR`     | `persistence_dir`     | `./data/session`       |
| `LIGHTORRENT_STATE_DB_PATH`       | `state_db_path`       | `./data/state.redb`    |
| `LIGHTORRENT_API_BIND_ADDRESS`    | `api_bind_address`    | `0.0.0.0`              |
| `LIGHTORRENT_API_PORT`            | `api_port`            | `8181`                 |
| `LIGHTORRENT_API_USERNAME`        | `api_username`        | `admin`                |
| `LIGHTORRENT_API_PASSWORD`        | `api_password`        | `adminadmin`           |
| `LIGHTORRENT_API_PASSWORD_HASH`   | `api_password` (PHC)  | *(unset; overrides plaintext if set)* |
| `RUST_LOG`                        | tracing filter        | `info`                 |
| `LIGHTORRENT_LOG_DIR`             | rotating file logs    | *(unset; stdout only)* |
| `LIGHTORRENT_LOG_JSON`            | JSON log format       | *(unset; text format)* |
| `LIGHTORRENT_TRUSTED_PROXIES`     | CSV of trusted peers  | *(empty — `X-Forwarded-For` ignored)* |

### Password handling

`api_password` accepts either plaintext or a PHC-encoded Argon2id hash (`$argon2id$…`). Plaintext is hashed in-memory on startup and a warning is logged — use it only for local dev. For production, generate a hash once with:

```sh
lightorrent hash-password 'your-plaintext-password'
```

and either paste the `$argon2id$…` string into `config.toml` as `api_password` or inject it via `LIGHTORRENT_API_PASSWORD_HASH` at runtime.

### Logging

Uses `tracing` with the standard `RUST_LOG` env filter:

```sh
RUST_LOG=debug lightorrent --config config.toml
RUST_LOG=lightorrent=debug,magpie_bt=warn lightorrent --config config.toml
```

Setting `LIGHTORRENT_LOG_DIR=/var/log/lightorrent` enables daily-rotated file logs (in addition to stdout). `LIGHTORRENT_LOG_JSON=1` switches the formatter to JSON.

## Docker

### Build

```sh
docker build -t lightorrent .
```

### docker-compose

```yaml
services:
  lightorrent:
    build: .
    ports:
      - "8080:8080"   # torrent protocol
      - "8181:8181"   # API
    volumes:
      - ./config.toml:/config/config.toml:ro
      - downloads:/downloads
      - state:/state
    environment:
      - LIGHTORRENT_DOWNLOAD_DIR=/downloads
      - LIGHTORRENT_PERSISTENCE_DIR=/state/session
      - LIGHTORRENT_STATE_DB_PATH=/state/state.redb
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-sf", "http://localhost:8181/api/v2/app/buildInfo"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s

volumes:
  downloads:
  state:
```

The published Dockerfile creates a non-root `lightorrent:10001` user. Recommended runtime flags for extra hardening: `--read-only`, `--cap-drop=ALL`, `--security-opt no-new-privileges`.

### Ports

| Port   | Protocol | Purpose                              |
|--------|----------|--------------------------------------|
| `8080` | TCP+UDP  | BitTorrent peer connections          |
| `8181` | TCP      | HTTP API (qBittorrent-compatible)    |

Both must be reachable from the internet for seeding. Forward them through your router/firewall.

## Connecting Sonarr / Radarr

1. In Sonarr/Radarr, go to **Settings > Download Clients > Add > qBittorrent**.
2. Set:
   - **Host**: your lightorrent IP/hostname
   - **Port**: `8181`
   - **Username**: value of `api_username`
   - **Password**: value of `api_password`
3. Click **Test** — should pass all detection probes.
4. Save.

## Data layout

```
/data/
  downloads/          # downloaded torrent content
  session/
    session.json      # magpie-bt session manifest
    *.bitv            # per-torrent piece bitfields (fast resume)
  state.redb          # torrent metadata, cumulative upload/download stats
```

- **session/** is managed by magpie-bt — piece verification state for fast resume after restart.
- **state.redb** is lightorrent's own store — torrent records, categories, cumulative stats that survive restarts.
- Both are needed for full state recovery. Back up the entire `/data/` directory.

## API

Implements the qBittorrent WebUI API subset that *arr apps probe:

- `POST /api/v2/auth/login` — cookie-based auth
- `GET  /api/v2/app/webapiVersion`
- `GET  /api/v2/app/version`
- `GET  /api/v2/app/preferences`
- `GET  /api/v2/app/buildInfo`
- `GET  /api/v2/torrents/info`
- `GET  /api/v2/torrents/properties`
- `GET  /api/v2/torrents/files`
- `POST /api/v2/torrents/add` — HTTP(S) URL to a `.torrent` or multipart `.torrent` file upload (magnet links are rejected)
- `POST /api/v2/torrents/delete`
- `POST /api/v2/torrents/pause`
- `POST /api/v2/torrents/resume`
- `POST /api/v2/torrents/setCategory`
- `POST /api/v2/torrents/createCategory`
- `GET  /api/v2/torrents/categories`
- `POST /api/v2/torrents/setShareLimits`
- `GET  /api/v2/transfer/info`

## License

Dual-licensed under either:

- MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
