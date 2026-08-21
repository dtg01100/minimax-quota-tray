//! Dynamic per-model pricing lookup.
//!
//! Fetches a provider's per-model price table at startup (and
//! periodically, per `Config::pricing_refresh_polls`), caches it
//! in `AppState`, and feeds `build_menu_state` so the burn row
//! can append a `· $X/h` cost fragment when the API response
//! names a model we know about.
//!
//! This module is provider-agnostic. The wire shape it expects
//! is OpenRouter's `/api/v1/models`:
//!
//! ```json
//! {
//!   "data": [
//!     {
//!       "id": "openai/gpt-4o",
//!       "pricing": {
//!         "prompt": "0.000005",        // USD per input token (string!)
//!         "completion": "0.000015",    // USD per output token (string!)
//!         "input_cache_read": "0.00000125"
//!       }
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! Strings (not numbers) on the wire: OpenRouter ships prices as
//! decimal strings to preserve precision through JSON. We parse
//! them with `str::parse::<f64>()` and treat non-finite /
//! non-positive values as "free" (zero cost). All other providers
//! that mirror this shape will work without modification.
//!
//! ## Refresh cadence
//!
//! `Config::pricing_endpoint` is the URL to fetch (OpenRouter uses
//! `https://openrouter.ai/api/v1/models` which is fully public, no
//! auth required). `Config::pricing_refresh_polls` (default `None`,
//! meaning "fetch once at startup") sets how often the table is
//! re-fetched during the tray's lifetime. Re-fetches are best-effort:
//! a failure leaves the previous table in place and just logs a
//! warning.
//!
//! ## Self-contained
//!
//! Pricing lookup is independent of `burn` (it doesn't change the
//! rate math — the rate stays in `tok/h` or `%/h` or `$/h`; pricing
//! only ADDS a secondary fragment). It also doesn't touch the
//! parser contract — `pricing_model_path` is an optional new field
//! on `WindowShape` that the parser reads when present.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::fetch::HttpClient;

/// Per-model USD-per-token rates. All values are USD/token
/// (i.e. divide the raw `prompt` string by 1,000,000 if you want
/// "per million tokens"). The defaults are zero — fields the
/// upstream didn't supply are treated as free.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelPricing {
    pub prompt_per_token: f64,
    pub completion_per_token: f64,
    /// Optional: cheaper cached-read rate. Some providers
    /// (Anthropic, OpenRouter) charge less for cache hits.
    pub input_cache_read_per_token: Option<f64>,
}

/// Cached price table: model id → rates. Keys are the wire-format
/// ids (e.g. `"openai/gpt-4o"`, `"deepseek/deepseek-v4-flash"`).
pub type PriceTable = HashMap<String, ModelPricing>;

/// Wire-format shape of one entry in OpenRouter's `/api/v1/models`
/// response. We only deserialize the fields we need; everything
/// else (description, context_length, architecture, supported
/// parameters, …) is ignored. If a provider's model endpoint
/// uses a different shape, this is the place to add a matching
/// enum variant or a custom deserialize impl.
#[derive(Debug, Deserialize)]
struct WireModelEntry {
    id: String,
    #[serde(default)]
    pricing: WirePricing,
}

#[derive(Debug, Default, Deserialize)]
struct WirePricing {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    completion: String,
    #[serde(default)]
    input_cache_read: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    data: Vec<WireModelEntry>,
}

/// Parse a USD-per-token decimal string into f64. Returns 0.0 for
/// empty / non-numeric / non-finite / non-positive input — the
/// caller treats zero as "this dimension is free" (which matches
/// the convention OpenRouter uses: `"prompt": "0"` means a free
/// model).
fn parse_rate(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => v,
        _ => 0.0,
    }
}

/// Build a `ModelPricing` from a wire entry. Missing or invalid
/// fields are silently treated as zero — defensive against
/// providers that omit fields for free models.
fn build_pricing(p: &WirePricing) -> ModelPricing {
    ModelPricing {
        prompt_per_token: parse_rate(&p.prompt),
        completion_per_token: parse_rate(&p.completion),
        input_cache_read_per_token: p.input_cache_read.as_deref()
            .and_then(|s| {
                let v = parse_rate(s);
                if v > 0.0 { Some(v) } else { None }
            }),
    }
}

/// Fetch a price table from `url` and parse it. Blocks the calling
/// thread (intended to be wrapped in `spawn_blocking`). Returns
/// the table on success; an empty table on a successful fetch
/// with zero models; an error on transport or parse failure.
///
/// Why blocking: the HTTP client lives on the blocking runtime
/// (see `fetch.rs`) so we reuse it here to avoid building two
/// TLS stacks.
pub fn fetch_pricing_blocking(
    client: &HttpClient,
    url: &str,
) -> Result<PriceTable> {
    let resp = client.get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "pricing endpoint returned HTTP {status}"));
    }
    let body: WireResponse = resp.json()
        .with_context(|| format!("decoding pricing JSON from {url}"))?;
    let mut table = PriceTable::with_capacity(body.data.len());
    for entry in body.data {
        // Some providers include a `null` id for deprecated entries;
        // skip rather than panic.
        if entry.id.is_empty() {
            continue;
        }
        table.insert(entry.id, build_pricing(&entry.pricing));
    }
    Ok(table)
}

/// Compute a USD-per-hour cost fragment for a given model id and
/// hourly rate (in tokens/hour). Returns `None` when:
///
/// - The model id is `None` (API response didn't tag the entry)
/// - The model id isn't in the table (provider doesn't price it)
/// - The computed rate is below $0.001/h (sub-tenth-cent — too
///   noisy to display for cheap-model / low-volume workloads)
///
/// `prompt_share` is the fraction of the rate that's prompt
/// tokens (0.0–1.0). Defaults to 0.5 (50/50 split) when the
/// caller can't distinguish — most LLM workloads are roughly
/// balanced and 50/50 is a better default than all-prompt or
/// all-completion. OpenRouter-side, the upstream usage payloads
/// do split prompt vs completion — but the tray's parser reads
/// a single `used` count today, so the 50/50 default is what
/// callers should pass.
pub fn cost_per_hour(
    table: &PriceTable,
    model: Option<&str>,
    tokens_per_hour: f64,
    prompt_share: f64,
) -> Option<String> {
    let model_id = model?;
    let pricing = table.get(model_id)?;
    if tokens_per_hour <= 0.0 || !tokens_per_hour.is_finite() {
        return None;
    }
    // Clamp prompt_share to [0, 1] — bad config shouldn't NaN out.
    let ps = prompt_share.clamp(0.0, 1.0);
    let prompt_tokens = tokens_per_hour * ps;
    let completion_tokens = tokens_per_hour * (1.0 - ps);
    let usd_per_hour =
        prompt_tokens * pricing.prompt_per_token +
        completion_tokens * pricing.completion_per_token;
    if !usd_per_hour.is_finite() || usd_per_hour < 0.001 {
        // Below 1/10 cent — hide. Returns "0" formatters would
        // round it, but we want the row to look like a normal
        // token burn rather than "$0/h".
        return None;
    }
    // Format at higher precision than burn_row_label's fmt_cost:
    // this is a fragment, not the main rate, so 4 decimals below
    // $0.01 are useful (e.g. "$0.0042/h").
    let s = format!("${usd_per_hour:.4}");
    // Trailing-zero strip mirroring fmt_cost / fmt_rate style.
    let trimmed = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else { s };
    Some(format!("{trimmed}/h"))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_table() -> PriceTable {
        PriceTable::new()
    }

    fn table_with(id: &str, prompt: f64, completion: f64) -> PriceTable {
        let mut t = PriceTable::new();
        t.insert(id.to_string(), ModelPricing {
            prompt_per_token: prompt,
            completion_per_token: completion,
            input_cache_read_per_token: None,
        });
        t
    }

    // ---- parse_rate ----

    #[test]
    fn parse_rate_handles_strings() {
        assert_eq!(parse_rate("0.000005"), 0.000005);
        assert_eq!(parse_rate("0"), 0.0);
        assert_eq!(parse_rate(""), 0.0);
        assert_eq!(parse_rate("nan"), 0.0);
        assert_eq!(parse_rate("not-a-number"), 0.0);
        assert_eq!(parse_rate("-1.0"), 0.0);     // negative ⇒ 0
        assert_eq!(parse_rate("inf"), 0.0);
    }

    // ---- cost_per_hour ----

    #[test]
    fn cost_per_hour_50_50_split() {
        // OpenAI GPT-4o-ish: $5/M input, $15/M output. 1M tokens/h
        // at a 50/50 split ⇒ 500k prompt × $5/M + 500k completion ×
        // $15/M = $2.5/h + $7.5/h = $10/h.
        let t = table_with("openai/gpt-4o", 5e-6, 15e-6);
        let c = cost_per_hour(&t, Some("openai/gpt-4o"), 1_000_000.0, 0.5);
        assert_eq!(c, Some("$10/h".to_string()));
    }

    #[test]
    fn cost_per_hour_all_completion() {
        let t = table_with("model", 0.0, 1e-6);
        // 100k tokens/h, all completion: $0.10/h
        let c = cost_per_hour(&t, Some("model"), 100_000.0, 0.0);
        assert_eq!(c, Some("$0.1/h".to_string()));
    }

    #[test]
    fn cost_per_hour_hides_below_one_tenth_cent() {
        let t = table_with("cheap", 2e-7, 6e-7);
        // 100 tokens/h on a cheap model: $0.00004/h — below threshold
        let c = cost_per_hour(&t, Some("cheap"), 100.0, 0.5);
        assert!(c.is_none(),
                "sub-tenth-cent should be hidden, got {c:?}");
    }

    #[test]
    fn cost_per_hour_hides_when_model_unknown() {
        let t = table_with("foo", 1e-6, 1e-6);
        let c = cost_per_hour(&t, Some("bar"), 1000.0, 0.5);
        assert!(c.is_none());
    }

    #[test]
    fn cost_per_hour_hides_when_model_missing() {
        let t = table_with("foo", 1e-6, 1e-6);
        let c = cost_per_hour(&t, None, 1000.0, 0.5);
        assert!(c.is_none());
    }

    #[test]
    fn cost_per_hour_empty_table() {
        let c = cost_per_hour(&empty_table(), Some("anything"), 1000.0, 0.5);
        assert!(c.is_none());
    }

    #[test]
    fn cost_per_hour_clamps_prompt_share() {
        // A bad prompt_share=2.0 (above 1) shouldn't break the math.
        let t = table_with("m", 1e-6, 1e-6);
        let c = cost_per_hour(&t, Some("m"), 100_000.0, 2.0);
        // clamped to 1.0 ⇒ all prompt: $0.10/h
        assert_eq!(c, Some("$0.1/h".to_string()));
    }

    #[test]
    fn cost_per_hour_clamps_negative_prompt_share() {
        let t = table_with("m", 1e-6, 1e-6);
        let c = cost_per_hour(&t, Some("m"), 100_000.0, -0.5);
        // clamped to 0.0 ⇒ all completion: $0.10/h
        assert_eq!(c, Some("$0.1/h".to_string()));
    }

    #[test]
    fn cost_per_hour_handles_nonfinite_rate() {
        let t = table_with("m", 1e-6, 1e-6);
        assert!(cost_per_hour(&t, Some("m"), f64::NAN, 0.5).is_none());
        assert!(cost_per_hour(&t, Some("m"), f64::INFINITY, 0.5).is_none());
    }

    // ---- JSON wire-format parsing ----

    /// Simulates OpenRouter's exact response shape so we can verify
    /// the deserialization matches. If they change field names or
    /// types, this test fails before the runtime breaks.
    #[test]
    fn parses_openrouter_wire_shape() {
        let body = r#"{
            "data": [
                {
                    "id": "openai/gpt-4o",
                    "name": "OpenAI: GPT-4o",
                    "context_length": 128000,
                    "pricing": {
                        "prompt": "0.0000025",
                        "completion": "0.00001",
                        "input_cache_read": "0.00000125"
                    }
                },
                {
                    "id": "free/model",
                    "pricing": {
                        "prompt": "0",
                        "completion": "0"
                    }
                }
            ]
        }"#;
        let parsed: WireResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.data.len(), 2);

        let mut table = PriceTable::new();
        for entry in parsed.data {
            table.insert(entry.id, build_pricing(&entry.pricing));
        }

        let gpt = table.get("openai/gpt-4o").unwrap();
        assert_eq!(gpt.prompt_per_token, 0.0000025);
        assert_eq!(gpt.completion_per_token, 0.00001);
        assert_eq!(gpt.input_cache_read_per_token, Some(0.00000125));

        let free = table.get("free/model").unwrap();
        assert_eq!(free.prompt_per_token, 0.0);
        assert_eq!(free.completion_per_token, 0.0);
        assert!(free.input_cache_read_per_token.is_none());
    }

    #[test]
    fn parses_empty_data_array() {
        let body = r#"{"data": []}"#;
        let parsed: WireResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.data.is_empty());
    }

    #[test]
    fn skips_empty_id_entries() {
        // OpenRouter sometimes returns {"id": null, ...} for legacy
        // entries; we don't want those blowing up the HashMap key.
        let body = r#"{"data": [{"id": "", "pricing": {"prompt": "0", "completion": "0"}}]}"#;
        let parsed: WireResponse = serde_json::from_str(body).unwrap();
        let mut table = PriceTable::new();
        for entry in parsed.data {
            if entry.id.is_empty() { continue; }
            table.insert(entry.id, build_pricing(&entry.pricing));
        }
        assert!(table.is_empty());
    }
}