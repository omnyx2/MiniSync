//! Sync configuration: mode selection (FullCopy vs Reference) and pattern rules.
//!
//! Configuration is stored in `.minisync/config.toml` within the sync root.
//!
//! The default mode is `reference` (selective sync: peers see each other's files
//! as metadata-only references and download on demand). Per-pattern rules can opt
//! specific paths into `full_copy` (mirror the bytes automatically).
//!
//! Example:
//! ```toml
//! default_mode = "reference"
//! [[rules]]
//! pattern = "src/**"
//! mode = "full_copy"
//! ```

pub mod app;
pub mod rules;

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::routing;
use rules::glob_match;

/// How a file should be synchronized across peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Full file contents are replicated to all peers.
    FullCopy,
    /// Only metadata is shared; file stays on the original node.
    Reference,
}

/// A single pattern→mode rule. Rules are evaluated top-down; first match wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRule {
    pub pattern: String,
    pub mode: SyncMode,
}

/// The complete sync configuration for a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub default_mode: SyncMode,
    #[serde(default)]
    pub rules: Vec<SyncRule>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            // Selective sync by default: File-lane files (media, documents, etc.)
            // are shared as references and only materialize on a node when the
            // user downloads them. (CRDT text/code files still sync fully.)
            default_mode: SyncMode::Reference,
            rules: Vec::new(),
        }
    }
}

impl SyncConfig {
    /// Determine the sync mode for a given relative path.
    /// Rules are evaluated top-down; first matching pattern wins.
    /// Falls back to `default_mode` if no rule matches.
    pub fn mode_for(&self, rel_path: &str) -> SyncMode {
        for rule in &self.rules {
            if glob_match(&rule.pattern, rel_path) {
                return rule.mode;
            }
        }
        self.default_mode
    }

    /// Load configuration from `.minisync/config.toml`.
    /// Returns default config if file doesn't exist or is invalid.
    pub fn load(root: &Path) -> Self {
        let path = root.join(routing::MINISYNC_DIR).join("config.toml");
        match fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => SyncConfig::default(),
        }
    }

    /// Save configuration to `.minisync/config.toml`.
    pub fn save(&self, root: &Path) {
        let dir = root.join(routing::MINISYNC_DIR);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        if let Ok(content) = toml::to_string_pretty(self) {
            let _ = fs::write(&path, content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = SyncConfig::default();
        // Selective sync default: Reference.
        assert_eq!(cfg.default_mode, SyncMode::Reference);
        assert_eq!(cfg.mode_for("anything.png"), SyncMode::Reference);
    }

    #[test]
    fn pattern_matching() {
        let cfg = SyncConfig {
            default_mode: SyncMode::FullCopy,
            rules: vec![
                SyncRule {
                    pattern: "*.pdf".to_string(),
                    mode: SyncMode::Reference,
                },
                SyncRule {
                    pattern: "src/**".to_string(),
                    mode: SyncMode::FullCopy,
                },
            ],
        };
        assert_eq!(cfg.mode_for("report.pdf"), SyncMode::Reference);
        assert_eq!(cfg.mode_for("src/main.rs"), SyncMode::FullCopy);
        assert_eq!(cfg.mode_for("image.png"), SyncMode::FullCopy);
    }

    #[test]
    fn first_match_wins() {
        let cfg = SyncConfig {
            default_mode: SyncMode::Reference,
            rules: vec![
                SyncRule {
                    pattern: "*.rs".to_string(),
                    mode: SyncMode::FullCopy,
                },
                SyncRule {
                    pattern: "*.rs".to_string(),
                    mode: SyncMode::Reference,
                },
            ],
        };
        assert_eq!(cfg.mode_for("main.rs"), SyncMode::FullCopy);
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = SyncConfig {
            default_mode: SyncMode::FullCopy,
            rules: vec![
                SyncRule {
                    pattern: "*.pdf".to_string(),
                    mode: SyncMode::Reference,
                },
            ],
        };
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let deserialized: SyncConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.default_mode, SyncMode::FullCopy);
        assert_eq!(deserialized.rules.len(), 1);
        assert_eq!(deserialized.rules[0].pattern, "*.pdf");
        assert_eq!(deserialized.rules[0].mode, SyncMode::Reference);
    }

    #[test]
    fn load_save_roundtrip() {
        let dir = std::env::temp_dir().join("minisync_config_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = SyncConfig {
            default_mode: SyncMode::Reference,
            rules: vec![SyncRule {
                pattern: "src/**".to_string(),
                mode: SyncMode::FullCopy,
            }],
        };
        cfg.save(&dir);
        let loaded = SyncConfig::load(&dir);
        assert_eq!(loaded.default_mode, SyncMode::Reference);
        assert_eq!(loaded.rules.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
