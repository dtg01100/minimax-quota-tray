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
//! where `<base>` is `llm-quota-tray` (see `crate::instance`). Two
//! instances never collide on disk, in the keyring, or in the PID
//! lock.
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
//! llm-quota-tray --instance=codex
//! # then write ~/.config/llm-quota-tray-codex/config.json
//! ```
//!
//! For a brand-new JSON shape, edit the per-instance config to
//! inline a full `shape` block (see `PlanShape` below for fields).
//! For a brand-new auth style, extend the `AuthConfig` enum here.

use serde::{Deserialize, Serialize};

// ============================================================================
// Ring colors
// ============================================================================

/// Per-instance hex colors for the chip's two visual channels.
///
///   - `inner` colors the center dot (and the solid static-state
///     icons). Three states: normal / warning / throttled. This is
///     the **status** channel — what state the tray is in.
///   - `outer` colors the outer ring (track + progress arc) and
///     carries the **remaining-quota** signal — the percentage fill
///     on the arc. A single color; the *length* of the arc encodes
///     the percentage, the hue just has to stay visually distinct
///     from the inner dot's hue so the two layers don't smear
///     together.
///
/// Splitting them lets the inner dot flip through the bucket colors
/// without re-coloring the percentage-fill arc on every state
/// change. The default outer color is a neutral accent (GNOME blue)
/// so it reads as a "progress meter" on light and dark panels
/// regardless of which bucket the inner dot is in.
///
/// Stored as `String` (not `&'static str`) so each instance's
/// config.json can specify its own palette without recompiling.
///
/// Backward-compatible deserialization: legacy configs that used
/// `{ "normal": ..., "warning": ..., "throttled": ... }` directly at
/// the `ring_colors` level are still accepted — those fields map to
/// `inner` and `outer` falls back to the default. See the
/// `Deserialize` impl below for the exact shapes accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RingColorsRepr")]
pub struct RingColors {
    pub inner: BucketColors,
    pub outer: String,
}

/// Internal serde shuttle. Holds the wire shape and converts to the
/// public `RingColors` via `From`. We accept three wire shapes:
///
///   1. `{}` — fully defaulted (empty object, common when the
///      config omits `ring_colors`).
///   2. `{ "inner": {normal, warning, throttled}, "outer": "..." }`
///      — the canonical new shape; `inner` and `outer` are both
///      optional, defaulting to compile-time fallbacks.
///   3. `{ "normal": "...", "warning": "...", "throttled": "..." }`
///      — the legacy flat shape (pre inner/outer split). The
///      top-level bucket colors become `inner` and `outer` defaults
///      to the neutral accent.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RingColorsRepr {
    inner: Option<BucketColors>,
    outer: Option<String>,
    // Legacy flat fields. When `inner` is None and any of these is
    // present, the conversion in `From` treats the payload as legacy.
    normal: Option<String>,
    warning: Option<String>,
    throttled: Option<String>,
}

impl From<RingColorsRepr> for RingColors {
    fn from(r: RingColorsRepr) -> Self {
        let inner = if let Some(b) = r.inner {
            // New shape wins outright.
            b
        } else if r.normal.is_some() || r.warning.is_some() || r.throttled.is_some() {
            // Legacy shape — pull bucket colors off the top level.
            BucketColors {
                normal: r.normal.unwrap_or_default(),
                warning: r.warning.unwrap_or_default(),
                throttled: r.throttled.unwrap_or_default(),
            }
        } else {
            // Empty payload — defaults.
            default_inner_colors()
        };
        RingColors {
            outer: r.outer.unwrap_or_else(|| DEFAULT_OUTER_COLOR.to_string()),
            inner,
        }
    }
}

/// The three bucket-state colors for the inner dot. Same names and
/// meaning as the legacy flat fields: `normal` for the healthy
/// green/yellow/red state, `warning` for the burn-flip / yellow
/// threshold state, `throttled` for the exhausted / red state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketColors {
    pub normal: String,
    pub warning: String,
    pub throttled: String,
}

/// Default inner-dot bucket colors. Classic gjs colors
/// (green / yellow / red) — widely distinguishable on light and
/// dark panel themes.
pub fn default_inner_colors() -> BucketColors {
    BucketColors {
        normal: "#3a9d4d".to_string(),
        warning: "#f6d32d".to_string(),
        throttled: "#e01b24".to_string(),
    }
}

/// Default outer-ring color. Neutral GNOME-blue accent (#3584e4) —
/// visible on light and dark panels, doesn't compete with the
/// inner dot's bucket colors when they're red/yellow/green.
pub const DEFAULT_OUTER_COLOR: &str = "#3584e4";

impl Default for RingColors {
    fn default() -> Self {
        default_ring_colors()
    }
}

/// Compile-time default ring colors. Used when config.json omits
/// `ring_colors`. See `RingColors` for the inner/outer rationale.
pub fn default_ring_colors() -> RingColors {
    RingColors {
        inner: default_inner_colors(),
        outer: DEFAULT_OUTER_COLOR.to_string(),
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// `Authorization: Bearer <key>` — the common case
    /// (OpenAI, Anthropic, Mistral, MiniMax, …).
    #[default]
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

impl AuthConfig {
    /// Build `(header_name, header_value)` for a request. Returns
    /// `("", "")` for `QueryParam` — the caller appends the param
    /// to the endpoint URL via `apply_to_endpoint()` instead.
    ///
    /// Both return values are owned `String`s; reqwest's
    /// `header(name, value)` accepts either `&str` or `String`.
    pub fn build(&self, api_key: &str) -> (String, String) {
        match self {
            AuthConfig::Bearer => ("Authorization".to_string(), format!("Bearer {api_key}")),
            AuthConfig::Header { name } => (name.clone(), api_key.to_string()),
            AuthConfig::Custom { name, format } => (name.clone(), format.replace("{key}", api_key)),
            AuthConfig::QueryParam { .. } => (String::new(), String::new()),
        }
    }

    /// If this auth style requires URL modification, return the
    /// rewritten URL. Otherwise return the input unchanged. Used by
    /// `fetch::fetch_windows_blocking` to handle query-string auth.
    ///
    /// The API key is percent-encoded so that keys containing
    /// URL-significant characters (`&`, `=`, `#`, `/`, non-ASCII, etc.)
    /// don't corrupt the request URL. The parameter name is trusted
    /// (it comes from the user's config.json, not from the key itself)
    /// and is appended verbatim.
    pub fn apply_to_endpoint(&self, endpoint: &str, api_key: &str) -> String {
        match self {
            AuthConfig::QueryParam { name } => {
                let sep = if endpoint.contains('?') { '&' } else { '?' };
                let encoded = percent_encode_query_value(api_key);
                format!("{endpoint}{sep}{name}={encoded}")
            }
            _ => endpoint.to_string(),
        }
    }
}

/// Percent-encode a query-string *value*. ASCII alphanumerics and
/// `-_.~` are left untouched (unreserved per RFC 3986); everything
/// else is `%XX`-encoded byte-by-byte. This is sufficient for the
/// common case (API keys in `QueryParam` auth) without pulling in a
/// dedicated percent-encoding crate. UTF-8 multibyte characters are
/// encoded one byte at a time, which is the correct behavior for
/// `application/x-www-form-urlencoded`.
fn percent_encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
            || byte == b'~'
        {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_query_value_leaves_alphanumeric_untouched() {
        assert_eq!(percent_encode_query_value("sk-abc123"), "sk-abc123");
    }

    #[test]
    fn percent_encode_query_value_encodes_special_chars() {
        assert_eq!(percent_encode_query_value("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn percent_encode_query_value_encodes_spaces() {
        assert_eq!(percent_encode_query_value("a b"), "a%20b");
    }

    #[test]
    fn percent_encode_query_value_encodes_non_ascii() {
        // UTF-8 bytes for 'é' are 0xC3 0xA9
        assert_eq!(percent_encode_query_value("café"), "caf%C3%A9");
    }

    #[test]
    fn percent_encode_query_value_empty() {
        assert_eq!(percent_encode_query_value(""), "");
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

    /// Unit of the `{prefix}_total_count` / `{prefix}_usage_count`
    /// integers. Drives the burn-row label suffix and the projected
    /// dollar rate. Defaults to `"tokens"` (legacy behavior).
    ///
    /// Supported values:
    /// - `"tokens"` — integers are token counts. Burn row says
    ///   `40 tok/h`. Default when this field is absent.
    /// - `"cents"` — integers are the smallest currency unit (cents
    ///   for USD, fen for CNY). Burn row says `$0.40/h` (or with the
    ///   `currency` field, `CNY 0.40/h`). Used by Together, DeepSeek,
    ///   OpenRouter (auth/key returns USD floats — scale to cents in
    ///   the adapter, then set this flag).
    /// - `"milliunits"` — integers are thousandths of a billing unit
    ///   (OpenAI-billed-units style). Burn row scales by 1/1000.
    ///   Rarely needed; included for completeness.
    ///
    /// New providers that report usage as a currency value (not as a
    /// token count) should set `count_unit: "cents"` AND scale the
    /// value in their adapter (e.g. `$25.00 → 2500 cents`). The tray
    /// never does the float→int conversion itself — the existing
    /// `num()` parser helper truncates floats to int and would lose
    /// sub-dollar precision otherwise.
    #[serde(default)]
    pub count_unit: Option<String>,

    /// Currency code for the burn-row label when `count_unit` is
    /// `"cents"` or `"milliunits"`. ISO-4217 lowercase ("usd",
    /// "eur", "cny") or display form ("USD", "¥"). Defaults to `""`
    /// (no currency symbol — burn row just shows the number with
    /// the existing rate formatter).
    ///
    /// For v1 of the OpenRouter prototype this is display-only.
    /// Values are not converted between currencies.
    #[serde(default)]
    pub currency: Option<String>,

    /// JSON pointer to a field in the API response entry holding
    /// the model id (e.g. `"/model_name"` for OpenRouter inference
    /// usage, or `"/data/0/model"` for nested shapes). When set,
    /// the parser reads the model id and the renderer can append a
    /// `· $X/h` cost fragment using the cached price table from
    /// `Config::pricing_endpoint`.
    ///
    /// Defaults to `None` (no model lookup). Token plans that don't
    /// tag entries by model — or any provider without a
    /// `pricing_endpoint` configured — leave this unset and the
    /// burn row stays as it is today.
    #[serde(default)]
    pub pricing_model_path: Option<String>,
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
            count_unit: None,
            currency: None,
            pricing_model_path: None,
        }],
        error_envelope: None,
    }
}

impl Default for PlanShape {
    fn default() -> Self {
        default_shape()
    }
}

// ============================================================================
// Default User-Agent
// ============================================================================

/// Default User-Agent prefix. Used when config.json omits
/// `user_agent`. The binary appends the crate version at build
/// client time to produce `"<prefix>/<version>"`.
pub const DEFAULT_USER_AGENT: &str = "llm-quota-tray";
