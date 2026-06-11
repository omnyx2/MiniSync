//! Global application configuration, stored at ~/.config/minisync/app.toml
//!
//! This is separate from the per-folder sync config (.minisync/config.toml).
//! It persists the sync folder path, listen address, and peer addresses
//! so the app can be launched without CLI arguments.
//!
//! Example ~/.config/minisync/app.toml:
//! ```toml
//! sync_folder = "/Users/me/Sync"
//! listen_addr = "0.0.0.0:9000"
//! peers = ["10.0.0.2:9001", "10.0.0.3:9002"]
//! ```

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Global app configuration (persisted between launches).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Path to the sync folder.
    pub sync_folder: String,
    /// Listen address (e.g. "0.0.0.0:9000").
    pub listen_addr: String,
    /// List of peer addresses to connect to.
    #[serde(default)]
    pub peers: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            sync_folder: String::new(),
            listen_addr: "0.0.0.0:9000".to_string(),
            peers: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Get the path to the global config file (~/.config/minisync/app.toml).
    pub fn config_path() -> PathBuf {
        if let Some(config_dir) = dirs_path() {
            config_dir.join("app.toml")
        } else {
            PathBuf::from("app.toml")
        }
    }

    /// Get the config directory (~/.config/minisync/).
    pub fn config_dir() -> PathBuf {
        dirs_path().unwrap_or_else(|| PathBuf::from("."))
    }

    /// Load from ~/.config/minisync/app.toml. Returns None if file doesn't exist.
    pub fn load() -> Option<Self> {
        let path = Self::config_path();
        let content = fs::read_to_string(&path).ok()?;
        toml::from_str(&content).ok()
    }

    /// Save to ~/.config/minisync/app.toml.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {e}"))?;
        }
        let content =
            toml::to_string_pretty(self).map_err(|e| format!("Failed to serialize: {e}"))?;
        fs::write(&path, content).map_err(|e| format!("Failed to write config: {e}"))?;
        Ok(())
    }

    /// Check if the config has a valid sync folder set.
    pub fn has_sync_folder(&self) -> bool {
        !self.sync_folder.is_empty() && Path::new(&self.sync_folder).exists()
    }
}

/// Platform-specific config directory.
fn dirs_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".config").join("minisync"))
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(|d| PathBuf::from(d).join("minisync"))
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config").join("minisync"))
            })
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|d| PathBuf::from(d).join("minisync"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".config").join("minisync"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.listen_addr, "0.0.0.0:9000");
        assert!(cfg.sync_folder.is_empty());
        assert!(cfg.peers.is_empty());
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = AppConfig {
            sync_folder: "/tmp/test".to_string(),
            listen_addr: "0.0.0.0:8000".to_string(),
            peers: vec!["10.0.0.1:9000".to_string()],
        };
        let s = toml::to_string_pretty(&cfg).unwrap();
        let loaded: AppConfig = toml::from_str(&s).unwrap();
        assert_eq!(loaded.sync_folder, "/tmp/test");
        assert_eq!(loaded.listen_addr, "0.0.0.0:8000");
        assert_eq!(loaded.peers, vec!["10.0.0.1:9000"]);
    }

    #[test]
    fn config_path_exists() {
        let path = AppConfig::config_path();
        // Just check it doesn't panic and returns something
        assert!(!path.as_os_str().is_empty());
    }
}
