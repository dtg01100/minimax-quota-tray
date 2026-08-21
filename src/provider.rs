//! Provider configuration for minimax-quota-tray.
//!
//! This module is the single source of truth for everything that
//! differs between quota APIs. To port this tray at a different
//! endpoint, edit THIS file (and `config.example.json` / your
//! `~/.config/minimax-quota/config.json`); the rest of the codebase
//! reads from the constants here and needs no changes for typical
//! ports.
//!
//! ## What lives here
//!
//! 1. **`auth::header(api_key)`** — how the API key is sent.
//!    Currently `Authorization: Bearer <key>`. Switch to
//!    `x-goog-api-key`, `x-api-key`, query-string, etc. by editing
//!    one function.
//!
//! 2. **`user_agent::PREFIX`** — User-Agent string prefix. The binary
//!    appends the crate version. Change this if you fork the tray
//!    and want your fork to identify itself separately.
//!
//! 3. **`json::MINIMAX`** — a `PlanShape` value describing:
//!
//!    - which JSON path holds the entry array (`/model_remains`)
//!    - what windows to extract from each entry, including each
//!      window's `id` (drives the menu label / chip lookup),
//!      field-name prefix (`current_interval_*` / `current_weekly_*`),
//!      optional per-window field-name overrides, and unit
//!      conversions (e.g. `start_time` is in epoch-seconds → ×1000,
//!      `remains_time` is a duration-ms → no conversion)
//!    - which JSON path carries the error envelope
//!      (`/base_resp/status_code` + `status_msg`) and which status
//!      codes count as success (`0`)
//!
//!    `parse_plan` in `src/parse.rs` reads this struct to do all the
//!    field lookups, so the JSON field names appear only here.
//!
//! 4. **`defaults::PLANS`** — the plan table that ships in
//!    `Config::default()` and `config.example.json` (currently the
//!    two MiniMax plans: `coding_plan` and `token_plan`).
//!
//! ## What does NOT live here
//!
//! - The tray UI, scheduler, keyring, network monitor — provider-agnostic.
//! - The `Window` struct itself (`src/burn.rs`) — its fields are the
//!   abstract shape the UI consumes, not a provider mapping.
//! - Ring colors / static SVG icons (`src/icon.rs`) — visual, not API.
//! - The two-window structural assumption: `main.rs` still hardcodes
//!   `five_h` and `weekly` (the two windows the chip + menu + burn
//!   projection are laid out for). To support more or fewer windows,
//!   that's a `main.rs` change beyond the provider surface.
//!
//! ## Worked example: porting to Provider X
//!
//! Suppose Provider X exposes:
//!
//! ```text
//! GET https://api.provider.com/v1/usage
//! x-api-key: <key>
//! → { "daily":  { "limit": 1000, "used": 120, "reset_in_ms": 7200000 },
//!     "monthly":{ "limit": 30000, "used": 4500, "reset_in_ms": 2592000000 } }
//! ```
//!
//! Edit `src/provider.rs`:
//!
//! 1. Change `auth_header()` to return `("x-api-key", api_key.into())`.
//! 2. Add a new `PlanShape` constant `PROVIDER_X` with:
//!    - `entries_path: "/"` if the root object *is* the entry (the
//!      parser wraps a single-object response into a one-element
//!      synthetic array internally), or a JSON pointer if there's an
//!      outer wrapper
//!    - one or two `WindowShape` entries pointing at
//!      `daily.limit` / `daily.used` / `daily.reset_in_ms` (and
//!      `monthly.*`)
//!    - `error_envelope: None` if Provider X returns HTTP errors
//!      with proper status codes, or a new `ErrorEnvelope` if it
//!      returns 200s with an error envelope
//! 3. Replace `MINIMAX` references with `PROVIDER_X` in
//!    `src/parse.rs` and `src/fetch.rs` (or wire a runtime selection
//!    based on the active `plan` field).
//!
//! The UI doesn't change. The keyring, scheduler, and tray code
//! don't change.

// ============================================================================
// HTTP auth header
// ============================================================================

/// Build the HTTP authentication header for an API request.
///
/// Returns `(header_name, header_value)`. MiniMax uses
/// `Authorization: Bearer <key>`; other providers differ:
///
/// | Provider            | Header                              |
/// | ------------------- | ----------------------------------- |
/// | OpenAI / Anthropic  | `Authorization: Bearer <key>`       |
/// | Google Gemini       | `x-goog-api-key: <key>`             |
/// | Mistral             | `Authorization: Bearer <key>`       |
/// | Custom (header)     | `x-api-key: <key>`                  |
/// | Custom (query)      | append `?key=<key>` to the endpoint |
///
/// For header-style auth, change this function to return the
/// appropriate `("header-name", value)` tuple. For query-string auth,
/// you'll also need to append the key to the endpoint in
/// `src/fetch.rs::fetch_windows_blocking` before the `client.get()`
/// call.
pub fn auth_header(api_key: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {api_key}"))
}

// ============================================================================
// User-Agent
// ============================================================================

/// User-Agent prefix. The binary's `CARGO_PKG_VERSION` is appended by
/// `src/fetch.rs::build_client()` to produce `"<PREFIX>/<version>"`.
/// Fork the prefix if you rebrand the binary — the API owner uses it
/// to identify clients when triaging support tickets.
pub const USER_AGENT_PREFIX: &str = "minimax-quota-tray";

// ============================================================================
// JSON shape
// ============================================================================

/// How to extract one quota window from one entry of the API payload.
///
/// The parser reads `{prefix}_total_count`, `{prefix}_usage_count`,
/// and `{prefix}_remaining_percent` from the entry by default. For
/// fields whose names don't follow that pattern (e.g. MiniMax's
/// `weekly_start_time` instead of `current_weekly_start_time`), set
/// `start_field` / `reset_field` to override.
///
/// `start_unit_ms` is multiplied into the raw `start_field` value to
/// convert it to ms-since-epoch. Use 1000 if the field is in
/// epoch-seconds, 1 if it's already in ms.
///
/// `reset_field` semantics: by default, the value is treated as a
/// DURATION in ms (time-until-reset), and the parser computes
/// `reset_at = now_ms + value`. Set `reset_is_absolute_epoch: true`
/// if your provider returns the reset time as an absolute epoch
/// (seconds or ms — multiply accordingly).
#[derive(Debug, Clone)]
pub struct WindowShape {
    /// Stable window ID; appears in menu labels and burn-rate rows.
    /// Must be unique across `windows` in one `PlanShape`.
    pub id: &'static str,
    /// Field-name prefix; entries are read as `{prefix}_total_count`,
    /// `{prefix}_usage_count`, `{prefix}_remaining_percent`.
    pub field_prefix: &'static str,
    /// Override field name for the window start (when it doesn't
    /// follow the `{prefix}_start_time` convention). Defaults to
    /// `"start_time"` if `None`.
    pub start_field: Option<&'static str>,
    /// Override field name for the time-until-reset / reset-time.
    /// Defaults to `"remains_time"` if `None`.
    pub reset_field: Option<&'static str>,
    /// Multiplier applied to the raw `start_field` value to convert
    /// to ms-since-epoch. 1000 if the field is epoch-seconds, 1 if
    /// it's already ms.
    pub start_unit_ms: i64,
    /// Multiplier applied to the raw `reset_field` value. MiniMax's
    /// `remains_time` is already in ms (duration), so 1. A provider
    /// returning epoch-seconds would want 1000.
    pub reset_unit_ms: i64,
    /// If `true`, the parser uses `reset_field` as an absolute epoch
    /// timestamp (after applying `reset_unit_ms`) instead of adding
    /// it to `now_ms`. Use this when your provider returns the reset
    /// time directly rather than the time-until-reset.
    pub reset_is_absolute_epoch: bool,
}

/// Where to find the entry array in the JSON payload, and which
/// windows to extract from each entry. The array element at index 0
/// is always used (MiniMax returns one entry per request; if your
/// provider returns multiple entries and you want to pick by key,
/// expose them as separate plans in `config.json`).
#[derive(Debug, Clone)]
pub struct PlanShape {
    /// JSON pointer to the array of entries (e.g. `"/model_remains"`).
    /// Use `"/"` if the response IS the entry (single-object
    /// response — `entry_lookup` wraps it into a one-element
    /// synthetic array).
    pub entries_path: &'static str,
    /// Window shapes, in display order. The first one drives the
    /// chip percentage; all of them get menu rows.
    pub windows: &'static [WindowShape],
    /// Provider-specific error envelope (e.g. MiniMax returns HTTP
    /// 200 with `{"base_resp": {"status_code": 1004, ...}}` on auth
    /// failures). `None` means the provider uses standard HTTP
    /// status codes only.
    pub error_envelope: Option<ErrorEnvelope>,
}

/// Some providers return HTTP 200 with an error envelope in the body
/// (MiniMax does this). `ErrorEnvelope` lets the parser surface those
/// as proper errors instead of falling through to "payload missing".
#[derive(Debug, Clone)]
pub struct ErrorEnvelope {
    /// JSON pointer to the status-code integer field.
    pub code_path: &'static str,
    /// JSON pointer to the human-readable message string field.
    pub message_path: &'static str,
    /// Status codes that count as success. Anything else is reported
    /// as `API error {code}: {message}`.
    pub success_codes: &'static [i64],
}

/// The MiniMax `/remains` endpoint's JSON shape. The Coding Plan and
/// Token Plan both use this shape; they just hit different paths
/// (see `defaults::PLANS` below).
///
/// Live shape (verified 2026-08-18):
/// ```json
/// {
///   "model_remains": [{
///     "model_name": "general",
///     "current_interval_total_count":      <i64>,
///     "current_interval_usage_count":      <i64>,
///     "current_interval_remaining_percent":<i64>,
///     "start_time":      <seconds-since-epoch>,
///     "remains_time":    <ms duration>,
///     "current_interval_status": <i64>,
///     "current_weekly_total_count":      <i64>,
///     "current_weekly_usage_count":      <i64>,
///     "current_weekly_remaining_percent":<i64>,
///     "weekly_start_time":   <seconds-since-epoch>,
///     "weekly_remains_time": <ms duration>,
///     "current_weekly_status": <i64>
///   }]
/// }
/// ```
///
/// `start_time` is epoch-seconds → ×1000. `remains_time` is a
/// DURATION in ms (not an epoch) → the parser computes
/// `reset_at = now_ms + remains_time_ms`. Mixing these up is the
/// bug behind the original "resets in 144 days" regression; see the
/// `reset_at_minus_now_is_time_remaining` test in `src/parse.rs`.
///
/// `current_interval_status` is deliberately not consumed: the
/// MiniMax-AI/cli documents it as `1=normal / 2=exhausted /
/// 3=unlimited`, so reading it would falsely flag every healthy
/// window as throttled. The parser derives throttling from
/// `remaining_pct <= 0` instead.
pub const MINIMAX: PlanShape = PlanShape {
    entries_path: "/model_remains",
    windows: &[
        WindowShape {
            id: "5h",
            field_prefix: "current_interval",
            start_field: None,             // → "start_time"
            reset_field: None,             // → "remains_time"
            start_unit_ms: 1000,           // epoch-seconds → ms
            reset_unit_ms: 1,              // already ms
            reset_is_absolute_epoch: false,
        },
        WindowShape {
            id: "weekly",
            field_prefix: "current_weekly",
            start_field: Some("weekly_start_time"),
            reset_field: Some("weekly_remains_time"),
            start_unit_ms: 1000,
            reset_unit_ms: 1,
            reset_is_absolute_epoch: false,
        },
    ],
    error_envelope: Some(ErrorEnvelope {
        code_path: "/base_resp/status_code",
        message_path: "/base_resp/status_msg",
        success_codes: &[0],
    }),
};

// ============================================================================
// Default plans (shipped with the binary)
// ============================================================================

/// A plan entry as it appears in `config.json`'s `plans` map. Used
/// to populate `Config::default()` and to write the example config on
/// first run. These are the MiniMax-specific defaults; a porter
/// editing this list changes what every fresh install starts with.
#[derive(Debug, Clone)]
pub struct DefaultPlan {
    pub id: &'static str,
    pub endpoint: &'static str,
    pub dashboard_url: &'static str,
    pub label: &'static str,
}

/// The plans that ship in `Config::default()` and `config.example.json`.
/// MiniMax exposes the same JSON shape under two endpoint paths (the
/// Coding Plan and the Token Plan) — one entry per plan keeps the
/// user's `plan` selector working without needing two parsers.
///
/// Add a new entry here when MiniMax adds a new plan tier (or when
/// you fork for a different provider — replace the whole array).
pub const DEFAULT_PLANS: &[DefaultPlan] = &[
    DefaultPlan {
        id: "coding_plan",
        endpoint: "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
        dashboard_url: "https://platform.minimax.io/console/plan",
        label: "Coding Plan",
    },
    DefaultPlan {
        id: "token_plan",
        endpoint: "https://api.minimax.io/v1/token_plan/remains",
        dashboard_url: "https://platform.minimax.io/console/plan",
        label: "Token Plan",
    },
];
