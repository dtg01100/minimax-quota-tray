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
//! ## Spec versions — dual implementation with auto-detection
//!
//! Two wire formats are in the wild:
//!
//! - **Original spec** (what `gnome-keyring-daemon` and the bulk of
//!   deployed daemons ship today):
//!   `OpenSession(s,v) → (v,o)`, `GetSecrets(ao,o) → a{o(...)}`,
//!   `Collection.CreateItem(a{sv}, (oayays), b) → (o,o)`,
//!   `Secret` shape `(ObjectPath, Byte[], Byte[], String)`.
//!   Attributes are set via `Properties.Set` after creation.
//!
//! - **New spec** (freedesktop.org spec rev 0.2, ~2020):
//!   `CreateSession(Algorithm{String}) → o`,
//!   `GetSecret(o) → Secret`, `Secret` shape `(ObjectPath, String,
//!   Byte[], String)`, `Collection.CreateItem(a{sv}, a{ss}, Secret,
//!   "replace") → (o,o)`.
//!
//! The daemon doesn't advertise a "version" property — the only
//! way to know which it supports is to introspect it. We call
//! `org.freedesktop.DBus.Introspectable.Introspect()` once per
//! process (cached) and grep for `CreateSession`:
//!
//! - If `CreateSession` is in the XML → daemon is new-spec.
//! - Otherwise → daemon is original-spec (the dominant case).
//!
//! Both code paths are kept around so we don't break older
//! daemons when newer-spec code lands.
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
use std::collections::HashMap;
use tokio::sync::OnceCell;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

/// Well-known bus name for the Secret Service.
const SS_BUS: &str = "org.freedesktop.secrets";

/// Well-known service object path.
const SS_SERVICE_PATH: &str = "/org/freedesktop/secrets";

/// Standard alias for the user's default collection. Read via
/// `ReadAlias("default")` to get the actual collection path.
/// ("session" is another common alias — sometimes the default
/// collection is unlocked for the entire session rather than
/// persisted; both should work via ReadAlias.)
const DEFAULT_ALIAS: &str = "default";

/// Interface names per the Secret Service spec.
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";
const SESSION_IFACE: &str = "org.freedesktop.Secret.Session";

/// Which Secret Service wire format the daemon speaks.
///
/// Detected via `Introspect()` once per process and cached. The
/// choice is locked in for the process lifetime — if a daemon
/// upgrades mid-session, we won't notice (the new connection
/// uses the cached value). Edge case, not worth the complexity
/// of re-probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonApi {
    /// New spec (rev 0.2): `CreateSession(Algorithm{String}) → o`,
    /// `GetSecret(o) → Secret`, secret shape `(osays)`,
    /// `CreateItem(a{sv}, a{ss}, Secret, "replace")`.
    New,
    /// Original spec: `OpenSession(s,v) → (v,o)`,
    /// `GetSecrets(ao,o) → a{o(...)}`, secret shape `(oayays)`,
    /// `CreateItem(a{sv}, Secret, b)`, attributes via
    /// `Properties.Set` after creation.
    Original,
}

impl DaemonApi {
    /// Human-readable label for log messages.
    fn label(self) -> &'static str {
        match self {
            DaemonApi::New => "new (rev 0.2)",
            DaemonApi::Original => "original",
        }
    }
}

/// Which API the daemon speaks, detected lazily on first use.
static DAEMON_API: OnceCell<DaemonApi> = OnceCell::const_new();

/// Per-instance `application` attribute used to namespace items
/// in the Secret Service collection.
fn attributes() -> HashMap<String, String> {
    let app = crate::instance::keyring_application();
    HashMap::from([("application".to_string(), app)])
}

/// Item Label — what shows up in `seahorse` and `ksecretserviceviewer`.
fn label() -> String {
    format!("{} API Key", crate::instance::config_dir_basename())
}

/// Session-bus connection, opened lazily on first use.
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

/// Detect which spec the daemon implements by looking at its
/// introspection XML for the presence of `CreateSession`.
async fn detect_api(conn: &Connection) -> DaemonApi {
    let svc = match ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await {
        Ok(p) => p,
        Err(_) => return DaemonApi::Original, // safe default
    };
    let xml = match svc.introspect().await {
        Ok(s) => s,
        Err(_) => return DaemonApi::Original, // safe default
    };
    if xml.contains("CreateSession") && xml.contains("GetSecret") {
        DaemonApi::New
    } else {
        DaemonApi::Original
    }
}

/// Cached detector — opens the session bus once, runs `detect_api`,
/// caches the answer.
async fn daemon_api() -> DaemonApi {
    let conn = match session_bus().await {
        Ok(c) => c,
        Err(_) => return DaemonApi::Original,
    };
    *DAEMON_API
        .get_or_init(|| async { detect_api(conn).await })
        .await
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Look up the API key. Priority:
///   1. Secret Service via session D-Bus.
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

/// Store the API key in Secret Service.
pub async fn set(value: &str) -> Result<()> {
    let conn = session_bus().await?;
    let api = daemon_api().await;
    log::debug!("Secret Service: using {} API", api.label());
    match api {
        DaemonApi::New => dbus_set_new(conn, value.as_bytes()).await,
        DaemonApi::Original => dbus_set_original(conn, value.as_bytes()).await,
    }
}

/// Remove the API key from Secret Service. Best-effort.
#[allow(dead_code)] // exposed for future "Clear stored key" menu item
pub async fn clear() -> Result<()> {
    dbus_clear().await
}

// ---------------------------------------------------------------------------
// Dispatcher (get): try detected API once; if it fails, fall back
// ---------------------------------------------------------------------------

async fn dbus_get() -> Option<String> {
    let conn = session_bus().await.ok()?;
    let api = daemon_api().await;

    // Try the detected API first. If it fails with UnknownMethod /
    // signature mismatch (the daemon upgrade-mid-session case, or a
    // daemon that lies in its introspection), fall back to the
    // other API. The fallback is bounded — we don't loop.
    let result = match api {
        DaemonApi::New => dbus_get_new(conn).await,
        DaemonApi::Original => dbus_get_original(conn).await,
    };
    match result {
        Ok(s) => s,
        Err(e) => {
            log::debug!(
                "Secret Service lookup via {} API failed: {e:#}",
                api.label()
            );
            // Try the other API once before giving up.
            let fallback_result = match api {
                DaemonApi::New => dbus_get_original(conn).await,
                DaemonApi::Original => dbus_get_new(conn).await,
            };
            match fallback_result {
                Ok(s) => s,
                Err(e2) => {
                    log::debug!("Secret Service fallback also failed: {e2:#}");
                    None
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// New spec implementation
// ---------------------------------------------------------------------------

async fn dbus_get_new(conn: &Connection) -> Result<Option<String>> {
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;

    // 1. CreateSession(Algorithm{String}) → ObjectPath
    //    (single-field struct wire signature `(s)`).
    let session: OwnedObjectPath = svc
        .call("CreateSession", &Algorithm("plain".into()))
        .await
        .context("CreateSession")?;

    // 2. SearchItems({application=...}) → (items, locked)
    let attrs = attributes();
    let (items, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = svc
        .call("SearchItems", &attrs)
        .await
        .context("SearchItems")?;
    if items.is_empty() && locked.is_empty() {
        return Ok(None);
    }

    // 3. Unlock locked items if any.
    if !locked.is_empty() {
        let to_unlock = locked.clone();
        let (unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
            svc.call("Unlock", &(to_unlock,)).await.context("Unlock")?;
        if prompt.as_str() != "/" {
            let ok = wait_for_prompt(conn, &prompt).await?;
            if !ok {
                anyhow::bail!("Unlock prompt dismissed");
            }
        }
        if unlocked.is_empty() {
            anyhow::bail!("Unlock returned no unlocked items");
        }
    }

    // 4. GetSecret(item) → Secret struct (osays)
    //    `(session: ObjectPath, parameters: String, value: Array<Byte>,
    //    content_type: String)`
    let item = items
        .first()
        .cloned()
        .or_else(|| locked.first().cloned())
        .context("no item to read")?;
    // GetSecret discards the session reply — we already have a
    // session for the read; the Secret's session field is just
    // metadata.
    let _ = session;

    let secret: NewSecret = svc
        .call("GetSecret", &(item.clone(),))
        .await
        .context("GetSecret")?;

    // `secret.2` is the value bytes per the new-spec shape
    // `(ObjectPath, String, Array<Byte>, String)`.
    Ok(Some(
        String::from_utf8(secret.2).context("non-UTF8 secret")?,
    ))
}

async fn dbus_set_new(conn: &Connection, value: &[u8]) -> Result<()> {
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;

    // 1. CreateSession.
    let session: OwnedObjectPath = svc
        .call("CreateSession", &Algorithm("plain".into()))
        .await
        .context("CreateSession")?;

    // 2. Default collection via ReadAlias.
    let collection: OwnedObjectPath = svc
        .call("ReadAlias", &(DEFAULT_ALIAS,))
        .await
        .context("ReadAlias(\"default\")")?;
    if collection.as_str() == "/" {
        anyhow::bail!("no default collection; user has no Secret Service collection set up");
    }

    // 3. CreateItem(properties, attributes, secret, replace_if_exists).
    //    New spec passes attributes as a parameter; replace is a
    //    String enum ("always" / "never" / "replace").
    let mut properties: HashMap<&'static str, Value<'_>> = HashMap::new();
    properties.insert(
        "org.freedesktop.Secret.Item.Label",
        Value::Str(label().into()),
    );
    properties.insert(
        "org.freedesktop.Secret.Item.Type",
        Value::Str("org.freedesktop.Secret.Generic".into()),
    );

    let attrs = attributes();
    let secret = NewSecret(
        session,
        String::new(),            // parameters (vestigial; empty for plain)
        value.to_vec(),           // value
        "text/plain".to_string(), // content_type
    );

    let coll = ss_proxy(conn, collection.as_str(), COLLECTION_IFACE).await?;
    let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = coll
        .call(
            "CreateItem",
            &(properties, attrs, secret, "replace".to_string()),
        )
        .await
        .context("CreateItem")?;
    if prompt.as_str() != "/" {
        let ok = wait_for_prompt(conn, &prompt).await?;
        if !ok {
            anyhow::bail!("CreateItem prompt dismissed");
        }
    }
    // Item is now created with attributes already set — no
    // Properties.Set follow-up needed (unlike the original API).
    let _ = item;
    Ok(())
}

// ---------------------------------------------------------------------------
// Original spec implementation
// ---------------------------------------------------------------------------

async fn dbus_get_original(conn: &Connection) -> Result<Option<String>> {
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;

    // 1. OpenSession(s, v) → (v, o)
    let session = open_session_original(conn, "plain").await?;

    // 2. SearchItems.
    let attrs = attributes();
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = svc
        .call("SearchItems", &attrs)
        .await
        .context("SearchItems")?;
    if unlocked.is_empty() && locked.is_empty() {
        return Ok(None);
    }

    // 3. Unlock.
    if !locked.is_empty() {
        let to_unlock = locked.clone();
        let (_unlocked_after, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
            svc.call("Unlock", &(to_unlock,)).await.context("Unlock")?;
        if prompt.as_str() != "/" {
            let ok = wait_for_prompt(conn, &prompt).await?;
            if !ok {
                anyhow::bail!("Unlock prompt dismissed by user");
            }
        }
    }

    // 4. GetSecrets(ao, o) → a{o(oayays)} (dict of item → Secret tuple).
    let mut all_items: Vec<OwnedObjectPath> = unlocked;
    all_items.extend(locked);
    let secrets: HashMap<OwnedObjectPath, OriginalSecret> = svc
        .call("GetSecrets", &(all_items, session.clone()))
        .await
        .context("GetSecrets")?;

    let (_item, secret) = secrets.into_iter().next().context("no secret returned")?;

    // `secret.2` is the value bytes per the original-spec shape
    // `(ObjectPath, Byte[], Byte[], String)`.
    Ok(Some(
        String::from_utf8(secret.2).context("non-UTF8 secret")?,
    ))
}

async fn dbus_set_original(conn: &Connection, value: &[u8]) -> Result<()> {
    // 1. OpenSession.
    let session = open_session_original(conn, "plain").await?;
    let session_path = zbus::zvariant::ObjectPath::from_str_unchecked(session.as_str());

    // 2. Default collection.
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;
    let collection: OwnedObjectPath = svc
        .call("ReadAlias", &(DEFAULT_ALIAS,))
        .await
        .context("ReadAlias(\"default\")")?;
    if collection.as_str() == "/" {
        anyhow::bail!("no default collection");
    }

    // 3. CreateItem(properties, secret, replace=true). No attributes
    //    arg in the original spec — set them via Properties.Set after.
    let mut properties: HashMap<&'static str, Value<'_>> = HashMap::new();
    properties.insert(
        "org.freedesktop.Secret.Item.Label",
        Value::Str(label().into()),
    );
    properties.insert(
        "org.freedesktop.Secret.Item.Type",
        Value::Str("org.freedesktop.Secret.Generic".into()),
    );

    let secret = OriginalSecret(
        OwnedObjectPath::from(session_path),
        Vec::new(),               // parameters (empty for plain)
        value.to_vec(),           // value (the secret bytes)
        "text/plain".to_string(), // content_type
    );

    let coll = ss_proxy(conn, collection.as_str(), COLLECTION_IFACE).await?;
    let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = coll
        .call("CreateItem", &(properties, secret, true))
        .await
        .context("CreateItem")?;
    if prompt.as_str() != "/" {
        let ok = wait_for_prompt(conn, &prompt).await?;
        if !ok {
            anyhow::bail!("CreateItem prompt dismissed");
        }
    }

    // 4. Set Attributes via Properties.Set (original-spec quirk:
    //    attributes aren't a CreateItem parameter, so we set them
    //    after the item exists).
    let item_proxy = ss_proxy(conn, item.as_str(), ITEM_IFACE).await?;
    let attrs = attributes();
    item_proxy
        .set_property("Attributes", attrs)
        .await
        .context("set Item.Attributes")?;

    Ok(())
}

async fn open_session_original(conn: &Connection, algorithm: &str) -> Result<OwnedObjectPath> {
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;
    let input = Value::Str("".into());
    let (_output, session): (OwnedValue, OwnedObjectPath) = svc
        .call("OpenSession", &(algorithm, input))
        .await
        .context("OpenSession")?;
    Ok(session)
}

// ---------------------------------------------------------------------------
// Clear (shared — both APIs use the same wire shape here)
// ---------------------------------------------------------------------------

#[allow(dead_code)] // see `clear()`
async fn dbus_clear() -> Result<()> {
    let conn = session_bus().await?;
    let svc = ss_proxy(conn, SS_SERVICE_PATH, SERVICE_IFACE).await?;

    let attrs = attributes();
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = svc
        .call("SearchItems", &attrs)
        .await
        .context("SearchItems")?;

    for item in unlocked.into_iter().chain(locked) {
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
// Wire types — both Secret shapes
// ---------------------------------------------------------------------------

/// Wire shape of the `Algorithm` struct from the new spec:
/// `struct Algorithm { String variant; }`. Single-field tuple
/// struct serializes to D-Bus `(s)`. Pin via the
/// `algorithm_signature` test below.
#[derive(Debug, serde::Serialize, zbus::zvariant::Type)]
#[zvariant(signature = "(s)")]
pub struct Algorithm(pub String);

/// Wire shape of `Secret` per the new spec rev 0.2:
/// `(Object_Path session, String parameters, Array<Byte> value,
/// String content_type)` → signature `(osays)`.
///
/// `OwnedObjectPath` is used for the session field rather than
/// `ObjectPath<'static>` because we hold onto this struct across
/// multiple awaits; borrowing from a `String` only works if the
/// source outlives the struct, which is awkward to express when
/// the source is a freshly-constructed D-Bus reply.
#[derive(Debug, Serialize, Deserialize, Type)]
pub struct NewSecret(pub OwnedObjectPath, pub String, pub Vec<u8>, pub String);

/// Wire shape of `Secret` per the original spec (what
/// `gnome-keyring-daemon` implements):
/// `(Object_Path session, Array<Byte> parameters, Array<Byte> value,
/// String content_type)` → signature `(oayays)`.
#[derive(Debug, Serialize, Deserialize, Type)]
pub struct OriginalSecret(pub OwnedObjectPath, pub Vec<u8>, pub Vec<u8>, pub String);

// Bring Serialize + Deserialize + Type into scope for the
// derives above (both used for both incoming and outgoing
// Secret values).
use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

// ---------------------------------------------------------------------------
// Prompt wait (shared)
// ---------------------------------------------------------------------------

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

/// Close a session object explicitly. Currently unused but exposed
/// for tests and completeness.
#[allow(dead_code)]
pub async fn close_session(session: OwnedObjectPath) -> Result<()> {
    let conn = session_bus().await?;
    let proxy = ss_proxy(conn, session.as_str(), SESSION_IFACE).await?;
    let _: () = proxy.call("Close", &()).await.context("Close")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
        // With a daemon reachable, `get()` consults it first and may
        // return whatever key is stored there. Without one, it falls
        // through to the env var. Either way, no panic — and the
        // function returns *some* value when both paths are reachable.
        with_env(Some("sk-from-env"), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(super::get());
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
        let attrs = attributes();
        assert!(attrs.contains_key("application"));
        assert!(attrs["application"].starts_with("llm-quota-tray"));
    }

    #[test]
    fn label_uses_instance_basename() {
        let l = label();
        assert!(l.starts_with("llm-quota-tray"));
        assert!(l.ends_with(" API Key"));
    }

    #[test]
    fn algorithm_signature_is_single_field_struct() {
        // The new-spec wire format for Algorithm is
        // `struct Algorithm { String variant; }` → D-Bus signature
        // `(s)`. Pinned because the original-spec OpenSession uses
        // a bare `s` for the algorithm, and the daemon distinguishes
        // them at the wire level.
        assert_eq!(format!("{}", Algorithm::SIGNATURE), "(s)");
    }

    #[test]
    fn original_secret_signature_matches_oayays() {
        // The original-spec Secret shape is
        // `(ObjectPath, Byte[], Byte[], String)`.
        assert_eq!(format!("{}", OriginalSecret::SIGNATURE), "(oayays)");
    }

    #[test]
    fn new_secret_signature_matches_osays() {
        // The new-spec Secret shape is
        // `(ObjectPath, String, Byte[], String)`.
        assert_eq!(format!("{}", NewSecret::SIGNATURE), "(osays)");
    }

    #[test]
    #[ignore = "requires a real session D-Bus with Secret Service daemon"]
    fn end_to_end_new_spec_roundtrip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let probe = format!("sk-new-api-{}", std::process::id());
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
    fn end_to_end_original_spec_roundtrip() {
        // Same flow as the new-spec test, but bypassing detection
        // and calling the original-spec code path directly. Useful
        // for confirming the fallback path is exercised end-to-end
        // even when the daemon advertises new-spec methods.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let probe = format!("sk-orig-api-{}", std::process::id());
            let conn = super::session_bus().await.expect("session bus");
            super::dbus_set_original(conn, probe.as_bytes())
                .await
                .expect("set (original)");
            let got = super::dbus_get_original(conn)
                .await
                .expect("get (original)")
                .expect("get returned None after set");
            assert_eq!(got, probe, "round-tripped secret must match");
            super::dbus_clear().await.expect("clear");
            let after = super::dbus_get_original(conn)
                .await
                .expect("get after clear");
            assert!(
                after.is_none() || after.as_deref() != Some(&probe),
                "secret was not cleared; got {:?}",
                after
            );
        });
    }
}
