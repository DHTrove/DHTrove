use dhtrove_crawler::CrawlerConfig;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub udp_port: u16,
    pub max_inflight_queries: usize,
    pub query_interval_ms: u64,
    pub max_active_fetches: usize,
    pub max_queued_hashes: usize,
    pub max_peers_per_torrent: usize,
    pub metadata_limit_mb: usize,
    pub checkpoint_every: usize,
    pub max_disk_gb: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            udp_port: 6881,
            max_inflight_queries: 64,
            query_interval_ms: 200,
            max_active_fetches: 32,
            max_queued_hashes: 32_768,
            max_peers_per_torrent: 12,
            metadata_limit_mb: 4,
            checkpoint_every: 2048,
            max_disk_gb: None,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.udp_port == 0 {
            return Err("UDP port must be non-zero");
        }
        if !(1..=512).contains(&self.max_inflight_queries) {
            return Err("DHT concurrent queries must be 1–512");
        }
        if !(50..=60_000).contains(&self.query_interval_ms) {
            return Err("DHT query interval must be 50–60000 ms");
        }
        if !(1..=256).contains(&self.max_active_fetches) {
            return Err("Metadata workers must be 1–256");
        }
        if !(128..=200_000).contains(&self.max_queued_hashes) {
            return Err("Pending hash capacity must be 128–200000");
        }
        if !(1..=64).contains(&self.max_peers_per_torrent) {
            return Err("Peers per torrent must be 1–64");
        }
        if !(1..=16).contains(&self.metadata_limit_mb) {
            return Err("Metadata limit must be 1–16 MiB");
        }
        if self.checkpoint_every > 1_000_000 {
            return Err("Checkpoint interval must be 0–1000000");
        }
        if self
            .max_disk_gb
            .is_some_and(|gb| !(1..=1_000_000).contains(&gb))
        {
            return Err("Disk limit must be 1–1000000 GiB or empty");
        }
        Ok(())
    }

    pub fn crawler(&self, directory: &Path) -> CrawlerConfig {
        let mut config = CrawlerConfig::default();
        config.store_path = directory.join("store");
        config.dht.bind.set_port(self.udp_port);
        config.dht.max_inflight_queries = self.max_inflight_queries;
        config.dht.query_interval = std::time::Duration::from_millis(self.query_interval_ms);
        config.max_active_fetches = self.max_active_fetches;
        config.max_queued_hashes = self.max_queued_hashes;
        config.max_peers_per_torrent = self.max_peers_per_torrent;
        config.fetch.max_metadata_bytes = self.metadata_limit_mb * 1024 * 1024;
        config.fetch.max_frame_bytes = config.fetch.max_metadata_bytes + 2_048;
        config.checkpoint_every = self.checkpoint_every;
        config.store.max_disk_bytes = self.max_disk_gb.map(|gb| gb * 1024 * 1024 * 1024);
        config
    }
}

pub fn load(path: &Path) -> anyhow::Result<Settings> {
    match fs::read(path) {
        Ok(bytes) => {
            let settings: Settings = serde_json::from_slice(&bytes)?;
            settings.validate().map_err(anyhow::Error::msg)?;
            Ok(settings)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(err) => Err(err.into()),
    }
}

pub fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    // Atomic rename prevents a crash from leaving a partially written config.
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        use std::io::Write;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_valid_and_disabled_limit_is_explicit() {
        let cfg = Settings::default();
        assert!(cfg.validate().is_ok());
        assert!(cfg
            .crawler(Path::new("/tmp/test"))
            .store
            .max_disk_bytes
            .is_none());
    }
    #[test]
    fn unsafe_settings_rejected() {
        let mut cfg = Settings::default();
        cfg.max_active_fetches = 0;
        assert!(cfg.validate().is_err());
    }
}
