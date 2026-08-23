//! Per-instance identity for multi-instance deployments.
//!
//! The binary can be launched multiple times concurrently, each as
//! its own tray icon targeting a different API endpoint (or the
//! same endpoint with different colors). Each instance is
//! identified by a name (the "instance name") which namespaces:
//!
//!   - config dir  → `~/.config/llm-quota-tray-<name>/config.json`
//!   - lock file   → `${XDG_RUNTIME_DIR}/llm-quota-tray-<name>.pid`
//!   - keyring app → `llm-quota-tray-<name>`
//!
//! The default instance (no `--instance=` flag) uses `llm-quota-tray`;
//! named instances append `-<name>`.
//!
//! ## Sources for the instance name
//!
//! 1. `--instance=<name>` CLI flag — preferred
//! 2. `QUOTA_INSTANCE=<name>` env var — fallback
//! 3. Otherwise empty (default instance, original paths)

use std::path::PathBuf;
use std::sync::OnceLock;

static INSTANCE_NAME: OnceLock<String> = OnceLock::new();

/// Parse CLI args + env, set the instance name. Called once from
/// `main()` before any path-sensitive code runs.
///
/// Returns the resolved instance name for convenience.
pub fn init() -> &'static str {
    let name = resolve();
    // set() takes a value (consumes it). We leak to get a
    // `&'static str` reference for the rest of the program — the
    // instance name is set once and never changes, so this is
    // effectively a one-time allocation per process.
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    let _ = INSTANCE_NAME.set(leaked.to_string());
    leaked
}

/// Resolve the instance name from CLI args + env (without storing).
/// Public so tests can exercise the resolution rules without
/// mutating global state.
pub fn resolve() -> String {
    parse(std::env::args().skip(1)).0
}

/// Full CLI parse: instance name + the `--set-key` one-shot flag.
/// Kept separate from `init()` so tests can exercise both fields
/// without mutating global state.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> (String, bool) {
    let mut iter = args.into_iter();
    let mut instance = String::new();
    let mut set_key = false;
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--instance=") {
            instance = rest.to_string();
            continue;
        }
        if arg == "--instance" {
            if let Some(v) = iter.next() {
                instance = v;
            }
            continue;
        }
        if arg == "--set-key" || arg == "--set_api_key" {
            set_key = true;
            continue;
        }
    }
    if instance.is_empty() {
        if let Ok(v) = std::env::var("QUOTA_INSTANCE") {
            instance = v;
        }
    }
    (instance, set_key)
}

/// True when the user passed `--set-key` on the command line. Used
/// by `main()` to short-circuit into the one-shot key-entry flow
/// before any daemon subsystems (lock, SNI, refresh loop) come up.
pub fn wants_set_key() -> bool {
    std::env::args()
        .skip(1)
        .any(|a| a == "--set-key" || a == "--set_api_key")
}

/// Look up the instance name. Returns "" for the default instance
/// (no namespace, original paths).
pub fn name() -> &'static str {
    INSTANCE_NAME.get().map(|s| s.as_str()).unwrap_or("")
}

/// Is this the default (un-namespaced) instance?
pub fn is_default() -> bool {
    name().is_empty()
}

/// The config dir basename, including instance suffix when set.
/// Used to build `~/.config/<basename>/config.json`.
pub fn config_dir_basename() -> String {
    if is_default() {
        "llm-quota-tray".to_string()
    } else {
        format!("llm-quota-tray-{}", name())
    }
}

/// Per-instance lock file path: `${XDG_RUNTIME_DIR}/<basename>.pid`.
pub fn lock_path() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime).join(format!("{}.pid", config_dir_basename()))
}

/// Per-instance keyring `application` attribute. Used by
/// `src/keyring.rs` to namespace Secret Service lookups so two
/// instances don't clobber each other's API key.
pub fn keyring_application() -> String {
    config_dir_basename()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(args: &[&str]) -> String {
        parse(args.iter().map(|s| s.to_string())).0
    }

    #[test]
    fn empty_args_no_env_returns_default() {
        // No CLI flag, no env → empty (default instance).
        // (We can't safely test the env fallback in isolation here
        // because QUOTA_INSTANCE may be set in the test environment.
        // `parse` alone covers CLI semantics; the env
        // branch is a one-liner that's covered by manual smoke
        // testing.)
        let s = resolve(&[]);
        assert_eq!(s, "");
    }

    #[test]
    fn cli_flag_equals_form() {
        assert_eq!(resolve(&["--instance=codex"]), "codex");
    }

    #[test]
    fn cli_flag_space_form() {
        assert_eq!(resolve(&["--instance", "openai"]), "openai");
    }

    #[test]
    fn cli_flag_space_form_missing_value_falls_through() {
        // `--instance` at the end with no value → no name.
        assert_eq!(resolve(&["--instance"]), "");
    }

    #[test]
    fn cli_flag_among_other_args() {
        assert_eq!(resolve(&["foo", "--instance=codex", "bar"]), "codex");
        assert_eq!(resolve(&["foo", "--instance", "codex", "bar"]), "codex");
    }

    #[test]
    fn config_dir_basename_default() {
        // The previous version of this test re-implemented the
        // function inline and asserted the output matched, which is
        // true by construction but does not actually test anything.
        // This version calls the real function and asserts exactly:
        // when the instance name is empty (the default), the basename
        // is exactly "llm-quota-tray" with no trailing dash or suffix.
        // INSTANCE_NAME is a OnceLock<String> and other tests in this
        // module may have set it to something else, so we can only
        // assert the default case here, not all cases.
        if is_default() {
            assert_eq!(
                config_dir_basename(),
                "llm-quota-tray",
                "default instance must produce basename 'llm-quota-tray'"
            );
        } else {
            // If another test left a non-default name in INSTANCE_NAME,
            // the helper must produce "llm-quota-tray-<name>".
            let n = name();
            assert!(
                !n.is_empty(),
                "non-default branch implies name() is non-empty"
            );
            assert_eq!(
                config_dir_basename(),
                format!("llm-quota-tray-{n}"),
                "named instance must produce basename 'llm-quota-tray-<name>'"
            );
        }
    }
}
