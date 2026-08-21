//! Config loading: each instance's `config.json` (path derived from
//! `--instance=<name>`, defaulting to `~/.config/llm-quota-tray/`).
//!
//! The config is the source of truth for everything per-instance:
//! endpoint, label, dashboard URL, JSON shape, ring colors, auth
//! style, User-Agent prefix. Defaults are baked in so the tray can
//! boot before the file exists.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::burn::BurnConfig;
use crate::provider::{
    default_ring_colors, default_shape, AuthConfig, PlanShape, RingColors,
    DEFAULT_USER_AGENT,
};

/// Default config dir basename (no instance suffix). Neutral — no
/// provider name baked in. Per-instance names append `-<name>`:
/// `llm-quota-tray-coding`, `llm-quota-tray-openai`, etc.
pub const CONFIG_DIR_BASE: &str = "llm-quota-tray";
pub const CONFIG_FILENAME: &str = "config.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The API endpoint to GET. Per-instance.
    pub endpoint: String,
    /// The URL opened by the **Open dashboard** menu item.
    pub dashboard_url: String,
    /// Human-readable label, shown in the chip and menu header.
    pub label: String,

    /// JSON shape of the response. Per-instance — every provider
    /// has different field names and unit conversions.
    #[serde(default = "default_shape")]
    pub shape: PlanShape,

    /// Ring colors for normal/warning/throttled states. Defaults
    /// to the classic green/yellow/red if omitted.
    #[serde(default = "default_ring_colors")]
    pub ring_colors: RingColors,

    /// How the API key is sent. Defaults to Bearer.
    #[serde(default)]
    pub auth: AuthConfig,

    /// User-Agent prefix (version auto-appended). Defaults to
    /// `llm-quota-tray`.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,

    pub refresh_seconds: u64,
    /// Floor for the polling cadence (gjs `refresh_min_seconds`,
    /// default 15). The adaptive cut (yellow/2, red/4) plus the
    /// `max_backoff_seconds` backoff never push the interval below
    /// this — protects the API from rapid-fire polling when usage
    /// is near-exhausted.
    #[serde(default = "default_refresh_min_seconds")]
    pub refresh_min_seconds: u64,
    pub refresh_max_backoff_seconds: u64,

    #[serde(default)]
    pub thresholds: Thresholds,

    #[serde(default)]
    pub burn_warning: BurnConfig,

    /// Optional URL to a per-model pricing endpoint (OpenRouter uses
    /// `https://openrouter.ai/api/v1/models`, fully public). When set,
    /// the tray fetches the table at startup and re-fetches every
    /// `pricing_refresh_polls` polls (default: once at startup).
    ///
    /// The expected wire shape is `{ "data": [ { "id": "...",
    /// "pricing": { "prompt": "...", "completion": "...",
    /// "input_cache_read": "..." } }, ... ] }` — see `src/pricing.rs`.
    /// Any provider exposing the same shape (or a thin adapter that
    /// projects onto it) works without further config.
    ///
    /// When unset, no pricing fetch happens and no `· $X/h` cost
    /// fragment is appended to burn rows — same behavior as before
    /// this field was added.
    #[serde(default)]
    pub pricing_endpoint: Option<String>,

    /// How often (in successful polls) to re-fetch the pricing
    /// endpoint. `None` means "fetch once at startup, never refresh".
    /// Set to e.g. `Some(60)` to re-fetch hourly at the default
    /// 60s poll cadence.
    ///
    /// Refresh failures are best-effort: a failed re-fetch keeps
    /// the previous table in place and just logs a warning.
    #[serde(default)]
    pub pricing_refresh_polls: Option<u64>,
}

fn default_user_agent() -> String {
    DEFAULT_USER_AGENT.to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Thresholds {
    /// Yellow when used% >= this.
    pub yellow: i64,
    /// Red when used% >= this.
    pub red: i64,
}

impl Default for Config {
    fn default() -> Self {
        // A neutral starting point: the compile-time defaults for
        // shape, ring colors, auth, and user-agent, plus reasonable
        // placeholder values for endpoint/label/dashboard_url. The
        // user is expected to override endpoint + label + shape in
        // their config.json — `load_or_init` writes this default and
        // the user customizes.
        Self {
            endpoint: String::new(),
            dashboard_url: String::new(),
            label: String::from("Quota"),
            shape: default_shape(),
            ring_colors: default_ring_colors(),
            auth: AuthConfig::default(),
            user_agent: default_user_agent(),
            refresh_seconds: 120,
            refresh_min_seconds: default_refresh_min_seconds(),
            refresh_max_backoff_seconds: 600,
            thresholds: Thresholds { yellow: 60, red: 85 },
            burn_warning: BurnConfig::default(),
            pricing_endpoint: None,
            pricing_refresh_polls: None,
        }
    }
}

/// Default for `refresh_min_seconds` — matches the gjs default.
fn default_refresh_min_seconds() -> u64 { 15 }

/// Per-instance config path: `<config_dir>/<instance>/config.json`.
/// If `instance` is empty, returns the original single-instance
/// path (`~/.config/llm-quota-tray/config.json`).
pub fn config_path_for(instance: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = if instance.is_empty() {
        PathBuf::from(home).join(format!(".config/{CONFIG_DIR_BASE}"))
    } else {
        PathBuf::from(home).join(format!(".config/{CONFIG_DIR_BASE}-{instance}"))
    };
    dir.join(CONFIG_FILENAME)
}

/// Backwards-compatible path: assumes the default (no-instance)
/// config dir. Prefer `config_path_for(&instance::name())`.
pub fn config_path() -> PathBuf {
    config_path_for(crate::instance::name())
}

/// Load config from disk; missing or malformed → defaults.
pub fn load_for(instance: &str) -> Config {
    let path = config_path_for(instance);
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

/// Backwards-compatible load (default instance). Prefer `load_for`.
pub fn load() -> Config {
    load_for(crate::instance::name())
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
        // Force 0600 — matches gjs's `f.set_attribute_uint32('unix::mode',
        // 0o600, ...)`. `std::fs::write` inherits the process umask
        // (typically 0644). The config has no secrets, but matching
        // gjs's permission flip keeps first-run installs consistent
        // with subsequent runs.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
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

    /// Acquire the test lock; all config tests should `let _g = lock_home();`
    /// at the top so they don't stomp each other's HOME.
    fn lock_home() -> std::sync::MutexGuard<'static, ()> {
        HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn defaults_when_file_does_not_exist() {
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-empty");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", &tmp);
        let cfg = load();
        assert_eq!(cfg.refresh_seconds, 120);
        assert_eq!(cfg.refresh_min_seconds, 15,
                   "default refresh_min_seconds must be 15");
        std::env::remove_var("HOME");
    }

    #[test]
    fn refresh_min_seconds_field_is_optional_in_json() {
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-no-min");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = tmp.join("config.json");
        write_at(&path, r#"{
            "endpoint": "https://example.invalid/coding",
            "dashboard_url": "https://example.invalid/dash",
            "label": "Coding Plan",
            "refresh_seconds": 120,
            "refresh_max_backoff_seconds": 600
        }"#);
        let bytes = std::fs::read(&path).unwrap();
        let cfg: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(cfg.refresh_min_seconds, 15);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn per_instance_config_path() {
        assert!(config_path_for("").to_string_lossy()
                .contains(".config/llm-quota-tray/config.json"));
        assert!(config_path_for("codex").to_string_lossy()
                .contains(".config/llm-quota-tray-codex/config.json"));
        assert!(config_path_for("openai").to_string_lossy()
                .contains(".config/llm-quota-tray-openai/config.json"));
    }

    #[test]
    fn ring_colors_override_in_config() {
        // A per-instance config can override ring colors (e.g. an
        // orange scheme for one tray, blue for another). The
        // canonical new shape splits them into `inner` (status
        // bucket colors) + `outer` (percentage-fill channel).
        let json = r##"{
            "endpoint": "https://example.invalid",
            "dashboard_url": "https://example.invalid/dash",
            "label": "Orange Tray",
            "ring_colors": {
                "inner": {
                    "normal":   "#ff9900",
                    "warning":  "#ff5500",
                    "throttled": "#ff0000"
                },
                "outer": "#1d8b3a"
            },
            "refresh_seconds": 60,
            "refresh_max_backoff_seconds": 600
        }"##;
        let cfg: Config = serde_json::from_slice(json.as_bytes()).unwrap();
        assert_eq!(cfg.ring_colors.inner.normal,   "#ff9900");
        assert_eq!(cfg.ring_colors.inner.warning,  "#ff5500");
        assert_eq!(cfg.ring_colors.inner.throttled, "#ff0000");
        assert_eq!(cfg.ring_colors.outer, "#1d8b3a");
    }

    #[test]
    fn ring_colors_partial_override_inner_only() {
        // Just the inner block, no outer → outer defaults to the
        // neutral accent (see `provider::DEFAULT_OUTER_COLOR`).
        let json = r##"{
            "endpoint": "https://example.invalid",
            "dashboard_url": "https://example.invalid/dash",
            "label": "x",
            "ring_colors": {
                "inner": {
                    "normal": "#00ff00",
                    "warning": "#ffff00",
                    "throttled": "#ff00ff"
                }
            },
            "refresh_seconds": 60,
            "refresh_max_backoff_seconds": 600
        }"##;
        let cfg: Config = serde_json::from_slice(json.as_bytes()).unwrap();
        assert_eq!(cfg.ring_colors.inner.normal, "#00ff00");
        assert_eq!(cfg.ring_colors.outer, crate::provider::DEFAULT_OUTER_COLOR);
    }

    #[test]
    fn ring_colors_legacy_flat_shape_still_loads() {
        // Backward-compat: configs from before the inner/outer split
        // used `{normal, warning, throttled}` directly at the
        // `ring_colors` level. Those must still parse — the legacy
        // bucket colors become `inner`, and `outer` falls back to
        // the neutral accent default.
        let json = r##"{
            "endpoint": "https://example.invalid",
            "dashboard_url": "https://example.invalid/dash",
            "label": "Legacy Tray",
            "ring_colors": {
                "normal":   "#3a9d4d",
                "warning":  "#f6d32d",
                "throttled": "#e01b24"
            },
            "refresh_seconds": 60,
            "refresh_max_backoff_seconds": 600
        }"##;
        let cfg: Config = serde_json::from_slice(json.as_bytes()).unwrap();
        assert_eq!(cfg.ring_colors.inner.normal,   "#3a9d4d");
        assert_eq!(cfg.ring_colors.inner.warning,  "#f6d32d");
        assert_eq!(cfg.ring_colors.inner.throttled, "#e01b24");
        assert_eq!(cfg.ring_colors.outer, crate::provider::DEFAULT_OUTER_COLOR,
                   "legacy config with no outer field should fall back to the neutral accent");
    }

    #[test]
    fn ring_colors_empty_object_uses_defaults() {
        // An empty `ring_colors: {}` should yield the compile-time
        // defaults (inner = gjs green/yellow/red, outer = neutral
        // accent) so an instance that wants all defaults can omit
        // every inner/outer field explicitly.
        let json = r##"{
            "endpoint": "https://example.invalid",
            "dashboard_url": "https://example.invalid/dash",
            "label": "Default Tray",
            "ring_colors": {},
            "refresh_seconds": 60,
            "refresh_max_backoff_seconds": 600
        }"##;
        let cfg: Config = serde_json::from_slice(json.as_bytes()).unwrap();
        let defaults = crate::provider::default_ring_colors();
        assert_eq!(cfg.ring_colors.inner.normal,    defaults.inner.normal);
        assert_eq!(cfg.ring_colors.inner.warning,   defaults.inner.warning);
        assert_eq!(cfg.ring_colors.inner.throttled, defaults.inner.throttled);
        assert_eq!(cfg.ring_colors.outer,           defaults.outer);
    }

    #[test]
    fn auth_override_in_config() {
        let json_bearer = r#"{
            "endpoint": "https://x", "dashboard_url": "x", "label": "x",
            "auth": {"type": "bearer"},
            "refresh_seconds": 60, "refresh_max_backoff_seconds": 600
        }"#;
        let cfg: Config = serde_json::from_slice(json_bearer.as_bytes()).unwrap();
        assert!(matches!(cfg.auth, AuthConfig::Bearer));

        let json_xapikey = r#"{
            "endpoint": "https://x", "dashboard_url": "x", "label": "x",
            "auth": {"type": "header", "name": "x-api-key"},
            "refresh_seconds": 60, "refresh_max_backoff_seconds": 600
        }"#;
        let cfg: Config = serde_json::from_slice(json_xapikey.as_bytes()).unwrap();
        match cfg.auth {
            AuthConfig::Header { name } => assert_eq!(name, "x-api-key"),
            _ => panic!("expected Header variant"),
        }

        let json_query = r#"{
            "endpoint": "https://x", "dashboard_url": "x", "label": "x",
            "auth": {"type": "query_param", "name": "key"},
            "refresh_seconds": 60, "refresh_max_backoff_seconds": 600
        }"#;
        let cfg: Config = serde_json::from_slice(json_query.as_bytes()).unwrap();
        match cfg.auth {
            AuthConfig::QueryParam { name } => assert_eq!(name, "key"),
            _ => panic!("expected QueryParam variant"),
        }
    }

    #[test]
    fn shape_override_in_config() {
        // A per-instance config can specify its own JSON shape
        // (different field names, units, error envelope).
        let json = r#"{
            "endpoint": "https://x", "dashboard_url": "x", "label": "Daily",
            "shape": {
                "entries_path": "/data",
                "windows": [{
                    "id": "daily",
                    "field_prefix": "daily",
                    "start_unit_ms": 1,
                    "reset_unit_ms": 1,
                    "reset_is_absolute_epoch": false
                }]
            },
            "refresh_seconds": 60, "refresh_max_backoff_seconds": 600
        }"#;
        let cfg: Config = serde_json::from_slice(json.as_bytes()).unwrap();
        assert_eq!(cfg.shape.entries_path, "/data");
        assert_eq!(cfg.shape.windows.len(), 1);
        assert_eq!(cfg.shape.windows[0].id, "daily");
    }

    #[test]
    fn malformed_falls_back_to_defaults() {
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-malformed");
        let path = tmp.join("config.json");
        write_at(&path, "this is not json");
        std::env::set_var("HOME", &tmp);
        let cfg = load();
        assert_eq!(cfg.refresh_seconds, 120);
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
        assert_eq!(cfg.refresh_seconds, 120);
        std::env::remove_var("HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[cfg(unix)]
    fn load_or_init_writes_default_config_with_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_home();
        let tmp = std::env::temp_dir().join("minimax-cfg-0600");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", &tmp);
        let _ = load_or_init().unwrap();
        let path = config_path();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600,
                   "default-written config must be 0600");
        std::env::remove_var("HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Walk every `*.json` file under `examples/providers/` and
    /// deserialize each as a `Config`. Catches typos and schema drift
    /// in the templates at `cargo test` time rather than at tray
    /// runtime. The templates are allowed to contain extra
    /// `_comment*` / `_win_comment*` / `_adapter_example` keys —
    /// those are silently ignored (the `Config` types don't use
    /// `deny_unknown_fields`).
    #[test]
    fn provider_templates_deserialize() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/providers");
        assert!(dir.is_dir(),
                "examples/providers/ should exist next to Cargo.toml \
                 — got {}", dir.display());

        let mut found_any = false;
        let mut errors: Vec<String> = Vec::new();

        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("read_dir(examples/providers)")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        entries.sort_by_key(|e| e.path());

        for entry in entries {
            found_any = true;
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => { errors.push(format!("{name}: read error: {e}")); continue; }
            };
            let cfg: Config = match serde_json::from_slice(&bytes) {
                Ok(c) => c,
                Err(e) => { errors.push(format!("{name}: parse error: {e}")); continue; }
            };

            // Required fields a template is expected to populate.
            // Empty values usually mean the template was started from
            // a partial scaffold and never finished.
            if cfg.endpoint.trim().is_empty() {
                errors.push(format!("{name}: endpoint is empty"));
            }
            if cfg.label.trim().is_empty() {
                errors.push(format!("{name}: label is empty"));
            }
            if cfg.shape.windows.is_empty() {
                errors.push(format!("{name}: shape has no windows"));
            }
            for w in &cfg.shape.windows {
                if w.id.trim().is_empty() {
                    errors.push(format!("{name}: window has empty id"));
                }
                if w.field_prefix.trim().is_empty() {
                    errors.push(format!("{name}: window {} has empty field_prefix",
                                        w.id));
                }
            }
            // 0 means 'never poll' — almost certainly a typo.
            if cfg.refresh_seconds == 0 {
                errors.push(format!("{name}: refresh_seconds is 0"));
            }
        }

        assert!(found_any,
                "no .json templates found under {}", dir.display());
        if !errors.is_empty() {
            panic!("provider template validation failed:\n  - {}",
                   errors.join("\n  - "));
        }
    }
}
