//! Provider definitions — the single source of truth for everything
//! that differs between quota APIs.
//!
//! A `Provider` encapsulates:
//!
//!   - **Auth** — how the API key is sent (Bearer, x-goog-api-key,
//!     x-api-key, query string, etc.). Encoded as a function pointer
//!     so different providers can implement different schemes without
//!     a trait object.
//!   - **User-Agent prefix** — provider identity in the User-Agent
//!     string (version auto-appended).
//!   - **Ring colors** — normal/warning/throttled. Hex strings; the
//!     gjs reference pinned a specific palette (green/yellow/red),
//!     but a forked provider can use any three distinguishable hues.
//!   - **Plan shapes** — a name→`PlanShape` map. Each `PlanShape`
//!     describes the JSON response structure for one endpoint family
//!     (entry path, window fields, error envelope).
//!   - **Default plans** — the table that ships in `Config::default()`
//!     and `config.example.json`. Each plan references a shape by ID,
//!     so multiple plans can share one shape (MiniMax's coding_plan
//!     and token_plan hit different endpoints but return the same
//!     JSON structure).
//!
//! Each `PlanShape` defines N windows — no fixed pair. The tray renders
//! however many windows the shape produces (see
//! `MenuInner::rebuild_window_rows`); the first window drives the chip
//! percentage.
//!
//! ## What does NOT live here
//!
//! - The tray UI, scheduler, keyring, network monitor — provider-agnostic.
//! - The `Window` struct (`src/burn.rs`) — its fields are the abstract
//!   shape every provider maps into.
//! - Ring radii / stroke widths / track opacity (`src/icon.rs`) — visual,
//!   not API.
//! - Window length: derived dynamically from `start_at`/`reset_at` by
//!   `burn::compute_burn`; the `id` field on `WindowShape` is just a
//!   label (e.g. "5h", "weekly", "daily").
//!
//! ## Multi-instance support
//!
//! A single binary can serve as multiple concurrent instances via the
//! `--instance=<name>` CLI flag (or `QUOTA_INSTANCE=<name>` env var).
//! The instance name is the seam that separates:
//!
//!   - config dir (`~/.config/minimax-quota-<name>/config.json`)
//!   - lock file (`${XDG_RUNTIME_DIR}/minimax-quota-<name>.pid`)
//!   - keyring `application` attribute (`minimax-quota-<name>`)
//!
//! so two instances don't collide on disk, in the keyring, or in the
//! PID lock. The default (no flag) keeps the original paths for
//! backwards compatibility.
//!
//! ## Adding a new provider
//!
//! Define a new `Provider` constant in this file and add it to the
//! `PROVIDERS` table. Users select it via `provider: "<id>"` in their
//! config.json. To run multiple providers concurrently, run the
//! binary twice with different `--instance=` flags pointing at
//! different config dirs.

// ============================================================================
// Auth header function type
// ============================================================================

/// Builds the HTTP auth header value for an API request.
///
/// Returns `(header_name, header_value)`. Common alternatives:
/// | Provider           | Header                              |
/// | ------------------ | ----------------------------------- |
/// | OpenAI / Anthropic | `Authorization: Bearer <key>`       |
/// | Google Gemini      | `x-goog-api-key: <key>`             |
/// | Mistral            | `Authorization: Bearer <key>`       |
/// | Custom (header)    | `x-api-key: <key>`                  |
/// | Custom (query)     | append `?key=<key>` to the endpoint |
///
/// For header-style auth, return the appropriate `("header-name", value)`
/// tuple. For query-string auth, the binary will append the key to the
/// endpoint in `src/fetch.rs::fetch_windows_blocking`.
pub type AuthHeaderFn = fn(&str) -> (&'static str, String);

/// Standard `Authorization: Bearer <key>` auth.
pub fn bearer_auth(api_key: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {api_key}"))
}

// ============================================================================
// Ring colors
// ============================================================================

/// Per-provider hex colors for the chip's three bucket states. Match
/// the gjs `RING_COLOR` table by default; forkers can override.
///
/// Colors are `&'static str` because `tiny_skia::Color::from_rgba8`
/// takes `u8` — `parse_hex_rgb` in `src/icon.rs` converts these
/// strings to `(u8, u8, u8)` at render time.
#[derive(Debug, Clone, Copy)]
pub struct RingColors {
    pub normal: &'static str,
    pub warning: &'static str,
    pub throttled: &'static str,
}

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
///
/// The window's `id` is arbitrary (e.g. "5h", "weekly", "daily",
/// "1m"). It appears as the menu row label and is the key under
/// which the tray stores burn-rate history; pick something stable
/// and descriptive.
#[derive(Debug, Clone)]
pub struct WindowShape {
    pub id: &'static str,
    pub field_prefix: &'static str,
    pub start_field: Option<&'static str>,
    pub reset_field: Option<&'static str>,
    pub start_unit_ms: i64,
    pub reset_unit_ms: i64,
    pub reset_is_absolute_epoch: bool,
}

/// Where to find the entry array in the JSON payload, and which
/// windows to extract from each entry. The array element at index 0
/// is always used (MiniMax returns one entry per request; if your
/// provider returns multiple entries and you want to pick by key,
/// expose them as separate plans in `config.json`).
///
/// Define as many windows as the API supports — there's no
/// structural 2-window limit. The tray renders each window as a
/// menu row; the first window drives the chip percentage.
#[derive(Debug, Clone)]
pub struct PlanShape {
    pub entries_path: &'static str,
    pub windows: &'static [WindowShape],
    pub error_envelope: Option<ErrorEnvelope>,
}

/// Some providers return HTTP 200 with an error envelope in the body
/// (MiniMax does this). `ErrorEnvelope` lets the parser surface those
/// as proper errors instead of falling through to "payload missing".
#[derive(Debug, Clone)]
pub struct ErrorEnvelope {
    pub code_path: &'static str,
    pub message_path: &'static str,
    pub success_codes: &'static [i64],
}

// ============================================================================
// Default plans
// ============================================================================

/// A plan entry as it appears in `config.json`'s `plans` map. Used
/// to populate `Config::default()` and to write the example config on
/// first run. The `shape_id` references an entry in
/// `Provider::plan_shapes` so multiple plans can share one shape
/// (MiniMax does this) or each plan can have its own.
#[derive(Debug, Clone)]
pub struct DefaultPlan {
    pub id: &'static str,
    pub endpoint: &'static str,
    pub dashboard_url: &'static str,
    pub label: &'static str,
    pub shape_id: &'static str,
}

// ============================================================================
// Provider struct
// ============================================================================

/// Everything that defines a single quota provider.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// Unique provider ID — used by `config.json`'s `provider` field
    /// to select which provider is active, and by `PROVIDERS` for
    /// lookup.
    pub id: &'static str,
    /// Build the auth header for an HTTP request.
    pub auth_header: AuthHeaderFn,
    /// User-Agent prefix; `src/fetch.rs::build_client` appends the
    /// crate version (`"<prefix>/<version>"`).
    pub user_agent_prefix: &'static str,
    /// Ring colors for the chip's three bucket states.
    pub ring_colors: RingColors,
    /// Name → PlanShape map. Plans reference shapes by ID.
    pub plan_shapes: &'static [(&'static str, PlanShape)],
    /// Plans that ship in `Config::default()` and
    /// `config.example.json`. Users can add more via `config.json`
    /// (any plan in `Config::plans` is honored).
    pub default_plans: &'static [DefaultPlan],
}

impl Provider {
    /// Look up a `PlanShape` by ID. Returns `None` if the ID is
    /// unknown — the parser treats that as a configuration error.
    pub fn shape(&self, id: &str) -> Option<&'static PlanShape> {
        self.plan_shapes.iter()
            .find(|(name, _)| *name == id)
            .map(|(_, s)| s)
    }
}

// ============================================================================
// MiniMax provider
// ============================================================================

/// The MiniMax `/remains` endpoint's JSON shape. Used by both the
/// Coding Plan and Token Plan endpoints (they return the same
/// structure, just at different URLs).
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
/// bug behind the original "resets in 144 days" regression; see
/// the `reset_at_minus_now_is_time_remaining` test in `src/parse.rs`.
///
/// `current_interval_status` is deliberately not consumed: the
/// MiniMax-AI/cli documents it as `1=normal / 2=exhausted /
/// 3=unlimited`, so reading it would falsely flag every healthy
/// window as throttled. The parser derives throttling from
/// `remaining_pct <= 0` instead.
pub const MINIMAX_REMAINS: PlanShape = PlanShape {
    entries_path: "/model_remains",
    windows: &[
        WindowShape {
            id: "5h",
            field_prefix: "current_interval",
            start_field: None,
            reset_field: None,
            start_unit_ms: 1000,
            reset_unit_ms: 1,
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

/// MiniMax ring colors — pinned to the gjs `RING_COLOR` table.
pub const MINIMAX_RING_COLORS: RingColors = RingColors {
    normal: "#3a9d4d",     // gjs RING_COLOR.normal
    warning: "#f6d32d",    // gjs RING_COLOR.warning
    throttled: "#e01b24",  // matches icons/quota-throttled.svg
};

/// The MiniMax provider — the default for `config.json`'s `provider`
/// field. Two plans (Coding Plan + Token Plan), both using the
/// `remains` shape.
pub const MINIMAX: Provider = Provider {
    id: "minimax",
    auth_header: bearer_auth,
    user_agent_prefix: "minimax-quota-tray",
    ring_colors: MINIMAX_RING_COLORS,
    plan_shapes: &[("remains", MINIMAX_REMAINS)],
    default_plans: &[
        DefaultPlan {
            id: "coding_plan",
            endpoint: "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
            dashboard_url: "https://platform.minimax.io/console/plan",
            label: "Coding Plan",
            shape_id: "remains",
        },
        DefaultPlan {
            id: "token_plan",
            endpoint: "https://api.minimax.io/v1/token_plan/remains",
            dashboard_url: "https://platform.minimax.io/console/plan",
            label: "Token Plan",
            shape_id: "remains",
        },
    ],
};

// ============================================================================
// Provider registry
// ============================================================================

/// All providers compiled into the binary. A user selects one via
/// the `provider` field in `config.json`. Currently only MiniMax is
/// registered; the registry is the seam where new providers plug in.
pub const PROVIDERS: &[&Provider] = &[&MINIMAX];

/// Look up a `Provider` by ID. Returns `None` if the ID isn't in the
/// registry — `main.rs` treats that as a config error and falls back
/// to the first registered provider (with a warning log).
pub fn provider_by_id(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().copied().find(|p| p.id == id)
}

/// The default provider — used when config.json has no `provider`
/// field. Currently always `MINIMAX`.
pub const DEFAULT_PROVIDER: &Provider = &MINIMAX;

/// The default plan table — re-exported as a convenience so
/// `Config::default()` can iterate `DEFAULT_PLANS` without referring
/// to a specific provider. Equivalent to `DEFAULT_PROVIDER.default_plans`;
/// kept as a separate constant for backwards compatibility with the
/// previous single-provider design.
pub const DEFAULT_PLANS: &[DefaultPlan] = MINIMAX.default_plans;
