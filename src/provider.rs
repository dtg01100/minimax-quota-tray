//! Per-instance configuration primitives for the quota tray.
//!
//! Compile-time defaults are deliberately neutral — no provider
//! branding baked into the source. Each instance configures itself
//! from `config.json`; the constants here are just fallbacks for
//! fields the user can omit.
//!
//! The tray is provider-agnostic. Everything that's API-specific
//! lives in `config.json` for each instance — endpoint, JSON shape,
//! ring colors, auth style, User-Agent prefix. This module holds the
//! types those values deserialize into plus a few compile-time
//! defaults for fields the user can omit from config.
//!
//! ## Multi-instance support
//!
//! A single binary can serve as multiple concurrent instances via
//! the `--instance=<name>` CLI flag (or `QUOTA_INSTANCE=<name>` env
//! var). The instance name namespaces:
//!
//!   - config dir  → `~/.config/<base>-<name>/config.json`
//!   - lock file   → `${XDG_RUNTIME_DIR}/<base>-<name>.pid`
//!   - keyring app → `<base>-<name>`
//!
//! where `<base>` is `minimax-quota`. Two instances never collide on
//! disk, in the keyring, or in the PID lock.
//!
//! Each instance is its own tray icon (different chip, different
//! colors, different menu). Each can target a different API (via
//! different endpoint + shape + auth in its config.json), so a user
//! can run MiniMax + Codex + OpenAI trays side-by-side.
//!
//! ## Adding a new provider
//!
//! No code change needed for a new endpoint hitting an existing
//! shape — just create a new instance:
//!
//! ```bash
//! minimax-quota-tray --instance=codex
//! # then write ~/.config/minimax-quota-codex/config.json
//! ```
//!
//! For a brand-new JSON shape, edit the per-instance config to
//! inline a full `shape` block (see `PlanShape` below for fields).
//! For a brand-new auth style, extend the `AuthConfig` enum here.

use serde::{Deserialize, Serialize};

// ============================================================================
// Ring colors
// ============================================================================

/// Per-instance hex colors for the chip's three bucket states. The
/// center dot uses the same color as the ring — that's what makes
/// the chip read as "ring with center" instead of an empty arc.
///
/// Stored as `String` (not `&'static str`) so each instance's
/// config.json can specify its own palette without recompiling.
/// Examples: `["#ff9900", "#ff5500", "#ff0000"]` for an orange
/// scheme, `["#3366ff", "#9933ff", "#cc00ff"]` for a blue/purple
/// scheme.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingColors {
    pub normal: String,
    pub warning: String,
    pub throttled: String,
}

impl Default for RingColors {
    fn default() -> Self { default_ring_colors() }
}

/// Compile-time default ring colors. Used when config.json omits
/// `ring_colors`. These are the classic gjs colors (green / yellow
/// / red) — chosen because they're widely distinguishable on light
/// and dark panel themes, not because of any provider branding.
pub fn default_ring_colors() -> RingColors {
    RingColors {
        normal:   "#3a9d4d".to_string(),
        warning:  "#f6d32d".to_string(),
        throttled: "#e01b24".to_string(),
    }
}

// ============================================================================
// Auth header dispatch
// ============================================================================

/// How the API key is sent in HTTP requests. Config-driven so each
/// instance picks its own auth style; no code change needed for the
/// common cases (Bearer, custom header, query string).
///
/// Serialized as a tagged enum — `"auth": {"type": "bearer"}` etc.
/// The runtime is `&'static str` for the header NAME so reqwest can
/// hold it without a heap allocation per request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// `Authorization: Bearer <key>` — the common case
    /// (OpenAI, Anthropic, Mistral, MiniMax, …).
    Bearer,

    /// `<header_name>: <key>` — e.g. `x-api-key`, `x-goog-api-key`.
    Header { name: String },

    /// `<header_name>: <format>` with `{key}` substituted — e.g.
    /// `{name: "Authorization", format: "Token {key}"}` for
    /// `Authorization: Token sk-...`.
    Custom { name: String, format: String },

    /// Append `?<param>=<key>` to the endpoint URL (no header).
    /// Useful for providers that only accept the key in the query
    /// string. The fetch code handles the URL rewriting before the
    /// request goes out.
    QueryParam { name: String },
}

impl Default for AuthConfig {
    fn default() -> Self { AuthConfig::Bearer }
}

impl AuthConfig {
    /// Build `(header_name, header_value)` for a request. Returns
    /// `("", "")` for `QueryParam` — the caller appends the param
    /// to the endpoint URL via `apply_to_endpoint()` instead.
    ///
    /// Both return values are owned `String`s; reqwest's
    /// `header(name, value)` accepts either `&str` or `String`.
    pub fn build(&self, api_key: &str) -> (String, String) {
        match self {
            AuthConfig::Bearer =>
                ("Authorization".to_string(), format!("Bearer {api_key}")),
            AuthConfig::Header { name } =>
                (name.clone(), api_key.to_string()),
            AuthConfig::Custom { name, format } =>
                (name.clone(), format.replace("{key}", api_key)),
            AuthConfig::QueryParam { .. } =>
                (String::new(), String::new()),
        }
    }

    /// If this auth style requires URL modification, return the
    /// rewritten URL. Otherwise return the input unchanged. Used by
    /// `fetch::fetch_windows_blocking` to handle query-string auth.
    pub fn apply_to_endpoint(&self, endpoint: &str, api_key: &str) -> String {
        match self {
            AuthConfig::QueryParam { name } => {
                let sep = if endpoint.contains('?') { '&' } else { '?' };
                format!("{endpoint}{sep}{name}={api_key}")
            }
            _ => endpoint.to_string(),
        }
    }
}

// ============================================================================
// JSON shape
// ============================================================================

/// How to extract one quota window from one entry of the API payload.
///
/// `id` is `String` (not `&'static str`) so each instance's
/// `PlanShape::windows` can be deserialized from config.json — the
/// parser produces `Window`s with the same `String` ids, which the
/// burn-rate history keys by.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowShape {
    pub id: String,
    pub field_prefix: String,
    /// Override field name for the window start (when it doesn't
    /// follow the `{prefix}_start_time` convention). Defaults to
    /// `"start_time"` if `None`.
    #[serde(default)]
    pub start_field: Option<String>,
    /// Override field name for the time-until-reset / reset-time.
    /// Defaults to `"remains_time"` if `None`.
    #[serde(default)]
    pub reset_field: Option<String>,
    /// Multiplier applied to the raw `start_field` value to convert
    /// to ms-since-epoch. 1000 if the field is epoch-seconds, 1 if
    /// it's already ms.
    pub start_unit_ms: i64,
    /// Multiplier applied to the raw `reset_field` value.
    pub reset_unit_ms: i64,
    /// If `true`, the parser uses `reset_field` as an absolute epoch
    /// timestamp (after applying `reset_unit_ms`) instead of adding
    /// it to `now_ms`.
    pub reset_is_absolute_epoch: bool,
}

/// Where to find the entry array in the JSON payload, and which
/// windows to extract from each entry. The array element at index 0
/// is always used (MiniMax returns one entry per request; if your
/// provider returns multiple entries and you want to pick by key,
/// expose them as separate instances).
///
/// Define as many windows as the API supports — there's no
/// structural limit. The tray renders each window as a menu row; the
/// first window drives the chip percentage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanShape {
    pub entries_path: String,
    pub windows: Vec<WindowShape>,
    #[serde(default)]
    pub error_envelope: Option<ErrorEnvelope>,
}

/// Some providers return HTTP 200 with an error envelope in the body
/// (MiniMax does this). `ErrorEnvelope` lets the parser surface those
/// as proper errors instead of falling through to "payload missing".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub code_path: String,
    pub message_path: String,
    pub success_codes: Vec<i64>,
}

/// Compile-time default shape — single generic window, no entry
/// path (the user is expected to override `shape` in config.json
/// for any real provider). Used as a fallback when the config
/// omits `shape`; most instances will override it.
pub fn default_shape() -> PlanShape {
    PlanShape {
        entries_path: "/".to_string(),
        windows: vec![WindowShape {
            id: String::from("primary"),
            field_prefix: String::from("primary"),
            start_field: None,
            reset_field: None,
            start_unit_ms: 1000,
            reset_unit_ms: 1,
            reset_is_absolute_epoch: false,
        }],
        error_envelope: None,
    }
}

impl Default for PlanShape {
    fn default() -> Self { default_shape() }
}

// ============================================================================
// Default User-Agent
// ============================================================================

/// Default User-Agent prefix. Used when config.json omits
/// `user_agent`. The binary appends the crate version at build
/// client time to produce `"<prefix>/<version>"`.
pub const DEFAULT_USER_AGENT: &str = "minimax-quota-tray";
