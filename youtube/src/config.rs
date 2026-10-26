//! Identity that must survive restarts so paired phones keep finding the speaker.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub uuid: String,
    pub screen_id: String,
    /// Screen ID for the YouTube Music ("m" theme) Lounge session.
    #[serde(default)]
    pub screen_id_m: String,
    #[serde(skip)]
    path: PathBuf,
}

impl Config {
    /// Loads `config.json` from `dir`, or starts a fresh identity.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join("config.json");
        let mut config = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                tracing::warn!("[config] ignoring unreadable {}: {e}", path.display());
                Config::default()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e),
        };
        config.path = path;
        if config.uuid.is_empty() {
            config.uuid = uuid::Uuid::new_v4().to_string();
            config.save()?;
        }
        Ok(config)
    }

    /// Writes atomically so a power cut never leaves a half-written identity.
    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &self.path)
    }
}
