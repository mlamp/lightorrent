use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tracing::info;

const TORRENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("torrents");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentRecord {
    pub info_hash_hex: String,
    pub source: String,
    pub save_path: String,
    pub ratio_target: f64,
    pub added_at: u64,
    pub user_paused: bool,
    #[serde(default)]
    pub completed_at: Option<u64>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub total_uploaded: u64,
    #[serde(default)]
    pub total_downloaded: u64,
    #[serde(default)]
    pub piece_bitfield: Vec<u8>,
    #[serde(default)]
    pub file_mtime: Option<u64>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

pub struct TorrentRegistry {
    db: Database,
}

impl TorrentRegistry {
    pub fn open(path: &str) -> Result<Self> {
        let db =
            Database::create(path).with_context(|| format!("failed to open registry at {path}"))?;
        {
            let txn = db.begin_write()?;
            txn.open_table(TORRENTS)?;
            txn.commit()?;
        }
        info!(path, "torrent registry opened");
        Ok(Self { db })
    }

    pub fn upsert(&self, info_hash: &[u8; 20], record: &TorrentRecord) -> Result<()> {
        let value = serde_json::to_vec(record)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TORRENTS)?;
            table.insert(info_hash.as_slice(), value.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get(&self, info_hash: &[u8; 20]) -> Result<Option<TorrentRecord>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TORRENTS)?;
        match table.get(info_hash.as_slice())? {
            Some(guard) => {
                let record: TorrentRecord = serde_json::from_slice(guard.value())?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    pub fn remove(&self, info_hash: &[u8; 20]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TORRENTS)?;
            table.remove(info_hash.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<([u8; 20], TorrentRecord)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TORRENTS)?;
        let mut results = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let hash: [u8; 20] = key
                .value()
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid key length in registry"))?;
            let record: TorrentRecord = serde_json::from_slice(value.value())?;
            results.push((hash, record));
        }
        Ok(results)
    }

    /// Atomic read-modify-write: reads the record, applies `f`, and writes
    /// back within a single redb write transaction (H4 fix).
    fn modify(&self, info_hash: &[u8; 20], f: impl FnOnce(&mut TorrentRecord)) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TORRENTS)?;
            let guard = table
                .get(info_hash.as_slice())?
                .ok_or_else(|| anyhow::anyhow!("record not found for info_hash"))?;
            let mut record: TorrentRecord = serde_json::from_slice(guard.value())?;
            drop(guard);
            f(&mut record);
            let value = serde_json::to_vec(&record)?;
            table.insert(info_hash.as_slice(), value.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn set_ratio_target(&self, info_hash: &[u8; 20], ratio: f64) -> Result<()> {
        self.modify(info_hash, |rec| rec.ratio_target = ratio)
    }

    pub fn set_user_paused(&self, info_hash: &[u8; 20], paused: bool) -> Result<()> {
        self.modify(info_hash, |rec| rec.user_paused = paused)
    }

    pub fn set_category(&self, info_hash: &[u8; 20], category: &str) -> Result<()> {
        self.modify(info_hash, |rec| rec.category = category.to_string())
    }
}
