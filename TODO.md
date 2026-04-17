# Known limitations & deferred work

Public-facing follow-ups. See [CHANGELOG.md](CHANGELOG.md) for what has shipped.

## Pending upstream (magpie-bt)

- **Magnet links** — requires magpie M3 (BEP 9 metadata exchange). Rejected at the API layer today.
- **Multi-file torrents** — magpie M2.
- **DHT peer discovery** — not on the near-term roadmap; tracker-only for now.

## Dependency major bumps (not bundled with 2026.4.1)

- `reqwest 0.12 → 0.13`
- `toml 0.8 → 1.x`
- `axum 0.7 → 0.8`
- `password-hash 0.5 → 0.6`
- `redb 2 → 4` — touches the on-disk `state.redb` format; needs a migration story.

## Internal polish (not shipping yet)

- **`StatsWriter` actor**: registry access via `Arc<TorrentRegistry>` is correct but not contention-proof. Build the actor once profiling shows write-amplification.
- **Store unit tests**: `set_ratio_target` on missing record, concurrent read-modify-write race. Useful coverage gap; not blocking.
- **CSRF `Origin`/`Referer` allow-list**: intentionally not added — would break Sonarr/Radarr server-to-server flow. `SameSite=Strict` cookie remains the defence.
