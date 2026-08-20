//! Config loading: ~/.config/minimax-quota/config.json (created from
//! config.example.json on first run by install.sh). Defaults are baked
//! in so the tray can boot before the file exists.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::burn::BurnConfig;

const CONFIG_DIR_NAME: &str = ".config/minimax-quota";
const CONFIG_FILENAME: &str = "config.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanConfig {
    pub endpoint: String,
    pub dashboard_url: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Thresholds {
    /// Yellow when used% >= this.
    pub yellow: i64,
    /// Red when used% >= this.
    pub red: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub plan: String,
    pub refresh_seconds: u64,
    pub refresh_max_backoff_seconds: u64,

    #[serde(default)]
    pub plans: std::collections::HashMap<String, PlanConfig>,

    #[serde(default)]
    pub thresholds: Thresholds,

    #[serde(default)]
    pub burn_warning: BurnConfig,
}

impl Default for Config {
    fn default() -> Self {
        let mut plans = std::collections::HashMap::new();
        plans.insert(
            "coding_plan".to_string(),
            PlanConfig {
                endpoint: "https://api.minimax.chat/v1/coding_plan/remains".to_string(),
                dashboard_url: "https://MiniMax.io/platform/code-plan".to_string(),
                label: "MiniMax Coding Plan".to_string(),
            },
        );
        Self {
            plan: "coding_plan".to_string(),
            refresh_seconds: 120,
            refresh_max_backoff_seconds: 600,
            plans,
            thresholds: Thresholds { yellow: 60, red: 85 },
            burn_warning: BurnConfig::default(),
        }
    }
}

/// `~/.config/minimax-quota/config.json` — uses HOME env var to avoid
/// pulling in the dirs crate at config-load time.
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(CONFIG_DIR_NAME).join(CONFIG_FILENAME)
}

/// Load config from disk; missing or malformed → defaults.
pub fn load() -> Config {
    let path = config_path();
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Config>(&bytes) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("config at {} is malformed ({e}); using defaults", path.display());
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

/// Load and save defaults to disk if no config exists yet.
pub fn load_or_init() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        let cfg = Config::default();
        std::fs::create_dir_all(path.parent().context("config path has no parent")?)
            .context("creating config dir")?;
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg)?)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        log::info!("wrote default config at {}", path.display());
        return Ok(cfg);
    }
    Ok(load())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The config tests all mutate the global HOME env var. Serialize them so
    /// `cargo test` can run other tests in parallel without interference.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Test helper: write a config to a custom path and read it back via load_with().
    fn write_at(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    /// Variant of load() that reads from a given path — used in tests so we
    /// don't fight HOME/serialization.
    fn load_at(path: &std::path::Path) -> Config {
        let bytes = std::fs::read(path).unwrap();
        match serde_json::from_slice::<Config>(&bytes) {
            Ok(c) => c,
            Err(e) => panic!("load_at failed for {}: {e}", path.display()),
        }
    }

    /// Acquire the test lock; all config tests should `let _g = lock_home();`
    /// at the top so they don't stomp each other's HOME.
    fn lock_home() -> std::sync::MutexGuard<'static, ()> {
        HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn defaults_when_file_does_not_exist() {
        let _g = lock_home();
        // load() with HOME pointed at empty dir returns defaults.
        let tmp = std::env::temp_dir().join("minimax-cfg-empty");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", &tmp);
        let cfg = load();
        assert_eq!(cfg.plan, "coding_plan");
        assert_eq!(cfg.refresh_seconds, 120);
        assert!(cfg.plans.contains_key("coding_plan"));
        std::env::remove_var("HOME");
    }

    #[test]
    fn roundtrip() {
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-roundtrip");
        let path = tmp.join("config.json");
        write_at(&path, r#"{
            "plan": "token_plan",
            "refresh_seconds": 60,
            "refresh_max_backoff_seconds": 600,
            "plans": {
                "token_plan": {
                    "endpoint": "https://example.invalid/token",
                    "dashboard_url": "https://example.invalid/dash",
                    "label": "Test Token Plan"
                }
            },
            "thresholds": {"yellow": 50, "red": 80},
            "burn_warning": {
                "enabled": false,
                "min_history_ms": 1,
                "lookback_ms": 60000,
                "use_epoch_average": false
            }
        }"#);
        let cfg = load_at(&path);
        assert_eq!(cfg.plan, "token_plan");
        assert_eq!(cfg.refresh_seconds, 60);
        assert_eq!(cfg.thresholds.yellow, 50);
        assert!(!cfg.burn_warning.enabled);
        assert!(!cfg.burn_warning.use_epoch_average);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn malformed_falls_back_to_defaults() {
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-malformed");
        let path = tmp.join("config.json");
        write_at(&path, "this is not json");
        std::env::set_var("HOME", &tmp);
        let cfg = load();
        assert_eq!(cfg.plan, "coding_plan");  // defaults
        std::env::remove_var("HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_or_init_writes_defaults_when_missing() {
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-init");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", &tmp);
        let cfg = load_or_init().unwrap();
        assert!(config_path().exists(), "config should have been written");
        assert_eq!(cfg.plan, "coding_plan");
        std::env::remove_var("HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_or_init_does_not_overwrite_existing() {
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-existing");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", &tmp);
        write_at(&config_path(), r#"{
            "plan": "token_plan",
            "refresh_seconds": 30,
            "refresh_max_backoff_seconds": 120,
            "plans": {
                "token_plan": {
                    "endpoint": "https://example.invalid/token",
                    "dashboard_url": "https://example.invalid/dash",
                    "label": "Test Token Plan"
                }
            },
            "thresholds": {"yellow": 40, "red": 70},
            "burn_warning": {
                "enabled": true,
                "min_history_ms": 1,
                "lookback_ms": 60000,
                "use_epoch_average": true
            }
        }"#);
        let cfg = load_or_init().unwrap();
        assert_eq!(cfg.plan, "token_plan");  // preserved
        std::env::remove_var("HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}