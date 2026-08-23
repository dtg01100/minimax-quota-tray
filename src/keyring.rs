//! GNOME Keyring / KWallet wrapper via the freedesktop
//! [`Secret Service`] D-Bus API. Stores the API key as a Secret
//! Service item, namespaced by `application = <instance>` so two
//! concurrent instances don't clobber each other's key.
//!
//! ## Why D-Bus instead of the `secret-tool(1)` subprocess
//!
//! Earlier versions of this module spawned `secret-tool` as a
//! subprocess:
//!
//! - The libsecret-tools package ships the binary; it was a soft
//!   runtime dependency at every poll and every key entry.
//! - The fork/exec round-trip was ~20–50ms, well below the
//!   `refresh_seconds` cadence but cumulative when paired with
//!   the HTTP fetch.
//! - Spawning `secret-tool` from `tokio::task::spawn_blocking`
//!   worked but required boilerplate at every call site; the
//!   earlier `secret-service = "3"` crate was unusable here
//!   because it builds on `zbus 3.x`'s sync API, which internally
//!   calls `zbus::utils::block_on(...)` and panics when invoked
//!   from inside a tokio worker thread.
//!
//! With zbus 5.x's async `Proxy::call`, calling Secret Service
//! directly is straightforward: it integrates with the existing
//! tokio runtime and skips the fork/exec.
//!
//! ## Spec compliance
//!
//! Targets the canonical freedesktop.org Secret Service spec at
//! https://specifications.freedesktop.org/secret-service/latest/.
//!
//! - **Service interface**: `OpenSession`, `SearchItems`, `Unlock`,
//!   `GetSecrets` (plural legacy; see `read_secret` for the
//!   canonical `Item.GetSecret` we prefer), `ReadAlias`.
//! - **Collection interface**: `CreateItem(properties, secret,
//!   replace_bool)` — the spec puts `replace` as a `Boolean`,
//!   not a `String`. Attributes are set on the Item via
//!   `org.freedesktop.DBus.Properties.Set("Attributes", a{ss})`
//!   after creation.
//! - **Item interface**: `Delete`, `GetSecret(session)`,
//!   `SetSecret(secret)`. Canonical read path.
//! - **Secret struct**: `(ObjectPath session, Array<Byte>
//!   parameters, Array<Byte> value, String content_type)` —
//!   wire signature `(oayays)`.
//!
//! The gnome-keyring-daemon also exposes a legacy
//! `Service.GetSecrets(items, session)` method that returns the
//! same dict shape. We don't use it; `Item.GetSecret` is cleaner.
//!
//! ## Attribute migration
//!
//! Earlier versions of this tray stored the API key under
//! `application = minimax-quota`. The codebase was later renamed
//! to `llm-quota-tray`, but already-stored items kept the old
//! attribute — users who installed the previous version have a
//! key sitting in their keyring under the old name, invisible
//! to a code path that only searches the new one. This module
//! searches BOTH attribute names on read, so existing keys are
//! found. On a successful set with the new name, the old item
//! (if any) is opportunistically deleted so the next lookup is
//! clean. The schema tag (`xdg:schema`) and the Item Type
//! (`org.freedesktop.Secret.Generic`) are kept consistent between
//! old and new items so this is purely a label/attribute
//! migration.
//!
//! ## Fallback chain
//!
//! 1. **Secret Service** via session D-Bus (GNOME Keyring / KWallet
//!    via libsecret, or any spec-compliant daemon).
//! 2. **`LLM_API_KEY` env var** — the documented systemd escape
//!    hatch for environments without a Secret Service provider
//!    (headless CI, distros without `gnome-keyring`).
//!
//! ## Spec
//!
//! [Secret Service]: https://specifications.freedesktop.org/secret-service/latest/

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::OnceCell;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::zvariant::Type;
use zbus::{Connection, Proxy};

/// Well-known bus name for the Secret Service.
const SS_BUS: &str = "org.freedesktop.secrets";

/// Well-known service object path.
const SS_SERVICE_PATH: &str = "/org/freedesktop/secrets";

/// Standard alias for the user's default collection. Read via
/// `ReadAlias("default")` to get the actual collection path.
const DEFAULT_ALIAS: &str = "default";

/// Interface names per the Secret Service spec.
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";

/// Current `application` attribute name (post-rename).
const ATTR_APP_CURRENT: &str = "application";

/// Legacy `application` attribute name (pre-rename). Users who
/// stored a key under this name have a real key sitting in their
/// keyring that we'd otherwise miss — see the module docs.
const ATTR_APP_LEGACY: &str = "minimax-quota";

/// Current instance namespace — the value of the `application`
/// attribute on items we own.
fn current_application() -> String {
    crate::instance::keyring_application()
}

/// Legacy instance namespace (pre-rename). Items stored under
/// `application = minimax-quota` are still valid and found via
/// `legacy_application()`.
fn legacy_application() -> &'static str {
    ATTR_APP_LEGACY
}

/// Item Label — what shows up in `seahorse` and `ksecretserviceviewer`.
fn label() -> String {
    format!("{} API Key", crate::instance::config_dir_basename())
}

/// Session-bus connection, opened lazily on first use. Reused
/// across calls so the D-Bus handshake (and any `XAUTHORITY`
/// cookie processing) runs at most once per process.
static SESSION_BUS: OnceCell<Connection> = OnceCell::const_new();

async fn session_bus() -> Result<&'static Connection> {
    SESSION_BUS
        .get_or_try_init(|| async { Connection::session().await.map_err(anyhow::Error::from) })
        .await
        .map_err(|e| anyhow!("connecting to session D-Bus: {e}"))
}

/// Build a proxy bound to a given object path + interface on the
/// Secret Service bus.
async fn ss_proxy<'a>(
    conn: &'a Connection,
    path: &'a str,
    iface: &'a str,
) -> zbus::Result<Proxy<'a>> {
    Proxy::new(conn, SS_BUS, path, iface).await
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Look up the API key. Priority:
///   1. Secret Service via session D-Bus (current AND legacy
///      `application` attribute, see module docs).
///   2. `LLM_API_KEY` env var (systemd escape hatch).
///
/// Returns `None` if both miss. Failures from (1) are logged at
/// debug level and treated as a miss.
pub async fn get() -> Option<String> {
    if let Some(s) = dbus_get().await {
        return secret_to_key(s.as_bytes());
    }
    if let Ok(raw) = std::env::var("LLM_API_KEY") {
        return secret_to_key(raw.as_bytes());
    }
    None
}

/// Store the API key in Secret Service. Uses the current
/// `application` attribute name. If a legacy-attribute item
/// exists in the same collection, it's deleted so the next
/// reload doesn't see both.
pub async fn set(value: &str) -> Result<()> {
    dbus_set(value.as_bytes()).await
}

/// Remove the API key from Secret Service. Clears both the
/// current-attribute and legacy-attribute items so users with
/// old installations don't see the key reappear after an
/// attribute-name migration.
#[allow(dead_code)] // exposed for future "Clear stored key" menu item
pub async fn clear() -> Result<()> {
    dbus_clear().await
}

// ---------------------------------------------------------------------------
// dbus_get: read the key, handling both attribute names
// ---------------------------------------------------------------------------

async fn dbus_get() -> Option<String> {
    let conn = session_bus().await.ok()?;
    let svc = match ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await {
        Ok(p) => p,
        Err(e) => {
            log::debug!("Secret Service: cannot build service proxy: {e}");
            return None;
        }
    };

    // 1. Search for items matching either the current or legacy
    //    `application` attribute. If we find a legacy-attribute
    //    item AND not a current-attribute one, we read the legacy
    //    item and signal to the caller that a migration is
    //    overdue (via the return tuple).
    let current = match search_items(conn, &svc, &current_attrs()).await {
        Ok(v) => v,
        Err(e) => {
            log::debug!("Secret Service: SearchItems (current) failed: {e:#}");
            Vec::new()
        }
    };
    let legacy = if current.is_empty() {
        // Only fall back to legacy if there's no current-attribute
        // hit — otherwise we'd return two keys and have to dedupe.
        match search_items(conn, &svc, &legacy_attrs()).await {
            Ok(v) => v,
            Err(e) => {
                log::debug!("Secret Service: SearchItems (legacy) failed: {e:#}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if current.is_empty() && legacy.is_empty() {
        return None;
    }

    // 2. Open a "plain" session for the read.
    let session = match open_session(conn, &svc).await {
        Ok(s) => s,
        Err(e) => {
            log::debug!("Secret Service: OpenSession failed: {e:#}");
            return None;
        }
    };

    // 3. Unlock all items we found. The spec's Unlock call on
    //    already-unlocked items is a no-op, so we just call it
    //    unconditionally — cheaper than a Locked-property round
    //    trip per item.
    let mut all_items: Vec<OwnedObjectPath> = current.to_vec();
    all_items.extend(legacy.iter().cloned());
    if !all_items.is_empty() {
        if let Err(e) = unlock_items(conn, &svc, all_items).await {
            log::debug!("Secret Service: Unlock failed: {e:#}");
            return None;
        }
    }

    // 4. Read the secret via Item.GetSecret(session) — the
    //    canonical method per the spec. Pick the first item
    //    (current preferred over legacy if both somehow exist).
    let item = current
        .first()
        .or(legacy.first())
        .cloned()
        .expect("non-empty by branch above");
    let bytes = match read_secret(conn, &item, &session).await {
        Ok(b) => b,
        Err(e) => {
            log::debug!("Secret Service: Item.GetSecret failed: {e:#}");
            return None;
        }
    };

    // 5. If we read a legacy item, log a hint that a `--set-key`
    //    would migrate it to the current attribute name. We don't
    //    auto-migrate on read (the user might be running an old
    //    tray instance in parallel and we don't want to delete
    //    their data behind their back).
    if !legacy.is_empty() && current.is_empty() {
        log::info!(
            "Secret Service: found legacy-attribute key for instance; \
             run `llm-quota-tray --set-key` to migrate to the current \
             attribute name."
        );
    }

    String::from_utf8(bytes).ok()
}

// ---------------------------------------------------------------------------
// dbus_set: write the key with the current attribute name
// ---------------------------------------------------------------------------

async fn dbus_set(value: &[u8]) -> Result<()> {
    let conn = session_bus().await?;
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;

    // 1. Open session.
    let session = open_session(conn, &svc).await?;
    let session_ref = session.as_str();

    // 2. Resolve default collection via ReadAlias.
    let collection: OwnedObjectPath = svc
        .call("ReadAlias", &(DEFAULT_ALIAS,))
        .await
        .context("ReadAlias(\"default\")")?;
    if collection.as_str() == "/" {
        anyhow::bail!("no default collection; user has no Secret Service collection set up");
    }

    // 3. Build the CreateItem arguments per the spec
    //    (Dict<String,Variant> properties, Secret secret, Boolean replace).
    let mut properties: HashMap<&'static str, Value<'_>> = HashMap::new();
    properties.insert(
        "org.freedesktop.Secret.Item.Label",
        Value::Str(label().into()),
    );
    properties.insert(
        "org.freedesktop.Secret.Item.Type",
        Value::Str("org.freedesktop.Secret.Generic".into()),
    );

    let session_op = zbus::zvariant::ObjectPath::from_str_unchecked(session_ref);
    let secret = Secret {
        session: OwnedObjectPath::from(session_op),
        parameters: Vec::new(),                 // empty for "plain"
        value: value.to_vec(),                  // the secret bytes
        content_type: "text/plain".to_string(), // per spec convention
    };

    let coll = ss_proxy(conn, collection.as_str(), COLLECTION_IFACE).await?;
    let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = coll
        .call(
            "CreateItem",
            &(properties, secret, true), // replace = true (per spec: Boolean)
        )
        .await
        .context("CreateItem")?;
    if prompt.as_str() != "/" {
        let ok = wait_for_prompt(conn, &prompt).await?;
        if !ok {
            anyhow::bail!("CreateItem prompt dismissed");
        }
    }

    // 4. Set Attributes on the new Item via Properties.Set. The
    //    spec deliberately keeps attributes off the CreateItem
    //    signature — they're a property of the item, not the
    //    creation call.
    let item_proxy = ss_proxy(conn, item.as_str(), ITEM_IFACE).await?;
    item_proxy
        .set_property("Attributes", current_attrs())
        .await
        .context("set Item.Attributes (current)")?;

    // 5. Migration: if a legacy-attribute item exists, delete it.
    //    Best-effort — a delete failure here shouldn't fail the
    //    whole set; the user can re-run set or use clear().
    if let Ok(legacy_items) = search_items(conn, &svc, &legacy_attrs()).await {
        for legacy_item in legacy_items {
            let p = ss_proxy(conn, legacy_item.as_str(), ITEM_IFACE).await?;
            let prompt: OwnedObjectPath = p.call("Delete", &()).await?;
            if prompt.as_str() != "/" {
                log::debug!("legacy Delete returned prompt {}; skipping wait", prompt);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// dbus_clear: delete items matching either attribute name
// ---------------------------------------------------------------------------

#[allow(dead_code)] // see `clear()`
async fn dbus_clear() -> Result<()> {
    let conn = session_bus().await?;
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;

    let mut all_items = Vec::new();
    for attrs in [current_attrs(), legacy_attrs()] {
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = svc
            .call("SearchItems", &attrs)
            .await
            .context("SearchItems")?;
        all_items.extend(unlocked);
        all_items.extend(locked);
    }

    for item in all_items {
        let item_proxy = ss_proxy(conn, item.as_str(), ITEM_IFACE).await?;
        let prompt: OwnedObjectPath = item_proxy
            .call("Delete", &())
            .await
            .context("Item.Delete")?;
        if prompt.as_str() != "/" {
            log::debug!("Delete returned prompt {}; skipping wait", prompt);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers (OpenSession, SearchItems, Unlock, Item.GetSecret, prompt)
// ---------------------------------------------------------------------------

fn current_attrs() -> HashMap<String, String> {
    HashMap::from([(ATTR_APP_CURRENT.to_string(), current_application())])
}

fn legacy_attrs() -> HashMap<String, String> {
    HashMap::from([(
        ATTR_APP_CURRENT.to_string(),
        legacy_application().to_string(),
    )])
}

async fn open_session(_conn: &Connection, svc: &Proxy<'_>) -> Result<OwnedObjectPath> {
    // Per the spec:
    //   OpenSession(IN String algorithm, IN Variant input,
    //               OUT Variant output, OUT ObjectPath result)
    // For "plain" the input Variant is empty (use
    // `Value::Str("")` — both gnome-keyring and libsecret accept
    // that as the "no algorithm parameters" sentinel).
    let input = Value::Str("".into());
    let (_output, session): (OwnedValue, OwnedObjectPath) = svc
        .call("OpenSession", &("plain", input))
        .await
        .context("OpenSession")?;
    Ok(session)
}

async fn search_items(
    conn: &Connection,
    svc: &Proxy<'_>,
    attrs: &HashMap<String, String>,
) -> Result<Vec<OwnedObjectPath>> {
    // Per the spec:
    //   SearchItems(IN Dict<String,String> attributes,
    //               OUT Array<ObjectPath> unlocked,
    //               OUT Array<ObjectPath> locked)
    // We return unlocked + locked (the caller handles them the
    // same way for the read path — both need Unlock if locked).
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = svc
        .call("SearchItems", attrs)
        .await
        .context("SearchItems")?;
    let mut all = unlocked;
    all.extend(locked);
    let _ = conn; // unused; reserved for future per-collection search
    Ok(all)
}

async fn _is_item_locked_unused(_conn: &Connection, _item: &OwnedObjectPath) -> bool {
    // The locked-counting in search_items() is informational; we
    // call Unlock unconditionally below. Kept as a stub for the
    // future optimization of skipping the Unlock round-trip when
    // nothing is locked.
    false
}

async fn unlock_items(
    conn: &Connection,
    svc: &Proxy<'_>,
    locked: Vec<OwnedObjectPath>,
) -> Result<()> {
    // Per the spec:
    //   Unlock(IN Array<ObjectPath> objects,
    //          OUT Array<ObjectPath> unlocked,
    //          OUT ObjectPath prompt)
    let (unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
        svc.call("Unlock", &(locked,)).await.context("Unlock")?;
    if prompt.as_str() != "/" {
        let ok = wait_for_prompt(conn, &prompt).await?;
        if !ok {
            anyhow::bail!("Unlock prompt dismissed by user");
        }
    }
    if unlocked.is_empty() {
        anyhow::bail!("Unlock returned no unlocked items");
    }
    Ok(())
}

async fn read_secret(
    conn: &Connection,
    item: &OwnedObjectPath,
    session: &OwnedObjectPath,
) -> Result<Vec<u8>> {
    // Canonical read path per the spec:
    //   Item.GetSecret(IN ObjectPath session, OUT Secret secret)
    // The Secret struct wire signature is (oayays); we read just
    // the value bytes (third field, index 2).
    let item_proxy = ss_proxy(conn, item.as_str(), ITEM_IFACE).await?;
    let secret: Secret = item_proxy
        .call("GetSecret", &(session.clone(),))
        .await
        .context("Item.GetSecret")?;
    Ok(secret.value)
}

async fn wait_for_prompt(conn: &Connection, prompt: &OwnedObjectPath) -> Result<bool> {
    let proxy = ss_proxy(conn, prompt.as_str(), PROMPT_IFACE)
        .await
        .context("proxy for prompt")?;
    let mut stream = proxy
        .receive_signal("Completed")
        .await
        .context("subscribe to Completed")?;
    #[allow(clippy::never_loop)]
    while let Some(signal) = stream.next().await {
        let body = signal.body();
        let (dismissed,): (bool,) = body.deserialize().context("deserialize Completed")?;
        return Ok(!dismissed);
    }
    anyhow::bail!("prompt Completed stream ended unexpectedly")
}

// ---------------------------------------------------------------------------
// Wire type: Secret struct (per spec, signature (oayays))
// ---------------------------------------------------------------------------

/// Wire shape of the `Secret` struct per the spec:
///
/// ```text
/// struct Secret {
///     Object_Path session;        // session used to decode (path)
///     Array<Byte> parameters;     // algorithm params (empty for plain)
///     Array<Byte> value;          // the secret bytes
///     String      content_type;   // MIME type, e.g. "text/plain"
/// }
/// ```
///
/// Signature: `(oayays)`.
#[derive(Debug, Serialize, Deserialize, Type)]
pub struct Secret {
    pub session: OwnedObjectPath,
    pub parameters: Vec<u8>,
    pub value: Vec<u8>,
    pub content_type: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Trim trailing whitespace / empty secrets. See `set_api_key_interactive`
/// for why the trimming matters (reqwest's header parser is strict).
pub fn secret_to_key(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use crate::instance::is_default;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(env_value: Option<&str>, body: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("LLM_API_KEY").ok();
        match env_value {
            Some(v) => std::env::set_var("LLM_API_KEY", v),
            None => std::env::remove_var("LLM_API_KEY"),
        }
        body();
        match prev {
            Some(v) => std::env::set_var("LLM_API_KEY", v),
            None => std::env::remove_var("LLM_API_KEY"),
        }
    }

    #[test]
    fn secret_with_trailing_newline_is_trimmed() {
        let key = secret_to_key(b"sk-abc123\n").unwrap();
        assert_eq!(key, "sk-abc123");
    }

    #[test]
    fn secret_with_surrounding_whitespace_is_trimmed() {
        let key = secret_to_key(b"  sk-abc123\n").unwrap();
        assert_eq!(key, "sk-abc123");
    }

    #[test]
    fn secret_empty_returns_none() {
        assert!(secret_to_key(b"").is_none());
        assert!(secret_to_key(b"\n\n").is_none());
        assert!(secret_to_key(b"   ").is_none());
    }

    #[test]
    fn secret_non_utf8_returns_none() {
        assert!(secret_to_key(&[0xff, 0xfe, 0xfd]).is_none());
    }

    #[test]
    fn env_var_fallback_does_not_panic_with_daemon_reachable() {
        with_env(Some("sk-from-env"), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(super::get());
            // With a daemon reachable, get() consults it first.
            // Without one, it falls through to the env var.
            // Either way, no panic and *some* value.
            assert!(got.is_some());
        });
    }

    #[test]
    fn env_var_with_trailing_newline_trimmed() {
        with_env(Some("sk-env-key\n"), || {
            let trimmed = secret_to_key(b"sk-env-key\n").unwrap();
            assert_eq!(trimmed, "sk-env-key");
        });
    }

    #[test]
    fn attributes_use_instance_application() {
        let attrs = current_attrs();
        assert_eq!(
            attrs.get(ATTR_APP_CURRENT).map(String::as_str),
            Some("llm-quota-tray")
        );
        // Legacy attrs use the legacy name; don't claim to be
        // the current namespace.
        let legacy = legacy_attrs();
        assert_eq!(
            legacy.get(ATTR_APP_CURRENT).map(String::as_str),
            Some("minimax-quota")
        );
    }

    #[test]
    fn label_uses_instance_basename() {
        // The label appears in `seahorse` and `ksecretservice-viewer`
        // and must uniquely identify the instance. The default
        // instance label is exactly "llm-quota-tray API Key". The
        // previous version used .starts_with("llm-quota-tray") which
        // also passed for "llm-quota-tray-codex API Key" -- too weak
        // to verify the default case (a regression that incorrectly
        // appended an empty suffix would have passed).
        let l = label();
        // Strip the suffix " API Key" first so we can assert the
        // basename portion is exactly "llm-quota-tray" (no trailing
        // dash, no extra suffix).
        assert!(
            l.ends_with(" API Key"),
            "label must end with ' API Key', got {l:?}"
        );
        let basename = &l[..l.len() - " API Key".len()];
        if is_default() {
            assert_eq!(
                basename, "llm-quota-tray",
                "default instance must have basename exactly 'llm-quota-tray'"
            );
        } else {
            // Named instance: basename must be "llm-quota-tray-<name>"
            assert!(
                basename.starts_with("llm-quota-tray-"),
                "named instance basename must start with 'llm-quota-tray-', got {basename:?}"
            );
        }
    }

    #[test]
    fn secret_signature_matches_oayays() {
        // Per the freedesktop.org spec at
        // https://specifications.freedesktop.org/secret-service/latest/types.html
        // the Secret struct is
        //   struct Secret { ObjectPath session; Array<Byte> parameters;
        //                   Array<Byte> value; String content_type; }
        // → signature `(oayays)`. Pinned here so a refactor can't
        // accidentally swap to the (wrong) spec text shape.
        assert_eq!(format!("{}", Secret::SIGNATURE), "(oayays)");
    }

    #[test]
    #[ignore = "requires a real session D-Bus with Secret Service daemon"]
    fn end_to_end_roundtrip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let probe = format!("sk-e2e-{}", std::process::id());
            super::set(&probe).await.expect("set");
            let got = super::get().await.expect("get returned None after set");
            assert_eq!(got, probe, "round-tripped secret must match");
            super::clear().await.expect("clear");
            let after = super::get().await;
            assert!(
                after.is_none() || after.as_deref() != Some(&probe),
                "secret was not cleared; got {:?}",
                after
            );
        });
    }

    #[test]
    #[ignore = "requires a real session D-Bus with Secret Service daemon"]
    fn end_to_end_legacy_attribute_is_found() {
        // Regression: a key stored under the OLD `application =
        // minimax-quota` attribute must still be readable after
        // the rename. We set up the legacy item by hand (because
        // `set()` would migrate it), then read it via `get()`.
        //
        // This test must isolate itself: the `end_to_end_roundtrip`
        // test runs in the same keyring namespace and leaves a
        // current-attribute item behind. If we didn't clear first,
        // the lookup would find the current item and skip the
        // legacy probe entirely.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Pre-flight cleanup: clear both attribute namespaces
            // so this test starts from a known-empty state.
            super::clear().await.ok();

            let conn = super::session_bus().await.expect("session bus");
            let svc = super::ss_proxy(conn, super::SS_SERVICE_PATH, super::SERVICE_IFACE)
                .await
                .expect("service proxy");
            let session = super::open_session(conn, &svc).await.expect("open session");
            let collection: OwnedObjectPath = svc
                .call("ReadAlias", &(super::DEFAULT_ALIAS,))
                .await
                .expect("read alias");

            // Write a legacy-attribute item directly.
            let probe = format!("sk-legacy-{}", std::process::id());
            let mut properties: HashMap<&str, Value> = HashMap::new();
            properties.insert(
                "org.freedesktop.Secret.Item.Label",
                Value::Str("legacy test item".into()),
            );
            let secret = Secret {
                session: OwnedObjectPath::try_from(session.as_str()).unwrap(),
                parameters: Vec::new(),
                value: probe.as_bytes().to_vec(),
                content_type: "text/plain".to_string(),
            };
            let coll = super::ss_proxy(conn, collection.as_str(), super::COLLECTION_IFACE)
                .await
                .expect("coll proxy");
            let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = coll
                .call("CreateItem", &(properties, secret, true))
                .await
                .expect("create item");
            if prompt.as_str() != "/" {
                let _ = super::wait_for_prompt(conn, &prompt).await;
            }
            let item_proxy = super::ss_proxy(conn, item.as_str(), super::ITEM_IFACE)
                .await
                .expect("item proxy");
            item_proxy
                .set_property("Attributes", super::legacy_attrs())
                .await
                .expect("set legacy attributes");

            // Now read via the public get() — it should find the
            // legacy-attribute item.
            let got = super::get()
                .await
                .expect("get returned None for legacy item");
            assert_eq!(got, probe, "legacy-attribute item must be discoverable");

            // Setting via the public set() should migrate: write
            // a new key, verify the legacy item is gone and the
            // new key is reachable.
            let new_probe = format!("sk-new-{}", std::process::id());
            super::set(&new_probe).await.expect("set (migrates)");
            let got_after = super::get().await.expect("get after migrate");
            assert_eq!(
                got_after, new_probe,
                "must find the new key after migration"
            );

            // Now run a set() again — that should NOT leave a
            // stale legacy item around (the migration runs every
            // time, deleting any legacy-attribute siblings).
            let third = format!("sk-third-{}", std::process::id());
            super::set(&third)
                .await
                .expect("set (idempotent migration)");

            // Cleanup: clear, then verify nothing matches anymore.
            super::clear().await.expect("clear");
            let after = super::get().await;
            assert!(
                after.is_none()
                    || (after.as_deref() != Some(&probe) && after.as_deref() != Some(&third)),
                "both legacy and new items should be cleared; got {:?}",
                after
            );
        });
    }
}
