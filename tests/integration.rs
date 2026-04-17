use lightorrent::config::Config;
use lightorrent::engine::Engine;
use lightorrent::store::{TorrentRecord, TorrentRegistry};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::multipart;

#[tokio::test]
async fn test_config_load_and_env_override() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("test.toml");
    {
        let mut f = std::fs::File::create(&toml_path).unwrap();
        writeln!(f, r#"download_dir = "/tmp/test_dl""#).unwrap();
        writeln!(f, "listen_port = 9999").unwrap();
    }

    let cfg = Config::load(toml_path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.download_dir, "/tmp/test_dl");
    assert_eq!(cfg.listen_port, 9999);
    assert_eq!(cfg.persistence_dir, "./data/session"); // default

    // Env overrides
    unsafe {
        std::env::set_var("LIGHTORRENT_DOWNLOAD_DIR", "/tmp/env_dl");
        std::env::set_var("LIGHTORRENT_LISTEN_PORT", "7777");
        std::env::set_var("LIGHTORRENT_PERSISTENCE_DIR", "/tmp/env_persist");
    }
    let cfg2 = Config::load(toml_path.to_str().unwrap()).unwrap();
    assert_eq!(cfg2.download_dir, "/tmp/env_dl");
    assert_eq!(cfg2.listen_port, 7777);
    assert_eq!(cfg2.persistence_dir, "/tmp/env_persist");

    // Clean up env vars
    unsafe {
        std::env::remove_var("LIGHTORRENT_DOWNLOAD_DIR");
        std::env::remove_var("LIGHTORRENT_LISTEN_PORT");
        std::env::remove_var("LIGHTORRENT_PERSISTENCE_DIR");
    }
}

#[tokio::test]
async fn test_engine_session_lifecycle() {
    let dl_dir = tempfile::tempdir().unwrap();
    let persist_dir = tempfile::tempdir().unwrap();

    let cfg = Config {
        download_dir: dl_dir.path().to_str().unwrap().to_string(),
        listen_port: 0,
        persistence_dir: persist_dir.path().to_str().unwrap().to_string(),
        torrents: None,
        api_bind_address: "127.0.0.1".to_string(),
        api_port: 0,
        api_username: "admin".to_string(),
        api_password: "adminadmin".to_string(),
        state_db_path: dl_dir
            .path()
            .join("state.redb")
            .to_str()
            .unwrap()
            .to_string(),
    };

    let engine = Engine::new(&cfg).await.expect("engine should start");
    engine.shutdown().await;

    // Directories should still exist after shutdown
    assert!(dl_dir.path().exists());
    assert!(persist_dir.path().exists());
}

async fn start_test_server() -> (
    Arc<Engine>,
    u16,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let dl_dir = tempfile::tempdir().unwrap();
    let persist_dir = tempfile::tempdir().unwrap();

    let cfg = Config {
        download_dir: dl_dir.path().to_str().unwrap().to_string(),
        listen_port: 0,
        persistence_dir: persist_dir.path().to_str().unwrap().to_string(),
        torrents: None,
        api_bind_address: "127.0.0.1".to_string(),
        api_port: 0,
        api_username: "admin".to_string(),
        api_password: "adminadmin".to_string(),
        state_db_path: dl_dir
            .path()
            .join("state.redb")
            .to_str()
            .unwrap()
            .to_string(),
    };

    let engine = Arc::new(Engine::new(&cfg).await.expect("engine should start"));
    let router = lightorrent::api::router(engine.clone(), &cfg);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    (engine, port, handle, dl_dir, persist_dir)
}

async fn login(client: &reqwest::Client, port: u16) -> String {
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=adminadmin")
        .send()
        .await
        .unwrap();
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    // Extract "SID=<value>" from "SID=<value>; HttpOnly"
    cookie.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn test_api_auth_login_success() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=adminadmin")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .expect("should have Set-Cookie");
    let cookie_str = set_cookie.to_str().unwrap();
    assert!(cookie_str.contains("SID="), "cookie should contain SID");
    let body = resp.text().await.unwrap();
    assert_eq!(body, "Ok.");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_auth_login_failure() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=wrongpassword")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert!(
        resp.headers().get("set-cookie").is_none(),
        "should not set cookie on failure"
    );
    let body = resp.text().await.unwrap();
    assert_eq!(body, "Fails.");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_version_with_auth() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/version"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "v5.0.0");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_version_without_auth() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/version"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_build_info_no_auth() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/buildInfo"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.get("qt").is_some());
    assert!(body.get("libtorrent").is_some());
    assert_eq!(body["bitness"], 64);
    assert!(body["app"].as_str().unwrap().contains("lightorrent"));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_info_empty() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/torrents/info"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!([]));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_webapi_version() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/webapiVersion"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "2.10.4");

    engine.shutdown().await;
}

/// Encode a bencode string (byte string with length prefix).
fn bencode_str(val: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", val.len()).into_bytes();
    out.extend_from_slice(val);
    out
}

/// Build a minimal valid .torrent file as raw bencode bytes.
fn make_test_torrent(_dl_dir: &std::path::Path) -> Vec<u8> {
    let file_content = b"hello torrent";
    let piece_length: i64 = 16384;

    // SHA1 of file content for the pieces field
    let digest = sha1_hash(file_content);

    let announce = b"http://tracker.invalid:6969";

    // Info dict (keys must be sorted)
    let mut info = Vec::new();
    info.push(b'd');
    info.extend_from_slice(&bencode_str(b"length"));
    info.extend_from_slice(format!("i{}e", file_content.len()).as_bytes());
    info.extend_from_slice(&bencode_str(b"name"));
    info.extend_from_slice(&bencode_str(b"testfile.txt"));
    info.extend_from_slice(&bencode_str(b"piece length"));
    info.extend_from_slice(format!("i{}e", piece_length).as_bytes());
    info.extend_from_slice(&bencode_str(b"pieces"));
    info.extend_from_slice(&bencode_str(&digest));
    info.push(b'e');

    // Outer dict
    let mut torrent = Vec::new();
    torrent.push(b'd');
    torrent.extend_from_slice(&bencode_str(b"announce"));
    torrent.extend_from_slice(&bencode_str(announce));
    torrent.extend_from_slice(&bencode_str(b"info"));
    // info is already a full bencode dict (d...e), not a string
    torrent.extend_from_slice(&info);
    torrent.push(b'e');

    torrent
}

fn sha1_hash(data: &[u8]) -> Vec<u8> {
    use std::process::Command;
    let mut child = Command::new("shasum")
        .arg("-a")
        .arg("1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let output = child.wait_with_output().unwrap();
    let hex = String::from_utf8(output.stdout).unwrap();
    let hex = hex.split_whitespace().next().unwrap();
    let mut result = vec![0u8; 20];
    for i in 0..20 {
        result[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    result
}

async fn add_torrent_file(client: &reqwest::Client, port: u16, sid: &str, torrent_bytes: Vec<u8>) {
    let form = multipart::Form::new().part(
        "torrents",
        multipart::Part::bytes(torrent_bytes).file_name("test.torrent"),
    );

    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .header("cookie", sid)
        .multipart(form)
        .send()
        .await
        .unwrap();

    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "add torrent failed: {body}");
    assert_eq!(body, "Ok.");
}

async fn get_torrents_info(
    client: &reqwest::Client,
    port: u16,
    sid: &str,
) -> Vec<serde_json::Value> {
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/torrents/info"))
        .header("cookie", sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn test_api_torrents_add_file() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    // Verify torrent appears in info
    let info = get_torrents_info(&client, port, &sid).await;
    assert!(!info.is_empty(), "torrent should appear in info");
    assert!(info[0]["hash"].as_str().is_some_and(|h| !h.is_empty()));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_add_magnet() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // Magnet links are now rejected with 400
    let magnet = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000&dn=test";
    let form = multipart::Form::new().text("urls", magnet.to_string());

    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .header("cookie", &sid)
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_pause_resume() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    // Small delay to let torrent settle after add
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap();

    // Pause
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/pause"))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Check state is paused
    let info = get_torrents_info(&client, port, &sid).await;
    let state = info[0]["state"].as_str().unwrap();
    assert!(
        state.starts_with("paused"),
        "expected paused state, got: {state}"
    );

    // Resume
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/resume"))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Check state is no longer paused
    let info = get_torrents_info(&client, port, &sid).await;
    let state = info[0]["state"].as_str().unwrap();
    assert!(
        !state.starts_with("paused"),
        "expected non-paused state after resume, got: {state}"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_delete() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap();

    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/delete"))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}&deleteFiles=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify torrent is gone
    let info = get_torrents_info(&client, port, &sid).await;
    assert!(info.is_empty(), "torrent list should be empty after delete");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_set_share_limits() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap();

    let resp = client
        .post(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/setShareLimits"
        ))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}&ratioLimit=2.0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_properties() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap();

    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/properties?hash={hash}"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let props: serde_json::Value = resp.json().await.unwrap();
    assert!(props.get("save_path").is_some(), "should have save_path");
    assert!(
        props.get("share_ratio").is_some(),
        "should have share_ratio"
    );

    // Verify real values instead of hardcoded zeros
    let addition_date = props["addition_date"].as_u64().unwrap();
    assert!(
        addition_date > 0,
        "addition_date should be real epoch, got {addition_date}"
    );
    let time_elapsed = props["time_elapsed"].as_i64().unwrap();
    assert!(
        time_elapsed >= 0,
        "time_elapsed should be >= 0, got {time_elapsed}"
    );
    let nb_connections = props["nb_connections"].as_i64().unwrap();
    assert!(nb_connections >= 0, "nb_connections should be >= 0");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_properties_completion_values() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap().to_string();

    // Simulate completion by writing completed_at to registry
    let completed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 10;
    let info_hash_bytes: [u8; 20] = {
        let mut buf = [0u8; 20];
        for i in 0..20 {
            buf[i] = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16).unwrap();
        }
        buf
    };
    let mut record = engine
        .registry()
        .get(&info_hash_bytes)
        .unwrap()
        .expect("record should exist");
    record.completed_at = Some(completed_at);
    engine.registry().upsert(&info_hash_bytes, &record).unwrap();

    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/properties?hash={hash}"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let props: serde_json::Value = resp.json().await.unwrap();
    let completion_date = props["completion_date"].as_i64().unwrap();
    assert_eq!(
        completion_date, completed_at as i64,
        "completion_date should match completed_at"
    );
    let seeding_time = props["seeding_time"].as_i64().unwrap();
    assert!(
        seeding_time >= 10,
        "seeding_time should be >= 10, got {seeding_time}"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_properties_bad_hash() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/properties?hash=badhash"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    // 400: hash is not 40 hex chars (rejected before lookup).
    assert_eq!(resp.status(), 400);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_registry_crud() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test_crud.redb");
    let registry = TorrentRegistry::open(db_path.to_str().unwrap()).unwrap();

    let info_hash: [u8; 20] = [0u8; 20];
    let record = TorrentRecord {
        info_hash_hex: "0000000000000000000000000000000000000000".to_string(),
        source: "<test>".to_string(),
        save_path: "/tmp/test".to_string(),
        ratio_target: 0.0,
        added_at: 1000,
        user_paused: false,
        completed_at: None,
        category: String::new(),
        total_uploaded: 0,
        total_downloaded: 0,
        piece_bitfield: Vec::new(),
        file_mtime: None,
        file_size: None,
    };

    // Upsert and get
    registry.upsert(&info_hash, &record).unwrap();
    let got = registry
        .get(&info_hash)
        .unwrap()
        .expect("should exist after upsert");
    assert_eq!(got.info_hash_hex, record.info_hash_hex);
    assert_eq!(got.source, "<test>");
    assert!((got.ratio_target - 0.0).abs() < f64::EPSILON);

    // Set ratio target and verify
    registry.set_ratio_target(&info_hash, 2.5).unwrap();
    let got = registry.get(&info_hash).unwrap().unwrap();
    assert!((got.ratio_target - 2.5).abs() < f64::EPSILON);

    // List should have 1 entry
    let list = registry.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, info_hash);

    // Remove and verify gone
    registry.remove(&info_hash).unwrap();
    assert!(registry.get(&info_hash).unwrap().is_none());
    assert!(registry.list().unwrap().is_empty());
}

#[tokio::test]
async fn test_store_error_paths() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test_errors.redb");
    let registry = TorrentRegistry::open(db_path.to_str().unwrap()).unwrap();

    let missing_hash: [u8; 20] = [0xFFu8; 20];
    assert!(registry.set_ratio_target(&missing_hash, 1.0).is_err());
    assert!(registry.set_user_paused(&missing_hash, true).is_err());
    assert!(registry.set_category(&missing_hash, "movies").is_err());
}

#[tokio::test]
async fn test_store_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test_edges.redb");
    let registry = TorrentRegistry::open(db_path.to_str().unwrap()).unwrap();

    assert!(registry.get(&[0u8; 20]).unwrap().is_none());
    assert!(registry.list().unwrap().is_empty());
}

#[tokio::test]
async fn test_store_serde_backward_compat() {
    // JSON missing total_uploaded and total_downloaded fields
    let json = r#"{
        "info_hash_hex": "abcd",
        "source": "test",
        "save_path": "/tmp",
        "ratio_target": 1.0,
        "added_at": 1000,
        "user_paused": false
    }"#;
    let record: TorrentRecord = serde_json::from_str(json).unwrap();
    assert_eq!(
        record.total_uploaded, 0,
        "total_uploaded should default to 0"
    );
    assert_eq!(
        record.total_downloaded, 0,
        "total_downloaded should default to 0"
    );
    assert_eq!(record.category, "", "category should default to empty");
    assert!(
        record.completed_at.is_none(),
        "completed_at should default to None"
    );
}

#[tokio::test]
async fn test_engine_restart_recovery() {
    let dl_dir = tempfile::tempdir().unwrap();
    let persist_dir = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let state_db = state_dir.path().join("state.redb");

    let make_cfg = || Config {
        download_dir: dl_dir.path().to_str().unwrap().to_string(),
        listen_port: 0,
        persistence_dir: persist_dir.path().to_str().unwrap().to_string(),
        torrents: None,
        api_bind_address: "127.0.0.1".to_string(),
        api_port: 0,
        api_username: "admin".to_string(),
        api_password: "adminadmin".to_string(),
        state_db_path: state_db.to_str().unwrap().to_string(),
    };

    // First engine: add torrent and set ratio target
    let cfg1 = make_cfg();
    let engine1 = Engine::new(&cfg1).await.expect("engine1 should start");
    let torrent_bytes = make_test_torrent(dl_dir.path());
    let hash = engine1
        .add_torrent_bytes(torrent_bytes, dl_dir.path().to_str().unwrap(), "", false)
        .await
        .expect("add torrent should succeed");
    engine1.set_ratio_target(&hash, 2.5).unwrap();

    // Verify ratio target is set
    let list1 = engine1.registry().list().unwrap();
    assert_eq!(list1.len(), 1);
    assert!((list1[0].1.ratio_target - 2.5).abs() < f64::EPSILON);

    engine1.shutdown().await;
    drop(engine1); // Release redb file lock before reopening

    // Second engine: same config, same state_db_path
    let cfg2 = make_cfg();
    let engine2 = Engine::new(&cfg2).await.expect("engine2 should start");

    // Registry should have the torrent with ratio target intact
    let list2 = engine2.registry().list().unwrap();
    assert_eq!(
        list2.len(),
        1,
        "registry should have 1 record after restart"
    );
    assert!(
        (list2[0].1.ratio_target - 2.5).abs() < f64::EPSILON,
        "ratio target should survive restart"
    );
    assert_eq!(list2[0].1.info_hash_hex, hash);

    // Torrent should also be in the engine state (re-added from registry)
    let snapshots = engine2.snapshot_all().await;
    assert!(
        !snapshots.is_empty(),
        "torrent should be present in engine after restart"
    );

    engine2.shutdown().await;
}

#[tokio::test]
async fn test_ratio_enforcement_via_api() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap();

    // Set ratio limit via API
    let resp = client
        .post(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/setShareLimits"
        ))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}&ratioLimit=2.0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify ratio target persisted in registry
    let registry_records = engine.registry().list().unwrap();
    let matching = registry_records
        .iter()
        .find(|(_, r)| r.info_hash_hex == hash);
    assert!(matching.is_some(), "torrent should be in registry");
    let (_, record) = matching.unwrap();
    assert!(
        (record.ratio_target - 2.0).abs() < f64::EPSILON,
        "ratio target should be 2.0, got {}",
        record.ratio_target
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_sonarr_test_connection_flow() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();

    // Step 1: POST /api/v2/auth/login -> 200, SID cookie
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=adminadmin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .expect("should have Set-Cookie");
    let sid = cookie
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(sid.starts_with("SID="));

    // Step 2: GET /api/v2/app/webapiVersion -> 200, "2.10.4"
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/webapiVersion"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "2.10.4");

    // Step 3: GET /api/v2/app/version -> 200, "v5.0.0"
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/version"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "v5.0.0");

    // Step 4: GET /api/v2/app/preferences -> 200, JSON with max_ratio, max_seeding_time
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/preferences"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let prefs: serde_json::Value = resp.json().await.unwrap();
    assert!(
        prefs.get("max_ratio").is_some(),
        "preferences should have max_ratio"
    );
    assert!(
        prefs.get("max_seeding_time").is_some(),
        "preferences should have max_seeding_time"
    );

    // Step 5: GET /api/v2/torrents/categories -> 200, {}
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/categories"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let cats: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(cats, serde_json::json!({}));

    // Step 6: GET /api/v2/torrents/info -> 200, []
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/torrents/info"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let info: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(info, serde_json::json!([]));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_sonarr_add_poll_flow() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    // Add torrent with stopped=true (Sonarr v5+ sends this instead of paused=true)
    let form = multipart::Form::new()
        .part(
            "torrents",
            multipart::Part::bytes(torrent_bytes).file_name("test.torrent"),
        )
        .text("stopped", "true");

    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .header("cookie", &sid)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Poll /torrents/info and verify the torrent appears paused
    let info = get_torrents_info(&client, port, &sid).await;
    assert!(!info.is_empty(), "torrent should appear in info");
    let t = &info[0];

    let state = t["state"].as_str().unwrap();
    assert!(
        state.starts_with("paused"),
        "torrent added with stopped=true should be paused, got: {state}"
    );

    // Verify all Sonarr-read fields are present
    for field in &[
        "ratio_limit",
        "seeding_time",
        "seeding_time_limit",
        "last_activity",
        "save_path",
        "content_path",
        "hash",
        "name",
        "progress",
        "state",
        "eta",
    ] {
        assert!(t.get(*field).is_some(), "missing field: {field}");
    }

    engine.shutdown().await;
}

#[tokio::test]
async fn test_stub_endpoints() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // POST /api/v2/torrents/createCategory
    let resp = client
        .post(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/createCategory"
        ))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("category=tv-sonarr&savePath=tv")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "createCategory should return 200");

    // POST /api/v2/torrents/setCategory
    let resp = client
        .post(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/setCategory"
        ))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("hashes=abc&category=tv-sonarr")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "setCategory should return 200");

    // POST /api/v2/torrents/topPrio
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/topPrio"))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("hashes=abc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "topPrio should return 200");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_torrents_files() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap();

    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/files?hash={hash}"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let files: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(!files.is_empty(), "should have at least one file entry");
    for file in &files {
        assert!(
            file["name"].is_string(),
            "file entry should have name (string)"
        );
        assert!(
            file["size"].is_number(),
            "file entry should have size (number)"
        );
        assert!(
            file["progress"].is_number(),
            "file entry should have progress (number)"
        );
        assert!(
            file["priority"].is_number(),
            "file entry should have priority (number)"
        );
    }

    engine.shutdown().await;
}

#[tokio::test]
async fn test_torrents_set_force_start() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap().to_string();

    // Pause the torrent first
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/pause"))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let info = get_torrents_info(&client, port, &sid).await;
    let state = info[0]["state"].as_str().unwrap();
    assert!(
        state.starts_with("paused"),
        "should be paused before force start"
    );

    // Force start (should resume)
    let resp = client
        .post(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/setForceStart"
        ))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let info = get_torrents_info(&client, port, &sid).await;
    let state = info[0]["state"].as_str().unwrap();
    assert!(
        !state.starts_with("paused"),
        "should not be paused after force start, got: {state}"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_new_endpoints_require_auth_s05() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();

    // GET endpoints that should require auth
    let get_endpoints = [
        "/api/v2/app/preferences",
        "/api/v2/torrents/categories",
        "/api/v2/torrents/files?hash=abc",
    ];
    for path in &get_endpoints {
        let resp = client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{path} should require auth");
    }

    // POST endpoints that should require auth
    let post_endpoints = [
        "/api/v2/torrents/createCategory",
        "/api/v2/torrents/setCategory",
        "/api/v2/torrents/topPrio",
        "/api/v2/torrents/setForceStart",
    ];
    for path in &post_endpoints {
        let resp = client
            .post(format!("http://127.0.0.1:{port}{path}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body("hashes=abc")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{path} should require auth");
    }

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_new_endpoints_require_auth() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();

    // POST endpoints that require auth -- use empty multipart for /add
    let form = multipart::Form::new().text(
        "urls",
        "magnet:?xt=urn:btih:0000000000000000000000000000000000000000",
    );
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "torrents/add should require auth");

    let form_endpoints = [
        "/api/v2/torrents/pause",
        "/api/v2/torrents/resume",
        "/api/v2/torrents/delete",
        "/api/v2/torrents/setShareLimits",
    ];
    for path in &form_endpoints {
        let resp = client
            .post(format!("http://127.0.0.1:{port}{path}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body("hashes=abc")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{path} should require auth");
    }

    // GET endpoint
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/properties?hash=abc"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "torrents/properties should require auth"
    );

    engine.shutdown().await;
}

#[test]
fn test_torrent_record_backward_compat_completed_at() {
    // Old format without completed_at should deserialize with completed_at = None
    let json = r#"{
        "info_hash_hex": "abc123",
        "source": "magnet:?xt=...",
        "save_path": "/tmp",
        "ratio_target": 0.0,
        "added_at": 1000,
        "user_paused": false
    }"#;
    let record: TorrentRecord = serde_json::from_str(json).unwrap();
    assert!(
        record.completed_at.is_none(),
        "completed_at should default to None for old records"
    );
}

#[test]
fn test_torrent_record_completed_at_round_trip() {
    let record = TorrentRecord {
        info_hash_hex: "abc123".to_string(),
        source: "test".to_string(),
        save_path: "/tmp".to_string(),
        ratio_target: 1.0,
        added_at: 1000,
        user_paused: false,
        completed_at: Some(2000),
        category: String::new(),
        total_uploaded: 0,
        total_downloaded: 0,
        piece_bitfield: Vec::new(),
        file_mtime: None,
        file_size: None,
    };
    let json = serde_json::to_string(&record).unwrap();
    let deserialized: TorrentRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.completed_at, Some(2000));
}

#[tokio::test]
async fn test_added_on_timestamp() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let torrent_bytes = make_test_torrent(dl_dir.path());
    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    assert!(!info.is_empty(), "torrent should appear in info");

    let added_on = info[0]["added_on"]
        .as_u64()
        .expect("added_on should be a number");
    let diff = added_on.abs_diff(now);
    assert!(
        diff <= 5,
        "added_on should be within 5 seconds of current time, got diff={diff}"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_completion_timestamps() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap().to_string();

    // Before completion: completion_on should be 0, seeding_time should be 0
    let completion_on = info[0]["completion_on"].as_u64().unwrap();
    let seeding_time = info[0]["seeding_time"].as_i64().unwrap();
    assert_eq!(
        completion_on, 0,
        "completion_on should be 0 before completion"
    );
    assert_eq!(
        seeding_time, 0,
        "seeding_time should be 0 before completion"
    );

    // Simulate completion by writing completed_at to the registry
    let completed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 10;
    let info_hash_bytes: [u8; 20] = {
        let mut buf = [0u8; 20];
        for i in 0..20 {
            buf[i] = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16).unwrap();
        }
        buf
    };
    let mut record = engine
        .registry()
        .get(&info_hash_bytes)
        .unwrap()
        .expect("record should exist");
    record.completed_at = Some(completed_at);
    engine.registry().upsert(&info_hash_bytes, &record).unwrap();

    // Query again -- completion_on should be the timestamp, seeding_time > 0
    let info = get_torrents_info(&client, port, &sid).await;
    let completion_on = info[0]["completion_on"].as_u64().unwrap();
    let seeding_time = info[0]["seeding_time"].as_i64().unwrap();
    assert_eq!(
        completion_on, completed_at,
        "completion_on should match completed_at"
    );
    assert!(
        seeding_time >= 10,
        "seeding_time should be >= 10 seconds, got {seeding_time}"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_build_info_endpoint() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;

    let client = reqwest::Client::new();
    // No SID cookie -- buildInfo should work without auth
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/buildInfo"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["app"].as_str().unwrap().starts_with("lightorrent v"));
    assert!(body["bitness"].as_i64().is_some());
    assert!(body["qt"].as_str().is_some());
    assert!(body["libtorrent"].as_str().is_some());

    engine.shutdown().await;
}

#[tokio::test]
async fn test_category_tagging() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // Add torrent with category
    let torrent_bytes = make_test_torrent(dl_dir.path());
    let form = multipart::Form::new()
        .part(
            "torrents",
            multipart::Part::bytes(torrent_bytes).file_name("test.torrent"),
        )
        .text("category", "tv-sonarr");

    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .header("cookie", &sid)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify category in torrents/info
    let info = get_torrents_info(&client, port, &sid).await;
    assert_eq!(
        info[0]["category"].as_str().unwrap(),
        "tv-sonarr",
        "category should be tv-sonarr"
    );
    assert!(
        info[0].get("stopped").is_some(),
        "stopped field should be present"
    );

    // Change category via setCategory
    let hash = info[0]["hash"].as_str().unwrap();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/setCategory"
        ))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("hashes={hash}&category=radarr"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify category changed
    let info = get_torrents_info(&client, port, &sid).await;
    assert_eq!(
        info[0]["category"].as_str().unwrap(),
        "radarr",
        "category should be radarr after setCategory"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_create_category() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // Create a category
    let resp = client
        .post(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/createCategory"
        ))
        .header("cookie", &sid)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("category=tv-sonarr&savePath=tv")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify it appears in categories list
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/categories"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let cats: serde_json::Value = resp.json().await.unwrap();
    assert!(
        cats.get("tv-sonarr").is_some(),
        "tv-sonarr category should exist"
    );
    assert_eq!(cats["tv-sonarr"]["savePath"].as_str().unwrap(), "tv");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_pause_resume_registry_persistence() {
    let dl_dir = tempfile::tempdir().unwrap();
    let persist_dir = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let state_db = state_dir.path().join("state.redb");

    let make_cfg = || Config {
        download_dir: dl_dir.path().to_str().unwrap().to_string(),
        listen_port: 0,
        persistence_dir: persist_dir.path().to_str().unwrap().to_string(),
        torrents: None,
        api_bind_address: "127.0.0.1".to_string(),
        api_port: 0,
        api_username: "admin".to_string(),
        api_password: "adminadmin".to_string(),
        state_db_path: state_db.to_str().unwrap().to_string(),
    };

    // --- First engine: add torrent, pause it, verify user_paused in registry ---
    let cfg1 = make_cfg();
    let engine1 = Engine::new(&cfg1).await.expect("engine1 should start");
    let torrent_bytes = make_test_torrent(dl_dir.path());
    let hash = engine1
        .add_torrent_bytes(torrent_bytes, dl_dir.path().to_str().unwrap(), "", false)
        .await
        .expect("add torrent should succeed");

    // Wait for torrent to finish initializing
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify user_paused starts as false
    let records = engine1.registry().list().unwrap();
    assert_eq!(records.len(), 1);
    assert!(
        !records[0].1.user_paused,
        "user_paused should be false initially"
    );

    // Pause via engine
    engine1
        .pause_torrent(&hash)
        .await
        .expect("pause should succeed");

    // Verify user_paused is now true in registry
    let records = engine1.registry().list().unwrap();
    assert!(
        records[0].1.user_paused,
        "user_paused should be true after pause"
    );

    engine1.shutdown().await;
    drop(engine1);

    // --- Second engine: same DB, verify torrent comes back paused ---
    let cfg2 = make_cfg();
    let engine2 = Engine::new(&cfg2).await.expect("engine2 should start");

    // Registry should show user_paused = true after restart
    let records = engine2.registry().list().unwrap();
    assert_eq!(
        records.len(),
        1,
        "registry should have 1 record after restart"
    );
    assert!(
        records[0].1.user_paused,
        "user_paused should survive restart as true"
    );

    // Resume via engine
    engine2
        .resume_torrent(&hash)
        .await
        .expect("resume should succeed");

    // Verify user_paused is now false
    let records = engine2.registry().list().unwrap();
    assert!(
        !records[0].1.user_paused,
        "user_paused should be false after resume"
    );

    engine2.shutdown().await;
}

#[tokio::test]
async fn test_transfer_info_endpoint() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // With no torrents, all values should be 0
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/transfer/info"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["dl_info_speed"].as_u64().is_some(),
        "dl_info_speed should be numeric"
    );
    assert!(
        body["up_info_speed"].as_u64().is_some(),
        "up_info_speed should be numeric"
    );
    assert!(
        body["dl_info_data"].as_u64().is_some(),
        "dl_info_data should be numeric"
    );
    assert!(
        body["up_info_data"].as_u64().is_some(),
        "up_info_data should be numeric"
    );
    assert!(
        body["dht_nodes"].as_u64().is_some(),
        "dht_nodes should be numeric"
    );
    assert_eq!(body["dl_info_speed"].as_u64().unwrap(), 0);
    assert_eq!(body["up_info_speed"].as_u64().unwrap(), 0);
    assert_eq!(body["dl_info_data"].as_u64().unwrap(), 0);
    assert_eq!(body["up_info_data"].as_u64().unwrap(), 0);

    // Add a torrent and verify shape still holds
    let torrent_bytes = make_test_torrent(dl_dir.path());
    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/transfer/info"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["dl_info_speed"].as_u64().is_some(),
        "dl_info_speed should be numeric after add"
    );
    assert!(
        body["up_info_speed"].as_u64().is_some(),
        "up_info_speed should be numeric after add"
    );
    assert!(
        body["dl_info_data"].as_u64().is_some(),
        "dl_info_data should be numeric after add"
    );
    assert!(
        body["up_info_data"].as_u64().is_some(),
        "up_info_data should be numeric after add"
    );
    assert!(
        body["dht_nodes"].as_u64().is_some(),
        "dht_nodes should be numeric after add"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_transfer_info_requires_auth() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/transfer/info"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_info_category_filter() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // Add first torrent with category "sonarr"
    let torrent1 = make_test_torrent(dl_dir.path());
    let form = multipart::Form::new()
        .part(
            "torrents",
            multipart::Part::bytes(torrent1).file_name("test1.torrent"),
        )
        .text("category", "sonarr");
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .header("cookie", &sid)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Add second torrent (different .torrent file) with category "radarr"
    let torrent2 = make_test_torrent_2(dl_dir.path());
    let form = multipart::Form::new()
        .part(
            "torrents",
            multipart::Part::bytes(torrent2).file_name("test2.torrent"),
        )
        .text("category", "radarr");
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .header("cookie", &sid)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify both torrents present with no filter
    let all = get_torrents_info(&client, port, &sid).await;
    assert_eq!(all.len(), 2, "should have 2 torrents total");

    // Filter by category=sonarr
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/info?category=sonarr"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sonarr: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(sonarr.len(), 1, "category=sonarr should return 1 torrent");
    assert_eq!(sonarr[0]["category"].as_str().unwrap(), "sonarr");

    // Filter by category=radarr
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/info?category=radarr"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let radarr: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(radarr.len(), 1, "category=radarr should return 1 torrent");
    assert_eq!(radarr[0]["category"].as_str().unwrap(), "radarr");

    // Empty category param -> returns all
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/info?category="
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let empty_filter: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(
        empty_filter.len(),
        2,
        "empty category should return all torrents"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_sessions_coexist_and_logout() {
    let (_engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();

    // Login once -> get SID1
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=adminadmin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sid1 = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // Login again -> get SID2
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=adminadmin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sid2 = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    assert_ne!(sid1, sid2, "two logins should produce different SIDs");

    // Both SIDs are valid simultaneously
    for sid in [&sid1, &sid2] {
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/v2/app/version"))
            .header("cookie", sid)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "SID {sid} should be accepted");
    }

    // Logout SID1; SID2 still works
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/logout"))
        .header("cookie", &sid1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/version"))
        .header("cookie", &sid1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "logged-out SID should be rejected");

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/app/version"))
        .header("cookie", &sid2)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "other SID should still work");

    _engine.shutdown().await;
}

#[tokio::test]
async fn test_speed_fields_across_endpoints() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    // Get hash from /torrents/info
    let info = get_torrents_info(&client, port, &sid).await;
    assert!(!info.is_empty(), "torrent should appear in info");
    let t = &info[0];
    let hash = t["hash"].as_str().expect("hash should be a string");

    // /torrents/info: dlspeed and upspeed
    let dlspeed = &t["dlspeed"];
    let upspeed = &t["upspeed"];
    assert!(
        dlspeed.is_number(),
        "dlspeed should be a number, got: {dlspeed}"
    );
    assert!(
        upspeed.is_number(),
        "upspeed should be a number, got: {upspeed}"
    );
    assert!(dlspeed.as_i64().unwrap() >= 0, "dlspeed should be >= 0");
    assert!(upspeed.as_i64().unwrap() >= 0, "upspeed should be >= 0");

    // /torrents/properties?hash=X: dl_speed and up_speed
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/properties"
        ))
        .query(&[("hash", hash)])
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let props: serde_json::Value = resp.json().await.unwrap();
    let dl_speed = &props["dl_speed"];
    let up_speed = &props["up_speed"];
    assert!(
        dl_speed.is_number(),
        "dl_speed should be a number, got: {dl_speed}"
    );
    assert!(
        up_speed.is_number(),
        "up_speed should be a number, got: {up_speed}"
    );
    assert!(dl_speed.as_i64().unwrap() >= 0, "dl_speed should be >= 0");
    assert!(up_speed.as_i64().unwrap() >= 0, "up_speed should be >= 0");

    // /transfer/info: dl_info_speed and up_info_speed
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/transfer/info"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let transfer: serde_json::Value = resp.json().await.unwrap();
    let dl_info_speed = &transfer["dl_info_speed"];
    let up_info_speed = &transfer["up_info_speed"];
    assert!(
        dl_info_speed.is_number(),
        "dl_info_speed should be a number, got: {dl_info_speed}"
    );
    assert!(
        up_info_speed.is_number(),
        "up_info_speed should be a number, got: {up_info_speed}"
    );
    assert!(
        dl_info_speed.as_i64().unwrap() >= 0,
        "dl_info_speed should be >= 0"
    );
    assert!(
        up_info_speed.as_i64().unwrap() >= 0,
        "up_info_speed should be >= 0"
    );

    engine.shutdown().await;
}

fn hex_to_hash(hex: &str) -> [u8; 20] {
    let mut buf = [0u8; 20];
    for i in 0..20 {
        buf[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    buf
}

#[tokio::test]
async fn test_enforce_ratios_sets_completed_at() {
    // Test that completed_at is persisted in the registry and survives restart.
    // With magpie, completion is detected via TorrentComplete alerts from the
    // engine; we test the persistence path by manually setting completed_at.
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());
    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let info = get_torrents_info(&client, port, &sid).await;
    assert!(!info.is_empty(), "torrent should appear");
    let hash_hex = info[0]["hash"].as_str().unwrap();
    let hash = hex_to_hash(hash_hex);

    // Verify completed_at starts as None
    let rec = engine.registry().get(&hash).unwrap().expect("record");
    assert!(rec.completed_at.is_none(), "should start incomplete");

    // Simulate completion by setting completed_at in registry
    let mut rec = rec;
    rec.completed_at = Some(1000);
    engine.registry().upsert(&hash, &rec).unwrap();

    // Verify it round-trips
    let rec = engine.registry().get(&hash).unwrap().expect("record");
    assert_eq!(rec.completed_at, Some(1000), "completed_at should persist");

    engine.shutdown().await;
}

#[tokio::test]
async fn test_enforce_ratios_skips_user_paused() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent_bytes = make_test_torrent(dl_dir.path());

    add_torrent_file(&client, port, &sid, torrent_bytes).await;

    let info = get_torrents_info(&client, port, &sid).await;
    assert!(!info.is_empty(), "torrent should appear in info");
    let hash_hex = info[0]["hash"].as_str().unwrap();
    let hash = hex_to_hash(hash_hex);

    // Set user_paused and a tiny ratio_target on the registry record
    engine.registry().set_user_paused(&hash, true).unwrap();
    engine.registry().set_ratio_target(&hash, 0.001).unwrap();

    // Wait for engine to potentially process
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Verify engine skipped this torrent -- user_paused should still be true
    let record = engine
        .registry()
        .get(&hash)
        .unwrap()
        .expect("record should exist");
    assert!(
        record.user_paused,
        "engine should skip user-paused torrents, but user_paused was cleared"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn test_config_missing_file() {
    let result = Config::load("nonexistent_path_that_does_not_exist.toml");
    assert!(
        result.is_err(),
        "loading a missing config file should return Err"
    );
}

#[tokio::test]
async fn test_config_invalid_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "[[[bad").unwrap();
    let result = Config::load(path.to_str().unwrap());
    assert!(result.is_err(), "invalid TOML should return Err");
}

#[tokio::test]
async fn test_config_invalid_env_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.toml");
    std::fs::write(&path, r#"download_dir = "/tmp/test""#).unwrap();

    unsafe { std::env::set_var("LIGHTORRENT_LISTEN_PORT", "notanumber") };
    let result = Config::load(path.to_str().unwrap());
    unsafe { std::env::remove_var("LIGHTORRENT_LISTEN_PORT") };

    assert!(result.is_err(), "non-numeric LISTEN_PORT should return Err");
}

#[tokio::test]
async fn test_config_remaining_env_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.toml");
    std::fs::write(&path, r#"download_dir = "/tmp/test""#).unwrap();

    unsafe {
        std::env::set_var("LIGHTORRENT_API_USERNAME", "myuser");
        std::env::set_var("LIGHTORRENT_API_PASSWORD", "mypass");
        std::env::set_var("LIGHTORRENT_STATE_DB_PATH", "/tmp/custom.redb");
    }
    let cfg = Config::load(path.to_str().unwrap()).unwrap();
    unsafe {
        std::env::remove_var("LIGHTORRENT_API_USERNAME");
        std::env::remove_var("LIGHTORRENT_API_PASSWORD");
        std::env::remove_var("LIGHTORRENT_STATE_DB_PATH");
    }

    assert_eq!(cfg.api_username, "myuser");
    assert_eq!(cfg.api_password, "mypass");
    assert_eq!(cfg.state_db_path, "/tmp/custom.redb");
}

#[tokio::test]
async fn test_api_torrents_files_no_hash() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/torrents/files"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_files_bad_hash() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/torrents/files?hash=0000000000000000000000000000000000000000"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_api_torrents_properties_no_hash() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/api/v2/torrents/properties"
        ))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    engine.shutdown().await;
}

fn make_test_torrent_2(_dl_dir: &std::path::Path) -> Vec<u8> {
    let file_content = b"hello torrent 2";
    let piece_length: i64 = 16384;
    let digest = sha1_hash(file_content);
    let announce = b"http://tracker.invalid:6969";

    let mut info = Vec::new();
    info.push(b'd');
    info.extend_from_slice(&bencode_str(b"length"));
    info.extend_from_slice(format!("i{}e", file_content.len()).as_bytes());
    info.extend_from_slice(&bencode_str(b"name"));
    info.extend_from_slice(&bencode_str(b"testfile2.txt"));
    info.extend_from_slice(&bencode_str(b"piece length"));
    info.extend_from_slice(format!("i{}e", piece_length).as_bytes());
    info.extend_from_slice(&bencode_str(b"pieces"));
    info.extend_from_slice(&bencode_str(&digest));
    info.push(b'e');

    let mut torrent = Vec::new();
    torrent.push(b'd');
    torrent.extend_from_slice(&bencode_str(b"announce"));
    torrent.extend_from_slice(&bencode_str(announce));
    torrent.extend_from_slice(&bencode_str(b"info"));
    torrent.extend_from_slice(&info);
    torrent.push(b'e');

    torrent
}

#[tokio::test]
async fn test_transfer_info_multi_torrent_aggregation() {
    let (engine, port, _handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // Add two torrents with different content (-> different info_hashes)
    let torrent1 = make_test_torrent(dl_dir.path());
    add_torrent_file(&client, port, &sid, torrent1).await;
    let torrent2 = make_test_torrent_2(dl_dir.path());
    add_torrent_file(&client, port, &sid, torrent2).await;

    // Verify both torrents are visible
    let info = get_torrents_info(&client, port, &sid).await;
    assert_eq!(info.len(), 2, "should have 2 torrents");
    let hash1 = info[0]["hash"].as_str().unwrap();
    let hash2 = info[1]["hash"].as_str().unwrap();
    assert_ne!(hash1, hash2, "two torrents should have different hashes");

    // Verify transfer/info aggregation shape
    let resp = client
        .get(format!("http://127.0.0.1:{port}/api/v2/transfer/info"))
        .header("cookie", &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let transfer: serde_json::Value = resp.json().await.unwrap();
    assert!(
        transfer["dl_info_speed"].is_number(),
        "dl_info_speed should be numeric"
    );
    assert!(
        transfer["up_info_speed"].is_number(),
        "up_info_speed should be numeric"
    );
    assert!(
        transfer["dl_info_data"].is_number(),
        "dl_info_data should be numeric"
    );
    assert!(
        transfer["up_info_data"].is_number(),
        "up_info_data should be numeric"
    );

    engine.shutdown().await;
}

// -----------------------------------------------------------------------------
// Adversarial integration tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn adversarial_auth_brute_force_returns_429() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();

    // Burn through the failure budget (LOGIN_MAX_FAILURES = 10).
    for _ in 0..10 {
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
            .form(&[("username", "admin"), ("password", "wrong")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }
    // 11th attempt should trip the rate limiter.
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/auth/login"))
        .form(&[("username", "admin"), ("password", "wrong")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        429,
        "expected rate-limited after 10 failures"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn adversarial_multipart_field_too_large_returns_413() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    // 40 MiB payload in a single `torrents` field -- exceeds MULTIPART_FIELD_MAX_BYTES (32 MiB).
    let huge = vec![0u8; 40 * 1024 * 1024];
    let form = multipart::Form::new().part(
        "torrents",
        multipart::Part::bytes(huge).file_name("big.torrent"),
    );
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
        .header("cookie", &sid)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413, "oversized field should trigger 413");

    engine.shutdown().await;
}

#[tokio::test]
async fn adversarial_category_save_path_traversal_rejected() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    for sp in &["../etc", "../../etc/passwd", "/etc/passwd"] {
        let body = format!("category=evil&savePath={sp}");
        let resp = client
            .post(format!(
                "http://127.0.0.1:{port}/api/v2/torrents/createCategory"
            ))
            .header("cookie", &sid)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "savePath={sp} should be rejected");
    }

    engine.shutdown().await;
}

#[tokio::test]
async fn adversarial_hash_validation_non_hex_rejected() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    for bad in &[
        "zzzz",
        "../../etc/passwd",
        "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
    ] {
        let url = format!("http://127.0.0.1:{port}/api/v2/torrents/properties?hash={bad}");
        let resp = client
            .get(&url)
            .header("cookie", &sid)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "non-hex hash {bad} should 400");

        let url = format!("http://127.0.0.1:{port}/api/v2/torrents/files?hash={bad}");
        let resp = client
            .get(&url)
            .header("cookie", &sid)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "non-hex hash {bad} should 400 on files");
    }

    engine.shutdown().await;
}

#[tokio::test]
async fn adversarial_add_url_ssrf_rejected() {
    let (engine, port, _handle, _dl, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;

    let forbidden = [
        "http://127.0.0.1/x.torrent",
        "http://10.0.0.5/x.torrent",
        "http://192.168.1.1/x.torrent",
        "http://169.254.169.254/latest/meta-data",
        "http://localhost/x.torrent",
        "file:///etc/passwd",
        "ftp://example.com/x.torrent",
    ];
    for url in &forbidden {
        let form = multipart::Form::new().text("urls", url.to_string());
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v2/torrents/add"))
            .header("cookie", &sid)
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "forbidden URL {url} should be 400");
    }

    engine.shutdown().await;
}

#[tokio::test]
async fn adversarial_shutdown_preserves_uploaded_bytes() {
    // Drive the engine, then trigger shutdown mid-tick via cancellation.
    // total_uploaded must not regress across restart.
    let (engine, port, handle, dl_dir, _persist) = start_test_server().await;
    let client = reqwest::Client::new();
    let sid = login(&client, port).await;
    let torrent = make_test_torrent(dl_dir.path());
    add_torrent_file(&client, port, &sid, torrent).await;

    // Let the engine run briefly.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let info = get_torrents_info(&client, port, &sid).await;
    let hash = info[0]["hash"].as_str().unwrap().to_string();

    // Force a non-zero uploaded into the registry so we can verify persistence.
    let info_hash: [u8; 20] = {
        let mut b = [0u8; 20];
        for i in 0..20 {
            b[i] = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16).unwrap();
        }
        b
    };
    let mut rec = engine
        .registry()
        .get(&info_hash)
        .unwrap()
        .expect("record exists");
    rec.total_uploaded = 12_345;
    engine.registry().upsert(&info_hash, &rec).unwrap();
    let baseline = rec.total_uploaded;
    let db_path = dl_dir
        .path()
        .join("state.redb")
        .to_str()
        .unwrap()
        .to_string();

    // Shutdown mid-tick; redb writes are synchronous so the committed value
    // must still be visible on reopen. We must also stop the axum server --
    // it holds the router -> AppState -> Arc<Engine> -> Arc<TorrentRegistry>,
    // which keeps the redb lock held even after engine.shutdown().
    // Drop the reqwest client first so all keep-alive connections close
    // and axum's per-connection futures resolve. Then shut down the engine,
    // abort the server, and explicitly drop the Arc<Engine> so the redb
    // lock held by Arc<TorrentRegistry> is released.
    drop(client);
    engine.shutdown().await;
    handle.abort();
    let _ = handle.await;
    drop(engine);
    // Give Tokio a chance to run drop futures across all worker threads.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Reopen registry and verify monotonicity.
    let reopened = lightorrent::store::TorrentRegistry::open(&db_path).unwrap();
    let got = reopened
        .get(&info_hash)
        .unwrap()
        .expect("record must persist");
    assert!(
        got.total_uploaded >= baseline,
        "uploaded regressed across shutdown: before={baseline}, after={}",
        got.total_uploaded
    );
}

#[tokio::test]
async fn test_fast_resume_bitfield_persistence() {
    let dl_dir = tempfile::tempdir().unwrap();
    let persist_dir = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let state_db = state_dir.path().join("state.redb");

    let cfg = Config {
        download_dir: dl_dir.path().to_str().unwrap().to_string(),
        listen_port: 0,
        persistence_dir: persist_dir.path().to_str().unwrap().to_string(),
        torrents: None,
        api_bind_address: "127.0.0.1".to_string(),
        api_port: 0,
        api_username: "admin".to_string(),
        api_password: "adminadmin".to_string(),
        state_db_path: state_db.to_str().unwrap().to_string(),
    };

    let engine = Engine::new(&cfg).await.expect("engine should start");
    let torrent_bytes = make_test_torrent(dl_dir.path());
    let hash_hex = engine
        .add_torrent_bytes(torrent_bytes, dl_dir.path().to_str().unwrap(), "", false)
        .await
        .expect("add torrent should succeed");

    let info_hash = hex_to_hash(&hash_hex);

    // Manually set a partial bitfield + file metadata in the registry
    let mut record = engine
        .registry()
        .get(&info_hash)
        .unwrap()
        .expect("record should exist");
    let partial_bitfield = vec![0x80u8]; // first piece marked as have (MSB-first)
    record.piece_bitfield = partial_bitfield.clone();
    record.file_mtime = Some(1_700_000_000);
    record.file_size = Some(13); // length of "hello torrent"
    engine.registry().upsert(&info_hash, &record).unwrap();

    // Verify registry has the bitfield stored
    let stored = engine
        .registry()
        .get(&info_hash)
        .unwrap()
        .expect("record should still exist");
    assert_eq!(
        stored.piece_bitfield, partial_bitfield,
        "bitfield should be persisted in registry"
    );
    assert_eq!(
        stored.file_mtime,
        Some(1_700_000_000),
        "file_mtime should be persisted"
    );
    assert_eq!(stored.file_size, Some(13), "file_size should be persisted");

    engine.shutdown().await;
}
