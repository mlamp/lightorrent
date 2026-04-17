# Changelog

All notable changes to this project are documented here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is calendar-based (`YYYY.M.PATCH`).

## [Unreleased]

## [2026.4.1] — 2026-04-18

Initial public release.

### Added

- qBittorrent-compatible WebUI API subset sufficient to pass Sonarr/Radarr detection probes (`/auth/login`, `/app/*`, `/torrents/*`, `/transfer/info`).
- magpie-bt-backed download engine with event-driven alert consumer and batch-lock piece tracking.
- Fast resume via per-torrent piece bitfield persistence (`.bitv` files) — rTorrent-inspired; partial torrents resume from where they left off instead of re-verifying from scratch.
- Persistent `state.redb` store (redb-backed) for torrent records, categories, and cumulative upload/download stats across restarts.
- Cookie-based auth with Argon2id password hashing; plaintext `api_password` in config is accepted but hashed in-memory on startup and a warning is logged. `LIGHTORRENT_API_PASSWORD_HASH` env var supplies a PHC-encoded hash directly.
- `lightorrent hash-password <plaintext>` CLI subcommand for generating PHC hashes offline.
- Input hardening on `/torrents/add`: SSRF allow-list (http/https only, RFC-1918 / loopback / link-local / metadata IPs rejected), magnet-link rejection, info-hash validation (40-hex), category `savePath` traversal checks.
- Rate limiting on `/auth/login` with LRU eviction at 4096 entries.
- `X-Forwarded-For` honoured only when the peer is in `LIGHTORRENT_TRUSTED_PROXIES`.
- Structured / rotating logs via `tracing-appender` (`LIGHTORRENT_LOG_DIR`) and JSON output (`LIGHTORRENT_LOG_JSON=1`).
- Dockerfile with non-root `lightorrent:10001` user and documented hardening flags.

### Changed

- Engine replaced: librqbit v8 → magpie-bt 0.1.1. Binary size 13 MB → 11 MB, RSS 35 MB → 28 MB.
- `tokio` feature flags narrowed from `["full"]` to the minimum actually used.
- Crate-scoped `[lints.rust] warnings = "deny"`.

### Known follow-ups

- Dependency major bumps deferred for this release, planned next:
  `reqwest 0.12 → 0.13`, `toml 0.8 → 1.x`, `axum 0.7 → 0.8`,
  `password-hash 0.5 → 0.6`, and `redb 2 → 4` (on-disk format change —
  needs a migration path).
- **Magnet links** (BEP 9 metadata exchange) — pending magpie M3.
- **Multi-file torrents** — pending magpie M2.
- **DHT** — not planned near-term; tracker-only peer discovery.

[Unreleased]: https://github.com/mlamp/lightorrent/compare/v2026.4.1...HEAD
[2026.4.1]: https://github.com/mlamp/lightorrent/releases/tag/v2026.4.1
