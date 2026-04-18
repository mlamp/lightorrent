use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Multipart, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::Config;
use crate::engine::{Engine, TorrentSnapshot};

/// Idle session lifetime. Any SID not used within this window is reaped.
const SESSION_IDLE_TTL: Duration = Duration::from_secs(60 * 60 * 24);
/// Per-IP login attempt limits (sliding window).
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_MAX_FAILURES: u32 = 10;
/// Per-multipart-field byte cap for `/torrents/add` uploads.
const MULTIPART_FIELD_MAX_BYTES: usize = 32 * 1024 * 1024;
/// Upper bound on rate-limiter map size before LRU-style eviction kicks in.
const LOGIN_ATTEMPTS_MAX: usize = 4096;
/// Max ETA clamp (100 days, matches qBittorrent convention).
const ETA_MAX: i64 = 8_640_000;
/// EMA smoothing factor for ETA (α ≈ 0.2).
const ETA_EMA_ALPHA: f64 = 0.2;

/// True if `s` is exactly 40 lowercase hexadecimal characters (BitTorrent v1
/// SHA-1 info hash). BEP 52 (v2 SHA-256) is not yet supported upstream.
fn is_valid_infohash_v1(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Clamp an f64 (possibly NaN/negative/over-large) into the non-negative i64 range.
fn f64_to_i64_clamped(v: f64) -> i64 {
    if !v.is_finite() || v <= 0.0 {
        0
    } else if v >= i64::MAX as f64 {
        i64::MAX
    } else {
        v as i64
    }
}

/// Compute ETA seconds from remaining bytes and download speed. Returns
/// [0, ETA_MAX]. `dl_speed` is bytes/sec; we clamp the divisor to 1 to avoid
/// div-by-zero and implicitly treat near-zero speed as "no ETA".
fn raw_eta(remaining_bytes: u64, dl_speed: f64) -> i64 {
    if remaining_bytes == 0 {
        return 0;
    }
    let spd = dl_speed.max(1.0);
    let eta = (remaining_bytes as f64) / spd;
    let clamped = eta.min(ETA_MAX as f64).max(0.0);
    clamped as i64
}

/// True iff `s` is an acceptable torrent-add URL. Accepts `magnet:?xt=urn:btih:…`
/// (case-insensitive scheme) and http(s) URLs whose host is NOT loopback /
/// link-local / RFC-1918 private / cloud-metadata (169.254.169.254). DNS
/// resolution is left to the HTTP client; we only guard against literal-IP abuse and
/// obviously-wrong hostnames to keep this sync.
fn validate_add_url(url: &str) -> Result<(), &'static str> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("magnet:") {
        return Err("magnet links not yet supported");
    }
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("only http, https, and magnet URLs are accepted");
    }
    if url.len() > 4096 {
        return Err("URL too long");
    }
    // Extract host portion: after ://, up to first /, ?, #, or :
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let host = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host_no_port = match host.rsplit_once(':') {
        Some((h, _)) if !h.starts_with('[') => h,
        _ => host,
    };
    if host_no_port.is_empty() {
        return Err("URL missing host");
    }
    // If the host is an IP literal, reject forbidden ranges outright.
    if let Ok(ip) = host_no_port.parse::<IpAddr>() {
        if is_forbidden_ip(&ip) {
            return Err("URL host resolves to a forbidden IP range");
        }
    } else {
        // Reject well-known metadata / loopback hostnames (cheap best-effort).
        let h = host_no_port.to_ascii_lowercase();
        if h == "localhost"
            || h.ends_with(".localhost")
            || h == "metadata.google.internal"
            || h == "metadata"
        {
            return Err("URL host is forbidden");
        }
    }
    Ok(())
}

fn is_forbidden_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_documentation()
                // Cloud metadata (AWS/GCP/Azure — all use 169.254.169.254, covered by link-local)
                || *v4 == Ipv4Addr::new(169, 254, 169, 254)
                // Carrier-grade NAT
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fe80::/10 link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // fc00::/7 ULA
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Validate a category `savePath`: must not contain `..` components, and if
/// absolute must be a descendant of `download_dir`. Empty path is accepted
/// (means "use default").
fn validate_save_path(save_path: &str, download_dir: &str) -> bool {
    if save_path.is_empty() {
        return true;
    }
    let p = Path::new(save_path);
    for comp in p.components() {
        match comp {
            Component::ParentDir => return false,
            Component::Normal(_)
            | Component::CurDir
            | Component::RootDir
            | Component::Prefix(_) => {}
        }
    }
    if p.is_absolute() {
        let dl = Path::new(download_dir);
        // Require prefix match on the logical path (no filesystem canonicalize —
        // the dir may not yet exist, and canonicalize can follow symlinks).
        if dl.as_os_str().is_empty() {
            return false;
        }
        let mut it_dl = dl.components();
        let mut it_p = p.components();
        loop {
            match (it_dl.next(), it_p.next()) {
                (None, _) => return true,
                (Some(a), Some(b)) if a == b => continue,
                _ => return false,
            }
        }
    }
    true
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct SessionMeta {
    #[allow(dead_code)]
    created: Instant,
    last_used: Instant,
}

#[derive(Default)]
struct LoginAttempts {
    window_start: Option<Instant>,
    failures: u32,
}

#[derive(Clone)]
struct AppState {
    engine: Arc<Engine>,
    sessions: Arc<Mutex<HashMap<String, SessionMeta>>>,
    categories: Arc<Mutex<HashMap<String, String>>>,
    login_attempts: Arc<Mutex<HashMap<IpAddr, LoginAttempts>>>,
    username: String,
    /// Argon2id PHC-encoded password hash.
    password_hash: Arc<String>,
    download_dir: String,
    /// Smoothed (EMA) ETA per info_hash, in seconds.
    eta_ema: Arc<Mutex<HashMap<[u8; 20], f64>>>,
    /// Optional allow-list of trusted proxy IPs; when a request arrives from
    /// one of these, `X-Forwarded-For` is honored for client IP.
    trusted_proxies: Arc<Vec<IpAddr>>,
}

fn extract_sid(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("SID=") {
            return Some(val.to_string());
        }
    }
    None
}

fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let sid = extract_sid(headers).ok_or(StatusCode::FORBIDDEN)?;
    let mut sessions = state
        .sessions
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let now = Instant::now();
    sessions.retain(|_, meta| now.duration_since(meta.last_used) < SESSION_IDLE_TTL);
    match sessions.get_mut(&sid) {
        Some(meta) => {
            meta.last_used = now;
            Ok(())
        }
        None => Err(StatusCode::FORBIDDEN),
    }
}

pub fn hash_password(plaintext: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut password_hash::rand_core::OsRng);
    let hash = Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))?
        .to_string();
    Ok(hash)
}

fn verify_password(stored_phc: &str, submitted: &str) -> bool {
    match PasswordHash::new(stored_phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(submitted.as_bytes(), &parsed)
            .is_ok(),
        Err(e) => {
            warn!(error = %e, "stored password hash is malformed");
            false
        }
    }
}

/// Returns the PHC string for an api_password from config: if already a PHC
/// argon2 hash, returned verbatim; otherwise hashed now with a warning.
pub fn prepare_password_hash(api_password: &str) -> anyhow::Result<String> {
    if api_password.starts_with("$argon2") {
        // Validate parseability early.
        PasswordHash::new(api_password)
            .map_err(|e| anyhow::anyhow!("invalid argon2 PHC string in config: {e}"))?;
        Ok(api_password.to_string())
    } else {
        let hashed = hash_password(api_password)?;
        warn!(
            "api_password is stored as plaintext in config; hashed in-memory for this run. \
             Replace with an argon2id PHC hash at rest: {hashed}"
        );
        Ok(hashed)
    }
}

fn record_login_failure(state: &AppState, ip: IpAddr) {
    let Ok(mut map) = state.login_attempts.lock() else {
        return;
    };
    let now = Instant::now();
    // LRU-ish eviction: when the map grows past LOGIN_ATTEMPTS_MAX, drop
    // entries whose window has already expired; if still too large, drop
    // the oldest-windowed entries until under cap. Prevents unbounded growth
    // under spoofed-IP floods on the LAN.
    if map.len() >= LOGIN_ATTEMPTS_MAX {
        map.retain(|_, a| {
            a.window_start
                .map(|s| now.duration_since(s) < LOGIN_WINDOW)
                .unwrap_or(false)
        });
        if map.len() >= LOGIN_ATTEMPTS_MAX {
            let mut entries: Vec<_> = map.iter().map(|(k, v)| (*k, v.window_start)).collect();
            entries.sort_by_key(|(_, ws)| *ws);
            let drop_n = map.len().saturating_sub(LOGIN_ATTEMPTS_MAX / 2);
            for (k, _) in entries.into_iter().take(drop_n) {
                map.remove(&k);
            }
        }
    }
    let entry = map.entry(ip).or_default();
    match entry.window_start {
        Some(start) if now.duration_since(start) < LOGIN_WINDOW => {
            entry.failures = entry.failures.saturating_add(1);
        }
        _ => {
            entry.window_start = Some(now);
            entry.failures = 1;
        }
    }
}

fn is_rate_limited(state: &AppState, ip: IpAddr) -> bool {
    let Ok(mut map) = state.login_attempts.lock() else {
        return false;
    };
    let now = Instant::now();
    let entry = map.entry(ip).or_default();
    match entry.window_start {
        Some(start) if now.duration_since(start) < LOGIN_WINDOW => {
            entry.failures >= LOGIN_MAX_FAILURES
        }
        _ => {
            entry.window_start = Some(now);
            entry.failures = 0;
            false
        }
    }
}

fn clear_login_attempts(state: &AppState, ip: IpAddr) {
    if let Ok(mut map) = state.login_attempts.lock() {
        map.remove(&ip);
    }
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

fn resolve_client_ip(state: &AppState, addr: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let peer = addr.ip();
    if !state.trusted_proxies.contains(&peer) {
        return peer;
    }
    if let Some(val) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = val.split(',').next() {
            if let Ok(parsed) = first.trim().parse::<IpAddr>() {
                return parsed;
            }
        }
    }
    peer
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> impl IntoResponse {
    let ip = resolve_client_ip(&state, addr, &headers);
    if is_rate_limited(&state, ip) {
        info!(%ip, "auth login rate-limited");
        return (StatusCode::TOO_MANY_REQUESTS, "Too many login attempts.").into_response();
    }

    let user_ok = form.username == state.username;
    let pass_ok = verify_password(&state.password_hash, &form.password);
    if user_ok && pass_ok {
        let sid = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        if let Ok(mut sessions) = state.sessions.lock() {
            sessions.insert(
                sid.clone(),
                SessionMeta {
                    created: now,
                    last_used: now,
                },
            );
        }
        clear_login_attempts(&state, ip);
        info!(username = %state.username, "auth login success");
        (
            StatusCode::OK,
            [(
                axum::http::header::SET_COOKIE,
                format!("SID={sid}; HttpOnly; SameSite=Strict; Path=/"),
            )],
            "Ok.",
        )
            .into_response()
    } else {
        record_login_failure(&state, ip);
        info!(%ip, "auth login failed");
        (StatusCode::UNAUTHORIZED, "Fails.").into_response()
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(sid) = extract_sid(&headers) {
        if let Ok(mut sessions) = state.sessions.lock() {
            sessions.remove(&sid);
        }
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::SET_COOKIE,
            "SID=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0".to_string(),
        )],
        "Ok.",
    )
}

async fn build_info() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "qt": "6.7.2",
        "libtorrent": "2.0.11.0",
        "boost": "1.86.0",
        "openssl": "3.3.1",
        "zlib": "1.3.1",
        "bitness": 64,
        "app": concat!("lightorrent v", env!("CARGO_PKG_VERSION")),
        "lightorrent": {
            "version": env!("CARGO_PKG_VERSION"),
            "git": env!("BUILD_GIT_HASH"),
            "built": env!("BUILD_TIMESTAMP"),
        },
    }))
}

async fn app_version(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<&'static str, StatusCode> {
    require_auth(&state, &headers)?;
    Ok("v5.0.0")
}

async fn webapi_version(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<&'static str, StatusCode> {
    require_auth(&state, &headers)?;
    Ok("2.10.4")
}

fn map_state(snap: &TorrentSnapshot) -> &'static str {
    if snap.paused {
        return if snap.completed {
            "pausedUP"
        } else {
            "pausedDL"
        };
    }
    if snap.has_error {
        return "error";
    }
    if snap.completed {
        if snap.up_speed > 0.0 {
            "uploading"
        } else {
            "stalledUP"
        }
    } else if snap.dl_speed > 0.0 {
        "downloading"
    } else {
        "stalledDL"
    }
}

async fn torrents_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    require_auth(&state, &headers)?;

    let snapshots = state.engine.snapshot_all().await;

    // Smooth ETAs with an EMA so *arr UIs don't churn on 1-s samples.
    let smoothed_etas: HashMap<[u8; 20], i64> = {
        let mut out = HashMap::new();
        if let Ok(mut map) = state.eta_ema.lock() {
            for snap in &snapshots {
                let remaining = snap.total_bytes.saturating_sub(snap.progress_bytes);
                let raw_val = if snap.completed {
                    0
                } else {
                    raw_eta(remaining, snap.dl_speed)
                };
                let entry = map.entry(snap.info_hash).or_insert(raw_val as f64);
                *entry = ETA_EMA_ALPHA * raw_val as f64 + (1.0 - ETA_EMA_ALPHA) * *entry;
                out.insert(snap.info_hash, f64_to_i64_clamped(*entry).min(ETA_MAX));
            }
        } else {
            for snap in &snapshots {
                let remaining = snap.total_bytes.saturating_sub(snap.progress_bytes);
                let raw_val = if snap.completed {
                    0
                } else {
                    raw_eta(remaining, snap.dl_speed)
                };
                out.insert(snap.info_hash, raw_val);
            }
        }
        out
    };

    let mut torrents = Vec::with_capacity(snapshots.len());
    for snap in &snapshots {
        let ratio = if snap.total_bytes > 0 {
            snap.uploaded as f64 / snap.total_bytes as f64
        } else {
            0.0
        };

        let qb_state = map_state(snap);

        let record = state.engine.registry().get(&snap.info_hash).ok().flatten();

        let ratio_limit = record
            .as_ref()
            .map(|rec| {
                if rec.ratio_target > 0.0 {
                    rec.ratio_target
                } else {
                    -2.0
                }
            })
            .unwrap_or(-2.0);

        let added_on = record.as_ref().map(|rec| rec.added_at).unwrap_or(0);

        let completion_on = record
            .as_ref()
            .and_then(|rec| rec.completed_at)
            .unwrap_or_else(|| if snap.completed { now_epoch() } else { 0 });

        let torrent_category = record
            .as_ref()
            .map(|rec| rec.category.as_str())
            .unwrap_or("");

        let now_secs = now_epoch();
        let seeding_time = if completion_on > 0 {
            (now_secs.saturating_sub(completion_on)) as i64
        } else {
            0
        };

        let save_path = &state.download_dir;
        let content_path = format!("{}/{}", save_path, snap.name);
        let eta = smoothed_etas.get(&snap.info_hash).copied().unwrap_or(0);
        let peers = snap.peer_count as i64;

        torrents.push(serde_json::json!({
            "hash": snap.hash,
            "name": snap.name,
            "size": snap.total_bytes,
            "total_size": snap.total_bytes,
            "amount_left": snap.total_bytes.saturating_sub(snap.progress_bytes),
            "progress": snap.progress,
            "dlspeed": f64_to_i64_clamped(snap.dl_speed),
            "upspeed": f64_to_i64_clamped(snap.up_speed),
            "state": qb_state,
            "save_path": save_path,
            "content_path": content_path,
            "ratio": ratio,
            "uploaded": snap.uploaded,
            "downloaded": snap.downloaded,
            "added_on": added_on,
            "completion_on": completion_on,
            "category": torrent_category,
            "tags": "",
            "num_seeds": if snap.completed { 0 } else { peers },
            "num_leechs": if snap.completed { peers } else { 0 },
            "eta": eta,
            "ratio_limit": ratio_limit,
            "seeding_time": seeding_time,
            "seeding_time_limit": -1,
            "inactive_seeding_time_limit": -1,
            "last_activity": now_secs,
            "stopped": snap.paused,
        }));
    }

    if let Some(cat) = params.get("category").filter(|c| !c.is_empty()) {
        torrents.retain(|item| item["category"] == *cat);
    }

    Ok(axum::Json(torrents))
}

#[derive(Deserialize)]
struct HashesForm {
    hashes: Option<String>,
    #[serde(alias = "deleteFiles")]
    delete_files: Option<String>,
}

#[derive(Deserialize)]
struct ShareLimitsForm {
    hashes: Option<String>,
    #[serde(alias = "ratioLimit")]
    ratio_limit: Option<f64>,
}

async fn torrents_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Err(code) = require_auth(&state, &headers) {
        return (code, "Fails.").into_response();
    }

    let mut urls: Vec<String> = Vec::new();
    let mut torrent_files: Vec<Vec<u8>> = Vec::new();
    let mut savepath: Option<String> = None;
    let mut paused = false;
    let mut category = String::new();

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if buf.len().saturating_add(chunk.len()) > MULTIPART_FIELD_MAX_BYTES {
                        info!(field = %name, "multipart field exceeded cap");
                        return (StatusCode::PAYLOAD_TOO_LARGE, "Field too large.").into_response();
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    info!(error = %e, "multipart read error");
                    return (StatusCode::BAD_REQUEST, "Multipart error.").into_response();
                }
            }
        }
        match name.as_str() {
            "urls" => {
                if let Ok(text) = std::str::from_utf8(&buf) {
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            urls.push(trimmed.to_string());
                        }
                    }
                }
            }
            "torrents" if !buf.is_empty() => {
                torrent_files.push(buf);
            }
            "savepath" => {
                if let Ok(text) = String::from_utf8(buf) {
                    savepath = Some(text);
                }
            }
            "paused" if buf == b"true" => {
                paused = true;
            }
            "stopped" if buf == b"true" => {
                paused = true;
            }
            "category" => {
                if let Ok(text) = String::from_utf8(buf) {
                    category = text;
                }
            }
            _ => {}
        }
    }

    if let Some(ref sp) = savepath {
        if !validate_save_path(sp, &state.download_dir) {
            info!(save_path = %sp, "rejecting add: savePath escapes download_dir");
            return (StatusCode::BAD_REQUEST, "Invalid savepath.").into_response();
        }
    }
    for url in &urls {
        if let Err(e) = validate_add_url(url) {
            info!(error = %e, url = %url, "rejecting URL");
            return (StatusCode::BAD_REQUEST, "Invalid URL.").into_response();
        }
    }

    let effective_save_path = savepath.as_deref().unwrap_or(&state.download_dir);

    for url in &urls {
        if let Err(e) = state
            .engine
            .add_torrent_url(url, effective_save_path, &category, paused)
            .await
        {
            info!(error = %e, "torrent add failed (url)");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Fails.").into_response();
        }
    }
    for file_bytes in torrent_files {
        if let Err(e) = state
            .engine
            .add_torrent_bytes(file_bytes, effective_save_path, &category, paused)
            .await
        {
            info!(error = %e, "torrent add failed (file)");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Fails.").into_response();
        }
    }

    info!(url_count = urls.len(), "torrents added via API");
    (StatusCode::OK, "Ok.").into_response()
}

async fn torrents_pause(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashesForm>,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    let hashes_str = form.hashes.unwrap_or_default();
    let hashes = parse_hashes(&state, &hashes_str).await;
    for hash in &hashes {
        state.engine.pause_torrent(hash).await.map_err(|e| {
            info!(error = %e, %hash, "pause failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    Ok(())
}

async fn torrents_resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashesForm>,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    let hashes_str = form.hashes.unwrap_or_default();
    let hashes = parse_hashes(&state, &hashes_str).await;
    for hash in &hashes {
        state.engine.resume_torrent(hash).await.map_err(|e| {
            info!(error = %e, %hash, "resume failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    Ok(())
}

async fn torrents_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashesForm>,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    let hashes_str = form.hashes.unwrap_or_default();
    let delete_files = form.delete_files.as_deref() == Some("true");
    let hashes = parse_hashes(&state, &hashes_str).await;
    for hash in &hashes {
        state
            .engine
            .delete_torrent(hash, delete_files)
            .await
            .map_err(|e| {
                info!(error = %e, %hash, "delete failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }
    Ok(())
}

async fn torrents_set_share_limits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ShareLimitsForm>,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    let hashes_str = form.hashes.unwrap_or_default();
    let ratio = form.ratio_limit.unwrap_or(-1.0);
    let hashes = parse_hashes(&state, &hashes_str).await;
    for hash in &hashes {
        state.engine.set_ratio_target(hash, ratio).map_err(|e| {
            info!(error = %e, %hash, "set ratio target failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    Ok(())
}

async fn torrents_properties(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    require_auth(&state, &headers)?;
    let hash = params.get("hash").ok_or(StatusCode::BAD_REQUEST)?;
    if !is_valid_infohash_v1(hash) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let snap = state
        .engine
        .snapshot(hash)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    let ratio = if snap.total_bytes > 0 {
        snap.uploaded as f64 / snap.total_bytes as f64
    } else {
        0.0
    };

    let record = state.engine.registry().get(&snap.info_hash).ok().flatten();

    let added_at = record.as_ref().map(|r| r.added_at).unwrap_or(0);
    let completed_at = record.as_ref().and_then(|r| r.completed_at).unwrap_or(0);

    let now_secs = now_epoch();
    let time_elapsed = if added_at > 0 {
        (now_secs.saturating_sub(added_at)) as i64
    } else {
        0
    };
    let seeding_time = if completed_at > 0 {
        (now_secs.saturating_sub(completed_at)) as i64
    } else {
        0
    };
    let completion_date = if completed_at > 0 {
        completed_at as i64
    } else {
        -1
    };

    Ok(axum::Json(serde_json::json!({
        "save_path": state.download_dir,
        "creation_date": 0,
        "piece_size": snap.piece_length,
        "comment": "",
        "total_wasted": 0,
        "total_uploaded": snap.uploaded,
        "total_downloaded": snap.downloaded,
        "up_limit": -1,
        "dl_limit": -1,
        "time_elapsed": time_elapsed,
        "seeding_time": seeding_time,
        "nb_connections": snap.peer_count,
        "share_ratio": ratio,
        "addition_date": added_at,
        "completion_date": completion_date,
        "created_by": snap.name,
        "dl_speed": f64_to_i64_clamped(snap.dl_speed),
        "up_speed": f64_to_i64_clamped(snap.up_speed),
    })))
}

async fn app_preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    require_auth(&state, &headers)?;
    Ok(axum::Json(serde_json::json!({
        "max_ratio_enabled": false,
        "max_ratio": -1,
        "max_seeding_time_enabled": false,
        "max_seeding_time": -1,
        "max_ratio_act": 0,
        "queueing_enabled": false,
        "dht": true,
    })))
}

async fn torrents_categories(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    require_auth(&state, &headers)?;
    let cats = state
        .categories
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut result = serde_json::Map::new();
    for (name, save_path) in cats.iter() {
        result.insert(
            name.clone(),
            serde_json::json!({
                "name": name,
                "savePath": save_path,
            }),
        );
    }
    Ok(axum::Json(serde_json::Value::Object(result)))
}

#[derive(Deserialize)]
struct CreateCategoryForm {
    #[serde(default)]
    category: String,
    #[serde(alias = "savePath", default)]
    save_path: String,
}

async fn torrents_create_category(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateCategoryForm>,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    if form.category.is_empty() {
        return Ok(());
    }
    if !validate_save_path(&form.save_path, &state.download_dir) {
        info!(save_path = %form.save_path, "createCategory rejected: savePath escapes download_dir");
        return Err(StatusCode::BAD_REQUEST);
    }
    state
        .categories
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .insert(form.category, form.save_path);
    Ok(())
}

async fn torrents_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    require_auth(&state, &headers)?;
    let hash = params.get("hash").ok_or(StatusCode::BAD_REQUEST)?;
    if !is_valid_infohash_v1(hash) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let files = state.engine.get_files(hash).await.map_err(|e| {
        info!(error = %e, %hash, "get files failed");
        StatusCode::NOT_FOUND
    })?;
    Ok(axum::Json(serde_json::Value::Array(files)))
}

#[derive(Deserialize)]
struct SetCategoryForm {
    #[serde(default)]
    hashes: String,
    #[serde(default)]
    category: String,
}

async fn torrents_set_category(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SetCategoryForm>,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    let hashes = parse_hashes(&state, &form.hashes).await;
    for hash in &hashes {
        if let Err(e) = state.engine.set_category(hash, &form.category) {
            info!(error = %e, %hash, "set category skipped");
        }
    }
    Ok(())
}

async fn torrents_top_prio(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    Ok(())
}

async fn torrents_set_force_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashesForm>,
) -> Result<(), StatusCode> {
    require_auth(&state, &headers)?;
    let hashes_str = form.hashes.unwrap_or_default();
    let hashes = parse_hashes(&state, &hashes_str).await;
    for hash in &hashes {
        state.engine.resume_torrent(hash).await.map_err(|e| {
            info!(error = %e, %hash, "force start failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    Ok(())
}

async fn parse_hashes(state: &AppState, hashes_str: &str) -> Vec<String> {
    if hashes_str == "all" {
        state.engine.torrent_hashes().await
    } else {
        hashes_str
            .split('|')
            .filter(|s| !s.is_empty())
            .filter(|s| is_valid_infohash_v1(s))
            .map(String::from)
            .collect()
    }
}

async fn transfer_info(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    require_auth(&state, &headers)?;

    let snapshots = state.engine.snapshot_all().await;

    let mut dl_speed = 0i64;
    let mut up_speed = 0i64;
    let mut dl_data = 0u64;
    let mut up_data = 0u64;
    for snap in &snapshots {
        dl_speed += f64_to_i64_clamped(snap.dl_speed);
        up_speed += f64_to_i64_clamped(snap.up_speed);
        up_data += snap.uploaded;
        dl_data += snap.downloaded;
    }

    Ok(axum::Json(serde_json::json!({
        "dl_info_speed": dl_speed,
        "up_info_speed": up_speed,
        "dl_info_data": dl_data,
        "up_info_data": up_data,
        "dht_nodes": 0,
    })))
}

pub fn router(engine: Arc<Engine>, config: &Config) -> Router {
    let password_hash =
        prepare_password_hash(&config.api_password).expect("failed to prepare api password hash");
    let trusted_proxies = std::env::var("LIGHTORRENT_TRUSTED_PROXIES")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|part| part.trim().parse::<IpAddr>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let state = AppState {
        engine,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        categories: Arc::new(Mutex::new(HashMap::new())),
        login_attempts: Arc::new(Mutex::new(HashMap::new())),
        username: config.api_username.clone(),
        password_hash: Arc::new(password_hash),
        download_dir: config.download_dir.clone(),
        eta_ema: Arc::new(Mutex::new(HashMap::new())),
        trusted_proxies: Arc::new(trusted_proxies),
    };

    Router::new()
        .route("/api/v2/app/buildInfo", get(build_info))
        .route("/api/v2/auth/login", post(login))
        .route("/api/v2/auth/logout", post(logout))
        .route("/api/v2/app/version", get(app_version))
        .route("/api/v2/app/webapiVersion", get(webapi_version))
        .route("/api/v2/torrents/info", get(torrents_info))
        .route(
            "/api/v2/torrents/add",
            post(torrents_add).layer(DefaultBodyLimit::max(
                MULTIPART_FIELD_MAX_BYTES.saturating_add(1024 * 1024),
            )),
        )
        .route("/api/v2/torrents/pause", post(torrents_pause))
        .route("/api/v2/torrents/resume", post(torrents_resume))
        .route("/api/v2/torrents/delete", post(torrents_delete))
        .route(
            "/api/v2/torrents/setShareLimits",
            post(torrents_set_share_limits),
        )
        .route("/api/v2/torrents/properties", get(torrents_properties))
        .route("/api/v2/app/preferences", get(app_preferences))
        .route("/api/v2/torrents/categories", get(torrents_categories))
        .route(
            "/api/v2/torrents/createCategory",
            post(torrents_create_category),
        )
        .route("/api/v2/torrents/files", get(torrents_files))
        .route("/api/v2/torrents/setCategory", post(torrents_set_category))
        .route("/api/v2/torrents/topPrio", post(torrents_top_prio))
        .route(
            "/api/v2/torrents/setForceStart",
            post(torrents_set_force_start),
        )
        .route("/api/v2/transfer/info", get(transfer_info))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_snap(
        completed: bool,
        paused: bool,
        has_error: bool,
        dl_speed: f64,
        up_speed: f64,
    ) -> TorrentSnapshot {
        TorrentSnapshot {
            info_hash: [0; 20],
            hash: String::new(),
            name: String::new(),
            total_bytes: 100,
            piece_length: 262144,
            progress: 0.0,
            progress_bytes: 0,
            uploaded: 0,
            downloaded: 0,
            dl_speed,
            up_speed,
            peer_count: 0,
            completed,
            paused,
            has_error,
        }
    }

    #[test]
    fn test_map_state_all_branches() {
        let cases: Vec<(bool, bool, bool, f64, f64, &str)> = vec![
            // completed, paused, error, dl, up, expected
            (false, true, true, 0.0, 0.0, "pausedDL"), // paused takes priority over error
            (false, false, true, 0.0, 0.0, "error"),
            (true, true, false, 0.0, 0.0, "pausedUP"),
            (false, true, false, 0.0, 0.0, "pausedDL"),
            (true, false, false, 0.0, 5.0, "uploading"),
            (true, false, false, 0.0, 0.0, "stalledUP"),
            (false, false, false, 5.0, 0.0, "downloading"),
            (false, false, false, 0.0, 0.0, "stalledDL"),
        ];

        for (completed, paused, has_error, dl_speed, up_speed, expected) in &cases {
            let snap = make_snap(*completed, *paused, *has_error, *dl_speed, *up_speed);
            let result = map_state(&snap);
            assert_eq!(
                result, *expected,
                "map_state(completed={}, paused={}, error={}, dl={}, up={}) = {:?}, expected {:?}",
                completed, paused, has_error, dl_speed, up_speed, result, expected
            );
        }
    }
}
