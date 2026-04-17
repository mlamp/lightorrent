use serde::Deserialize;
use std::env;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub download_dir: String,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default = "default_persistence_dir")]
    pub persistence_dir: String,
    pub torrents: Option<Vec<String>>,
    #[serde(default = "default_api_bind_address")]
    pub api_bind_address: String,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_api_username")]
    pub api_username: String,
    #[serde(default = "default_api_password")]
    pub api_password: String,
    #[serde(default = "default_state_db_path")]
    pub state_db_path: String,
}

fn default_listen_port() -> u16 {
    8080
}

fn default_persistence_dir() -> String {
    "./data/session".to_string()
}

fn default_api_bind_address() -> String {
    "0.0.0.0".to_string()
}

fn default_api_port() -> u16 {
    8181
}

fn default_api_username() -> String {
    "admin".to_string()
}

fn default_api_password() -> String {
    "adminadmin".to_string()
}

fn default_state_db_path() -> String {
    "./data/state.redb".to_string()
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Config> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config file '{}': {}", path, e))?;
        let mut config: Config = toml::from_str(&contents)?;

        if let Ok(val) = env::var("LIGHTORRENT_DOWNLOAD_DIR") {
            config.download_dir = val;
        }
        if let Ok(val) = env::var("LIGHTORRENT_LISTEN_PORT") {
            config.listen_port = val.parse()?;
        }
        if let Ok(val) = env::var("LIGHTORRENT_PERSISTENCE_DIR") {
            config.persistence_dir = val;
        }
        if let Ok(val) = env::var("LIGHTORRENT_API_BIND_ADDRESS") {
            config.api_bind_address = val;
        }
        if let Ok(val) = env::var("LIGHTORRENT_API_PORT") {
            config.api_port = val.parse()?;
        }
        if let Ok(val) = env::var("LIGHTORRENT_API_USERNAME") {
            config.api_username = val;
        }
        if let Ok(val) = env::var("LIGHTORRENT_API_PASSWORD") {
            config.api_password = val;
        }
        // PHC-encoded hash takes precedence so operators can inject the hash
        // directly without leaking plaintext via the env.
        if let Ok(val) = env::var("LIGHTORRENT_API_PASSWORD_HASH") {
            config.api_password = val;
        }
        if let Ok(val) = env::var("LIGHTORRENT_STATE_DB_PATH") {
            config.state_db_path = val;
        }
        Ok(config)
    }
}
