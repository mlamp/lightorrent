use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use magpie_bt::{
    AddTorrentRequest, Alert, AlertCategory, AlertQueue, AttachTrackerConfig, FileStorage,
    HttpTracker, PeerIdBuilder, TorrentId, TorrentParams, TorrentStateView,
};
use magpie_bt_metainfo::FileListV1;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::Config;
use crate::store::{TorrentRecord, TorrentRegistry};

// ── Hex utilities ──

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> anyhow::Result<[u8; 20]> {
    if s.len() != 40 {
        anyhow::bail!("invalid info hash length: {}", s.len());
    }
    let mut out = [0u8; 20];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(b: u8) -> anyhow::Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => anyhow::bail!("invalid hex char: {}", b as char),
    }
}

// ── Name sanitization (H5) ──

fn sanitize_name(name: &str) -> String {
    let s: String = name.replace(['/', '\\', '\0'], "_").replace("..", "_");
    let s = s.trim_start_matches('.');
    if s.is_empty() {
        "unnamed".to_string()
    } else {
        s.to_string()
    }
}

// ── Bitfield utilities (rTorrent-inspired fast resume) ──

fn new_bitfield(piece_count: u32) -> Vec<u8> {
    vec![0u8; (piece_count as usize).div_ceil(8)]
}

fn set_bit(bitfield: &mut [u8], piece: u32) {
    let idx = piece as usize / 8;
    if idx < bitfield.len() {
        bitfield[idx] |= 0x80 >> (piece % 8); // MSB-first (BT convention)
    }
}

fn get_bit(bitfield: &[u8], piece: u32) -> bool {
    let idx = piece as usize / 8;
    idx < bitfield.len() && (bitfield[idx] & (0x80 >> (piece % 8))) != 0
}

fn bitfield_to_initial_have(bitfield: &[u8], piece_count: u32) -> Vec<bool> {
    (0..piece_count).map(|i| get_bit(bitfield, i)).collect()
}

fn popcount(bitfield: &[u8], piece_count: u32) -> u32 {
    (0..piece_count).filter(|&i| get_bit(bitfield, i)).count() as u32
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Engine state ──

struct EngineState {
    by_hash: HashMap<[u8; 20], LiveTorrent>,
    id_to_hash: HashMap<TorrentId, [u8; 20]>,
}

struct LiveTorrent {
    torrent_id: TorrentId,
    info_hash: [u8; 20],
    name: String,
    save_path: String,
    total_length: u64,
    piece_count: u32,
    piece_length: u64,
    pieces_have: u32,
    piece_bitfield: Vec<u8>,
    uploaded: u64,
    downloaded_wire: u64,
    dl_speed: f64,
    up_speed: f64,
    peer_count: usize,
    completed: bool,
    paused: bool,
    has_error: bool,
    persisted_uploaded: u64,
    persisted_downloaded: u64,
    prev_downloaded: u64,
    prev_uploaded: u64,
    prev_stats_time: Instant,
}

/// Read-only snapshot of a torrent, computed on demand from LiveTorrent.
pub struct TorrentSnapshot {
    pub info_hash: [u8; 20],
    pub hash: String,
    pub name: String,
    pub total_bytes: u64,
    pub piece_length: u64,
    pub progress: f64,
    pub progress_bytes: u64,
    pub uploaded: u64,
    pub downloaded: u64,
    pub dl_speed: f64,
    pub up_speed: f64,
    pub peer_count: usize,
    pub completed: bool,
    pub paused: bool,
    pub has_error: bool,
}

impl LiveTorrent {
    fn to_snapshot(&self) -> TorrentSnapshot {
        let progress = if self.piece_count > 0 {
            self.pieces_have as f64 / self.piece_count as f64
        } else {
            0.0
        };
        let progress_bytes = ((self.pieces_have as u64) * self.piece_length).min(self.total_length);
        TorrentSnapshot {
            info_hash: self.info_hash,
            hash: hex_encode(&self.info_hash),
            name: self.name.clone(),
            total_bytes: self.total_length,
            piece_length: self.piece_length,
            progress,
            progress_bytes,
            uploaded: self.persisted_uploaded + self.uploaded,
            downloaded: self.persisted_downloaded + self.downloaded_wire,
            dl_speed: self.dl_speed,
            up_speed: self.up_speed,
            peer_count: self.peer_count,
            completed: self.completed,
            paused: self.paused,
            has_error: self.has_error,
        }
    }
}

// ── Engine ──

pub struct Engine {
    magpie: Arc<magpie_bt::Engine>,
    alerts: Arc<AlertQueue>,
    registry: Arc<TorrentRegistry>,
    state: Arc<RwLock<EngineState>>,
    http: reqwest::Client,
    cancel: CancellationToken,
    persistence_dir: String,
    listen_port: u16,
    peer_id: [u8; 20],
    alert_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let download_dir = PathBuf::from(&config.download_dir);
        let persistence_dir = PathBuf::from(&config.persistence_dir);

        std::fs::create_dir_all(&download_dir)
            .with_context(|| format!("creating download dir {:?}", download_dir))?;
        std::fs::create_dir_all(&persistence_dir)
            .with_context(|| format!("creating persistence dir {:?}", persistence_dir))?;
        if let Some(parent) = std::path::Path::new(&config.state_db_path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state db dir {:?}", parent))?;
        }

        let registry = Arc::new(TorrentRegistry::open(&config.state_db_path)?);

        let alerts = Arc::new(AlertQueue::with_mask(4096, AlertCategory::ALL));
        let magpie = Arc::new(magpie_bt::Engine::new(Arc::clone(&alerts)));

        let peer_id = PeerIdBuilder::magpie(*b"0001").build();

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none()) // H2: no redirects (SSRF defense)
            .build()
            .context("building HTTP client")?;

        // Start inbound listener
        let listen_addr: SocketAddr = format!("0.0.0.0:{}", config.listen_port)
            .parse()
            .context("parsing listen address")?;
        let bound = magpie
            .listen(listen_addr, magpie_bt::ListenConfig::default())
            .await
            .context("starting inbound listener")?;
        info!(port = bound.port(), "listening for incoming peers");

        let state = Arc::new(RwLock::new(EngineState {
            by_hash: HashMap::new(),
            id_to_hash: HashMap::new(),
        }));

        let cancel = CancellationToken::new();

        let engine = Self {
            magpie,
            alerts,
            registry,
            state,
            http,
            cancel,
            persistence_dir: config.persistence_dir.clone(),
            listen_port: config.listen_port,
            peer_id,
            alert_task: tokio::sync::Mutex::new(None),
        };

        // Re-add torrents from registry
        engine.startup_readd().await;

        // Spawn alert consumer (after re-add so we don't process stale alerts)
        let task = tokio::spawn(alert_consumer(
            Arc::clone(&engine.alerts),
            Arc::clone(&engine.magpie),
            Arc::clone(&engine.state),
            Arc::clone(&engine.registry),
            engine.cancel.clone(),
        ));
        *engine.alert_task.lock().await = Some(task);

        info!("magpie engine started");
        Ok(engine)
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn registry(&self) -> &Arc<TorrentRegistry> {
        &self.registry
    }

    // ── Add torrent ──

    pub async fn add_torrent_bytes(
        &self,
        bytes: Vec<u8>,
        save_path: &str,
        category: &str,
        paused: bool,
    ) -> anyhow::Result<String> {
        let meta = magpie_bt::parse(&bytes).context("parsing .torrent file")?;
        let info_hash = *meta
            .info_hash
            .v1()
            .ok_or_else(|| anyhow::anyhow!("only v1 torrents supported"))?;
        let hash_hex = hex_encode(&info_hash);

        // H1: reject duplicate adds
        {
            let st = self.state.read().await;
            if st.by_hash.contains_key(&info_hash) {
                info!(%hash_hex, "torrent already managed");
                return Ok(hash_hex);
            }
        }

        let v1 = meta
            .info
            .v1
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("only v1 torrents supported"))?;

        let total_length = match &v1.files {
            FileListV1::Single { length } => *length,
            FileListV1::Multi { .. } => {
                anyhow::bail!("multi-file torrents not yet supported");
            }
        };

        let name_raw = String::from_utf8_lossy(meta.info.name).into_owned();
        let name = sanitize_name(&name_raw);
        let piece_hashes = v1.pieces.to_vec();
        let piece_count = (v1.pieces.len() / 20) as u32;
        let piece_length = meta.info.piece_length;
        let private = meta.info.private;

        // Save .torrent file for re-add on restart
        let torrent_path = self.torrent_path(&info_hash);
        std::fs::write(&torrent_path, &bytes)
            .with_context(|| format!("saving .torrent to {:?}", torrent_path))?;

        // Create storage
        let file_path = PathBuf::from(save_path).join(&name);
        let storage = Arc::new(
            FileStorage::create(&file_path, total_length)
                .with_context(|| format!("creating storage at {:?}", file_path))?,
        );

        // Build params and request
        let params = TorrentParams {
            piece_count,
            piece_length,
            total_length,
            piece_hashes,
            private,
        };
        let req = AddTorrentRequest::new(info_hash, params, storage, self.peer_id);
        let torrent_id = self
            .magpie
            .add_torrent(req)
            .await
            .context("adding torrent to magpie")?;
        info!(%hash_hex, "torrent added");

        // Attach trackers
        self.attach_trackers(&meta, torrent_id).await;

        // Load persisted stats (or 0 if new)
        let existing = self.registry.get(&info_hash).ok().flatten();
        let persisted_up = existing.as_ref().map(|r| r.total_uploaded).unwrap_or(0);
        let persisted_dl = existing.as_ref().map(|r| r.total_downloaded).unwrap_or(0);

        // Insert into state map (single write lock)
        {
            let mut st = self.state.write().await;
            st.by_hash.insert(
                info_hash,
                LiveTorrent {
                    torrent_id,
                    info_hash,
                    name: name.clone(),
                    save_path: save_path.to_string(),
                    total_length,
                    piece_count,
                    piece_length,
                    pieces_have: 0,
                    piece_bitfield: new_bitfield(piece_count),
                    uploaded: 0,
                    downloaded_wire: 0,
                    dl_speed: 0.0,
                    up_speed: 0.0,
                    peer_count: 0,
                    completed: false,
                    paused,
                    has_error: false,
                    persisted_uploaded: persisted_up,
                    persisted_downloaded: persisted_dl,
                    prev_downloaded: 0,
                    prev_uploaded: 0,
                    prev_stats_time: Instant::now(),
                },
            );
            st.id_to_hash.insert(torrent_id, info_hash);
        }

        // Upsert registry
        let record = TorrentRecord {
            info_hash_hex: hash_hex.clone(),
            source: torrent_path.to_string_lossy().into_owned(),
            save_path: save_path.to_string(),
            ratio_target: 0.0,
            added_at: now_epoch(),
            user_paused: paused,
            completed_at: None,
            category: category.to_string(),
            total_uploaded: persisted_up,
            total_downloaded: persisted_dl,
            piece_bitfield: Vec::new(),
            file_mtime: None,
            file_size: None,
        };
        if let Err(e) = self.registry.upsert(&info_hash, &record) {
            warn!(error = %e, "failed to upsert registry");
        }

        // Pause if requested
        if paused {
            let _ = self.magpie.pause(torrent_id).await;
        }

        Ok(hash_hex)
    }

    pub async fn add_torrent_url(
        &self,
        url: &str,
        save_path: &str,
        category: &str,
        paused: bool,
    ) -> anyhow::Result<String> {
        // validate_add_url() in api.rs is the first-line defense for literal IPs.
        // reqwest fetch may still resolve DNS to a private IP (DNS rebinding).
        // For trusted callers (Sonarr/Radarr), this is an acceptable residual risk.
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .context("fetching .torrent URL")?;

        if !resp.status().is_success() {
            anyhow::bail!("HTTP {} fetching {}", resp.status(), url);
        }

        // H3: reject oversized responses (32 MiB cap, matching multipart field limit)
        const MAX_TORRENT_BYTES: u64 = 32 * 1024 * 1024;
        if let Some(len) = resp.content_length() {
            if len > MAX_TORRENT_BYTES {
                anyhow::bail!("response too large: {} bytes", len);
            }
        }
        let bytes = resp.bytes().await.context("reading .torrent body")?;
        if bytes.len() as u64 > MAX_TORRENT_BYTES {
            anyhow::bail!("response too large: {} bytes", bytes.len());
        }
        let bytes = bytes.to_vec();
        self.add_torrent_bytes(bytes, save_path, category, paused)
            .await
    }

    // ── Pause / Resume / Delete ──

    pub async fn pause_torrent(&self, hash: &str) -> anyhow::Result<()> {
        let hash_bytes = hex_decode(hash)?;
        let torrent_id = {
            let mut st = self.state.write().await;
            let lt = st
                .by_hash
                .get_mut(&hash_bytes)
                .ok_or_else(|| anyhow::anyhow!("torrent not found: {hash}"))?;
            lt.paused = true;
            lt.torrent_id
        };
        self.magpie.pause(torrent_id).await?;
        self.registry.set_user_paused(&hash_bytes, true)?;
        info!(%hash, "torrent paused");
        Ok(())
    }

    pub async fn resume_torrent(&self, hash: &str) -> anyhow::Result<()> {
        let hash_bytes = hex_decode(hash)?;
        let torrent_id = {
            let mut st = self.state.write().await;
            let lt = st
                .by_hash
                .get_mut(&hash_bytes)
                .ok_or_else(|| anyhow::anyhow!("torrent not found: {hash}"))?;
            lt.paused = false;
            lt.has_error = false;
            lt.torrent_id
        };
        self.magpie.resume(torrent_id).await?;
        self.registry.set_user_paused(&hash_bytes, false)?;
        info!(%hash, "torrent resumed");
        Ok(())
    }

    pub async fn delete_torrent(&self, hash: &str, delete_files: bool) -> anyhow::Result<()> {
        let hash_bytes = hex_decode(hash)?;
        let torrent_id = {
            let mut st = self.state.write().await;
            let lt = st
                .by_hash
                .remove(&hash_bytes)
                .ok_or_else(|| anyhow::anyhow!("torrent not found: {hash}"))?;
            st.id_to_hash.remove(&lt.torrent_id);
            lt.torrent_id
        };
        self.magpie.remove(torrent_id, delete_files).await?;
        if let Err(e) = self.registry.remove(&hash_bytes) {
            warn!(error = %e, %hash, "failed to remove from registry");
        }
        let _ = std::fs::remove_file(self.torrent_path(&hash_bytes));
        info!(%hash, delete_files, "torrent deleted");
        Ok(())
    }

    // ── Metadata setters ──

    pub fn set_ratio_target(&self, hash: &str, ratio: f64) -> anyhow::Result<()> {
        let hash_bytes = hex_decode(hash)?;
        self.registry.set_ratio_target(&hash_bytes, ratio)?;
        info!(%hash, ratio, "ratio target set");
        Ok(())
    }

    pub fn set_category(&self, hash: &str, category: &str) -> anyhow::Result<()> {
        let hash_bytes = hex_decode(hash)?;
        self.registry.set_category(&hash_bytes, category)?;
        info!(%hash, category, "category set");
        Ok(())
    }

    // ── Snapshots for API ──

    pub async fn snapshot_all(&self) -> Vec<TorrentSnapshot> {
        let st = self.state.read().await;
        st.by_hash.values().map(|lt| lt.to_snapshot()).collect()
    }

    pub async fn snapshot(&self, hash: &str) -> Option<TorrentSnapshot> {
        let hash_bytes = hex_decode(hash).ok()?;
        let st = self.state.read().await;
        st.by_hash.get(&hash_bytes).map(|lt| lt.to_snapshot())
    }

    pub async fn torrent_hashes(&self) -> Vec<String> {
        let st = self.state.read().await;
        st.by_hash.keys().map(|h| hex_encode(h)).collect()
    }

    pub async fn get_files(&self, hash: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        let hash_bytes = hex_decode(hash)?;
        let st = self.state.read().await;
        let lt = st
            .by_hash
            .get(&hash_bytes)
            .ok_or_else(|| anyhow::anyhow!("torrent not found: {hash}"))?;
        let progress = if lt.piece_count > 0 {
            lt.pieces_have as f64 / lt.piece_count as f64
        } else {
            0.0
        };
        Ok(vec![serde_json::json!({
            "name": lt.name,
            "size": lt.total_length,
            "progress": progress,
            "priority": 1,
        })])
    }

    // ── Shutdown (M3) ──

    pub async fn shutdown(&self) {
        info!("shutting down engine");

        // 1. Cancel alert consumer
        self.cancel.cancel();
        if let Some(task) = self.alert_task.lock().await.take() {
            match task.await {
                Ok(()) => {}
                Err(e) if e.is_cancelled() => {}
                Err(e) => warn!(error = %e, "alert task join error"),
            }
        }

        // 2. Final drain
        let batch = self.alerts.drain();
        if !batch.is_empty() {
            let mut st = self.state.write().await;
            apply_alerts(&batch, &mut st);
        }

        // 3. Persist final stats
        self.persist_all_stats().await;

        // 4. Shut down magpie
        for id in self.magpie.torrents().await {
            self.magpie.shutdown(id).await;
        }
        self.magpie.join().await;
        info!("engine stopped");
    }

    // ── Internal helpers ──

    fn torrent_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        PathBuf::from(&self.persistence_dir).join(format!("{}.torrent", hex_encode(info_hash)))
    }

    async fn attach_trackers(&self, meta: &magpie_bt::MetaInfo<'_>, torrent_id: TorrentId) {
        let cfg = AttachTrackerConfig {
            listen_port: self.listen_port,
            ..Default::default()
        };

        // Collect tracker URLs from announce and announce_list
        let mut urls: Vec<String> = Vec::new();
        if let Some(announce) = meta.announce {
            if let Ok(url) = std::str::from_utf8(announce) {
                urls.push(url.to_string());
            }
        }
        if let Some(tiers) = &meta.announce_list {
            for tier in tiers {
                for entry in tier {
                    if let Ok(url) = std::str::from_utf8(entry) {
                        if !urls.contains(&url.to_string()) {
                            urls.push(url.to_string());
                        }
                    }
                }
            }
        }

        for url in &urls {
            match HttpTracker::new(url.as_str()) {
                Ok(tracker) => {
                    if let Err(e) = self
                        .magpie
                        .attach_tracker(torrent_id, Arc::new(tracker), cfg)
                        .await
                    {
                        warn!(url, error = %e, "failed to attach tracker");
                    }
                }
                Err(e) => {
                    warn!(url, error = %e, "invalid tracker URL");
                }
            }
        }
    }

    async fn startup_readd(&self) {
        let records = match self.registry.list() {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to list registry for startup re-add");
                return;
            }
        };

        for (info_hash, record) in &records {
            let bytes = resolve_torrent_source(&self.persistence_dir, record);
            let Some(bytes) = bytes else {
                warn!(hash = %record.info_hash_hex, "skipping re-add: .torrent file not found");
                continue;
            };

            let meta = match magpie_bt::parse(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    warn!(hash = %record.info_hash_hex, error = %e, "skipping re-add: parse failed");
                    continue;
                }
            };

            // H5: verify parsed info_hash matches registry key
            let parsed_hash = match meta.info_hash.v1() {
                Some(h) => *h,
                None => {
                    warn!(hash = %record.info_hash_hex, "skipping re-add: not a v1 torrent");
                    continue;
                }
            };
            if parsed_hash != *info_hash {
                warn!(
                    expected = %record.info_hash_hex,
                    actual = %hex_encode(&parsed_hash),
                    "skipping re-add: .torrent file hash mismatch (corrupt or wrong file)"
                );
                continue;
            }

            let v1 = match meta.info.v1.as_ref() {
                Some(v) => v,
                None => {
                    warn!(hash = %record.info_hash_hex, "skipping re-add: not a v1 torrent");
                    continue;
                }
            };

            let total_length = match &v1.files {
                FileListV1::Single { length } => *length,
                FileListV1::Multi { .. } => {
                    warn!(hash = %record.info_hash_hex, "skipping re-add: multi-file not supported");
                    continue;
                }
            };

            let name_raw = String::from_utf8_lossy(meta.info.name).into_owned();
            let name = sanitize_name(&name_raw);
            let piece_hashes = v1.pieces.to_vec();
            let piece_count = (v1.pieces.len() / 20) as u32;
            let piece_length = meta.info.piece_length;

            // Open existing file or create new
            let file_path = PathBuf::from(&record.save_path).join(&name);
            let storage: Arc<dyn magpie_bt::Storage> = if file_path.exists() {
                match FileStorage::open(&file_path) {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        warn!(hash = %record.info_hash_hex, error = %e, "failed to open storage, creating new");
                        match FileStorage::create(&file_path, total_length) {
                            Ok(s) => Arc::new(s),
                            Err(e) => {
                                warn!(hash = %record.info_hash_hex, error = %e, "failed to create storage");
                                continue;
                            }
                        }
                    }
                }
            } else {
                match FileStorage::create(&file_path, total_length) {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        warn!(hash = %record.info_hash_hex, error = %e, "failed to create storage");
                        continue;
                    }
                }
            };

            let params = TorrentParams {
                piece_count,
                piece_length,
                total_length,
                piece_hashes,
                private: meta.info.private,
            };

            let mut req = AddTorrentRequest::new(*info_hash, params, storage, self.peer_id);

            // Fast resume: rTorrent-inspired mtime+size validation
            let is_completed = record.completed_at.is_some();
            let has_bitfield = !record.piece_bitfield.is_empty()
                && record.piece_bitfield.len() == (piece_count as usize).div_ceil(8);

            let trust_bitfield = has_bitfield
                && match (record.file_mtime, record.file_size) {
                    (Some(stored_mtime), Some(stored_size)) => {
                        std::fs::metadata(&file_path).ok().is_some_and(|meta| {
                            let current_mtime = meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            current_mtime == stored_mtime && meta.len() == stored_size
                        })
                    }
                    _ => false,
                };

            let (initial_have, initial_pieces_have, initial_bitfield) = if is_completed {
                (vec![true; piece_count as usize], piece_count, {
                    let mut bf = new_bitfield(piece_count);
                    for i in 0..piece_count {
                        set_bit(&mut bf, i);
                    }
                    bf
                })
            } else if trust_bitfield {
                let count = popcount(&record.piece_bitfield, piece_count);
                info!(
                    hash = %record.info_hash_hex,
                    pieces = count,
                    total = piece_count,
                    "fast resume: {} of {} pieces",
                    count,
                    piece_count
                );
                (
                    bitfield_to_initial_have(&record.piece_bitfield, piece_count),
                    count,
                    record.piece_bitfield.clone(),
                )
            } else {
                if file_path.exists() && !is_completed {
                    warn!(hash = %record.info_hash_hex, "partial torrent will re-download (no trusted bitfield)");
                }
                (Vec::new(), 0, new_bitfield(piece_count))
            };

            req.initial_have = initial_have;

            match self.magpie.add_torrent(req).await {
                Ok(torrent_id) => {
                    info!(hash = %record.info_hash_hex, "torrent re-added");

                    self.attach_trackers(&meta, torrent_id).await;

                    let mut st = self.state.write().await;
                    st.by_hash.insert(
                        *info_hash,
                        LiveTorrent {
                            torrent_id,
                            info_hash: *info_hash,
                            name,
                            save_path: record.save_path.clone(),
                            total_length,
                            piece_count,
                            piece_length,
                            pieces_have: initial_pieces_have,
                            piece_bitfield: initial_bitfield,
                            uploaded: 0,
                            downloaded_wire: 0,
                            dl_speed: 0.0,
                            up_speed: 0.0,
                            peer_count: 0,
                            completed: is_completed,
                            paused: record.user_paused,
                            has_error: false,
                            persisted_uploaded: record.total_uploaded,
                            persisted_downloaded: record.total_downloaded,
                            prev_downloaded: 0,
                            prev_uploaded: 0,
                            prev_stats_time: Instant::now(),
                        },
                    );
                    st.id_to_hash.insert(torrent_id, *info_hash);

                    if record.user_paused {
                        let _ = self.magpie.pause(torrent_id).await;
                    }
                }
                Err(e) => {
                    warn!(hash = %record.info_hash_hex, error = %e, "failed to re-add torrent");
                }
            }
        }
    }

    async fn persist_all_stats(&self) {
        // Snapshot under read lock, persist outside (C2 fix)
        struct Snap {
            hash: [u8; 20],
            cum_up: u64,
            cum_dl: u64,
            bitfield: Vec<u8>,
            save_path: String,
            name: String,
        }
        let snaps: Vec<Snap> = {
            let st = self.state.read().await;
            st.by_hash
                .iter()
                .map(|(hash, lt)| Snap {
                    hash: *hash,
                    cum_up: lt.persisted_uploaded + lt.uploaded,
                    cum_dl: lt.persisted_downloaded + lt.downloaded_wire,
                    bitfield: lt.piece_bitfield.clone(),
                    save_path: lt.save_path.clone(),
                    name: lt.name.clone(),
                })
                .collect()
        };
        for s in &snaps {
            if let Ok(Some(mut rec)) = self.registry.get(&s.hash) {
                // Use max() to prevent regression if registry was modified externally
                rec.total_uploaded = rec.total_uploaded.max(s.cum_up);
                rec.total_downloaded = rec.total_downloaded.max(s.cum_dl);
                rec.piece_bitfield = s.bitfield.clone();
                let file_path = PathBuf::from(&s.save_path).join(&s.name);
                if let Ok(meta) = std::fs::metadata(&file_path) {
                    rec.file_mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    rec.file_size = Some(meta.len());
                }
                let _ = self.registry.upsert(&s.hash, &rec);
            }
        }
    }
}

// ── M4: source resolution for startup ──

fn resolve_torrent_source(persistence_dir: &str, record: &TorrentRecord) -> Option<Vec<u8>> {
    // New format: persistence_dir/{hash}.torrent
    let persisted = format!("{}/{}.torrent", persistence_dir, record.info_hash_hex);
    if let Ok(bytes) = std::fs::read(&persisted) {
        return Some(bytes);
    }
    // Old format fallback: record.source is a file path
    if !record.source.starts_with("http")
        && !record.source.starts_with("magnet")
        && !record.source.starts_with("<")
    {
        if let Ok(bytes) = std::fs::read(&record.source) {
            return Some(bytes);
        }
    }
    None
}

// ── Alert consumer ──

fn lookup_mut(state: &mut EngineState, id: TorrentId) -> Option<&mut LiveTorrent> {
    let hash = state.id_to_hash.get(&id)?;
    state.by_hash.get_mut(hash)
}

/// Apply non-StatsUpdate alerts to state. No async, no I/O — safe under write lock.
fn apply_alerts(batch: &[Alert], st: &mut EngineState) {
    for alert in batch {
        match alert {
            Alert::PieceCompleted { torrent, piece } => {
                if let Some(lt) = lookup_mut(st, *torrent) {
                    lt.pieces_have += 1;
                    set_bit(&mut lt.piece_bitfield, *piece);
                }
            }
            Alert::TorrentComplete { torrent } => {
                if let Some(lt) = lookup_mut(st, *torrent) {
                    lt.completed = true;
                }
            }
            Alert::PeerConnected { torrent, .. } => {
                if let Some(lt) = lookup_mut(st, *torrent) {
                    lt.peer_count += 1;
                }
            }
            Alert::PeerDisconnected { torrent, .. } => {
                if let Some(lt) = lookup_mut(st, *torrent) {
                    lt.peer_count = lt.peer_count.saturating_sub(1);
                }
            }
            Alert::Error { torrent, code } => {
                warn!(?torrent, ?code, "magpie error");
                if let Some(lt) = lookup_mut(st, *torrent) {
                    lt.has_error = true;
                }
            }
            _ => {}
        }
    }
}

async fn alert_consumer(
    alerts: Arc<AlertQueue>,
    magpie: Arc<magpie_bt::Engine>,
    state: Arc<RwLock<EngineState>>,
    registry: Arc<TorrentRegistry>,
    cancel: CancellationToken,
) {
    loop {
        // Wait for alerts or cancellation, with 3s timeout for speed staleness
        tokio::select! {
            _ = cancel.cancelled() => return,
            result = tokio::time::timeout(Duration::from_secs(3), alerts.wait()) => {
                if result.is_err() {
                    // Timeout: zero out speeds on stale torrents (#4)
                    let mut st = state.write().await;
                    for lt in st.by_hash.values_mut() {
                        if lt.prev_stats_time.elapsed() > Duration::from_secs(3) {
                            lt.dl_speed = 0.0;
                            lt.up_speed = 0.0;
                        }
                    }
                    continue;
                }
            }
        }

        let batch = alerts.drain();
        if batch.is_empty() {
            continue;
        }

        // ── Phase 1: fast alerts under write lock (no async) ──
        let mut needs_reconciliation = false;
        let mut completed_hashes: Vec<[u8; 20]> = Vec::new();
        {
            let mut st = state.write().await;
            apply_alerts(&batch, &mut st);

            // Check for completion and overflow
            for alert in &batch {
                match alert {
                    Alert::TorrentComplete { torrent } => {
                        if let Some(hash) = state_id_to_hash(&st, *torrent) {
                            completed_hashes.push(hash);
                        }
                    }
                    Alert::Dropped { count } => {
                        warn!(count, "alert overflow — scheduling reconciliation");
                        needs_reconciliation = true;
                    }
                    _ => {}
                }
            }
        } // write lock dropped

        // ── Phase 1b: persist completed_at (no state lock) ──
        for hash in &completed_hashes {
            if let Ok(Some(mut rec)) = registry.get(hash) {
                if rec.completed_at.is_none() {
                    rec.completed_at = Some(now_epoch());
                    let _ = registry.upsert(hash, &rec);
                    info!(hash = %rec.info_hash_hex, "marked completed");
                }
            }
        }

        // ── Phase 1c: reconciliation on alert overflow (#1) ──
        if needs_reconciliation {
            let ids: Vec<(TorrentId, [u8; 20])> = {
                let st = state.read().await;
                st.by_hash
                    .values()
                    .map(|lt| (lt.torrent_id, lt.info_hash))
                    .collect()
            };
            let mut reconciled: Vec<([u8; 20], TorrentStateView)> = Vec::new();
            for (tid, hash) in &ids {
                if let Some(view) = magpie.torrent_state(*tid).await {
                    reconciled.push((*hash, view));
                }
            }
            let mut st = state.write().await;
            for (hash, view) in &reconciled {
                if let Some(lt) = st.by_hash.get_mut(hash) {
                    lt.peer_count = view.peer_count;
                }
            }
        }

        // ── Phase 2: StatsUpdate — batch pattern (C3) ──
        let has_stats_update = batch.iter().any(|a| matches!(a, Alert::StatsUpdate));
        if !has_stats_update {
            continue;
        }

        // 2a. Collect IDs (read lock)
        let ids: Vec<(TorrentId, [u8; 20])> = {
            let st = state.read().await;
            st.by_hash
                .values()
                .map(|lt| (lt.torrent_id, lt.info_hash))
                .collect()
        };

        // 2b. Query stats (NO lock)
        let mut snapshots = Vec::new();
        for (tid, hash) in &ids {
            let snap = magpie.torrent_stats_snapshot(*tid).await;
            let view = magpie.torrent_state(*tid).await;
            if let Some(snap) = snap {
                snapshots.push((*hash, snap, view));
            }
        }

        // 2c-pre. Pre-fetch ratio targets from registry (blocking I/O outside lock, C1 fix)
        let ratio_targets: HashMap<[u8; 20], f64> = snapshots
            .iter()
            .filter_map(|(hash, _, _)| {
                registry
                    .get(hash)
                    .ok()
                    .flatten()
                    .filter(|rec| rec.ratio_target > 0.0)
                    .map(|rec| (*hash, rec.ratio_target))
            })
            .collect();

        // 2c. Apply (write lock, NO async, NO blocking I/O)
        struct DirtyRecord {
            hash: [u8; 20],
            cum_up: u64,
            cum_dl: u64,
            bitfield: Vec<u8>,
            save_path: String,
            name: String,
        }
        let mut to_pause: Vec<TorrentId> = Vec::new();
        let mut dirty: Vec<DirtyRecord> = Vec::new();
        {
            let mut st = state.write().await;
            for (hash, snap, view) in &snapshots {
                let Some(lt) = st.by_hash.get_mut(hash) else {
                    continue;
                };

                let elapsed = lt.prev_stats_time.elapsed().as_secs_f64();
                if elapsed > 0.1 {
                    lt.dl_speed =
                        snap.downloaded.saturating_sub(lt.prev_downloaded) as f64 / elapsed;
                    lt.up_speed = snap.uploaded.saturating_sub(lt.prev_uploaded) as f64 / elapsed;
                    lt.prev_downloaded = snap.downloaded;
                    lt.prev_uploaded = snap.uploaded;
                    lt.prev_stats_time = Instant::now();
                }
                lt.downloaded_wire = snap.downloaded;
                lt.uploaded = snap.uploaded;

                // H4: reconcile peer count
                if let Some(v) = view {
                    lt.peer_count = v.peer_count;
                }

                let cum_up = lt.persisted_uploaded + lt.uploaded;
                let cum_dl = lt.persisted_downloaded + lt.downloaded_wire;
                dirty.push(DirtyRecord {
                    hash: *hash,
                    cum_up,
                    cum_dl,
                    bitfield: lt.piece_bitfield.clone(),
                    save_path: lt.save_path.clone(),
                    name: lt.name.clone(),
                });

                // Ratio enforcement (uses pre-fetched targets, no I/O under lock)
                if !lt.paused && !lt.has_error && lt.total_length > 0 {
                    if let Some(&target) = ratio_targets.get(hash) {
                        let ratio = cum_up as f64 / lt.total_length as f64;
                        if ratio >= target {
                            info!(
                                hash = %hex_encode(hash),
                                ratio,
                                target,
                                "ratio reached, pausing"
                            );
                            to_pause.push(lt.torrent_id);
                            lt.paused = true;
                        }
                    }
                }
            }
        } // write lock dropped

        // 2d. Persist dirty stats + bitfield + file metadata (no lock)
        for dr in &dirty {
            if let Ok(Some(mut rec)) = registry.get(&dr.hash) {
                let changed = rec.total_uploaded != dr.cum_up
                    || rec.total_downloaded != dr.cum_dl
                    || rec.piece_bitfield != dr.bitfield;
                if changed {
                    rec.total_uploaded = dr.cum_up;
                    rec.total_downloaded = dr.cum_dl;
                    rec.piece_bitfield = dr.bitfield.clone();
                    // File metadata for rTorrent-style mtime validation on resume
                    let file_path = PathBuf::from(&dr.save_path).join(&dr.name);
                    if let Ok(meta) = std::fs::metadata(&file_path) {
                        rec.file_mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs());
                        rec.file_size = Some(meta.len());
                    }
                    let _ = registry.upsert(&dr.hash, &rec);
                }
            }
        }

        // 2e. Issue pause commands (no lock)
        for tid in to_pause {
            let _ = magpie.pause(tid).await;
        }

        // M6: yield to avoid starving API handlers on rapid alert floods
        tokio::task::yield_now().await;
    }
}

fn state_id_to_hash(st: &EngineState, id: TorrentId) -> Option<[u8; 20]> {
    st.id_to_hash.get(&id).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitfield_size() {
        assert_eq!(new_bitfield(0).len(), 0);
        assert_eq!(new_bitfield(1).len(), 1);
        assert_eq!(new_bitfield(7).len(), 1);
        assert_eq!(new_bitfield(8).len(), 1);
        assert_eq!(new_bitfield(9).len(), 2);
        assert_eq!(new_bitfield(16).len(), 2);
        assert_eq!(new_bitfield(17).len(), 3);
    }

    #[test]
    fn test_set_get_bit() {
        let mut bf = new_bitfield(16);
        assert!(!get_bit(&bf, 0));
        assert!(!get_bit(&bf, 7));
        assert!(!get_bit(&bf, 15));

        set_bit(&mut bf, 0);
        set_bit(&mut bf, 7);
        set_bit(&mut bf, 15);

        assert!(get_bit(&bf, 0));
        assert!(get_bit(&bf, 7));
        assert!(get_bit(&bf, 15));
        assert!(!get_bit(&bf, 1));
        assert!(!get_bit(&bf, 8));
        assert!(!get_bit(&bf, 14));

        // MSB-first: bit 0 is 0x80 in byte 0
        assert_eq!(bf[0], 0x81); // bits 0 and 7
        assert_eq!(bf[1], 0x01); // bit 15 (= bit 7 of byte 1)
    }

    #[test]
    fn test_popcount() {
        let mut bf = new_bitfield(32);
        assert_eq!(popcount(&bf, 32), 0);

        set_bit(&mut bf, 0);
        set_bit(&mut bf, 10);
        set_bit(&mut bf, 31);
        assert_eq!(popcount(&bf, 32), 3);

        // Full bitfield
        let mut bf = new_bitfield(8);
        for i in 0..8 {
            set_bit(&mut bf, i);
        }
        assert_eq!(popcount(&bf, 8), 8);
    }

    #[test]
    fn test_bitfield_to_initial_have() {
        let mut bf = new_bitfield(4);
        set_bit(&mut bf, 1);
        set_bit(&mut bf, 3);

        let have = bitfield_to_initial_have(&bf, 4);
        assert_eq!(have, vec![false, true, false, true]);
    }

    #[test]
    fn test_bitfield_roundtrip() {
        for count in [0, 1, 7, 8, 9, 15, 16, 100, 255] {
            let mut bf = new_bitfield(count);
            for i in 0..count {
                set_bit(&mut bf, i);
            }
            assert_eq!(
                popcount(&bf, count),
                count,
                "roundtrip failed for count={count}"
            );
            let have = bitfield_to_initial_have(&bf, count);
            assert!(
                have.iter().all(|&b| b),
                "all should be true for count={count}"
            );
        }
    }

    #[test]
    fn test_hex_encode_decode() {
        let bytes = [
            0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0x00, 0xff, 0x11, 0x22, 0x33, 0x44,
            0x55, 0x66, 0x77, 0x88, 0x99, 0xaa,
        ];
        let hex = hex_encode(&bytes);
        assert_eq!(
            hex,
            "abcdef0123456789_00ff112233445566778899aa".replace('_', "")
        );
        let decoded = hex_decode(&hex).unwrap();
        assert_eq!(decoded, bytes);
    }
}
