//! XDG Activation token plumbing.
//!
//! When the user launches the tray via the desktop shell (app
//! launcher, autostart, file manager → Open With), the shell
//! generates an [XDG Activation token][xdg-activation] and writes
//! it to one of:
//!
//! 1. `$XDG_ACTIVATION_TOKEN` env var — the canonical XDG route.
//! 2. `--token=<token>` CLI argument — passed by shells that
//!    substitute `%u` style placeholders. Our `.desktop` file
//!    declares `StartupNotify=true`, which causes compliant
//!    shells (GNOME, KDE) to provide the token on launch.
//!
//! The token is opaque and single-use (it expires when the launch
//! transition settles). We forward it to the freedesktop portals
//! (OpenURI, Notification) via their `activation_token` options
//! vardict key, so the portal dialogs and notifications animate
//! from the originating click instead of appearing to come from
//! nowhere.
//!
//! We never persist the token, never log it, and never hand it to
//! anything that isn't a portal call. Stale tokens (the user
//! clicked the chip an hour after launch) are simply ignored by
//! the receiving portal — no error surface for us.
//!
//! [xdg-activation]: https://specifications.freedesktop.org/xdg-activation/xdg-activation-latest.html

use std::sync::OnceLock;

static ACTIVATION_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Initialize the activation token. Call once from `main()` after
/// `instance::init()` (the instance parser also strips `--token=`,
/// so order matters).
///
/// Precedence (per the XDG Activation spec):
///   1. `--token=<token>` CLI flag
///   2. `$XDG_ACTIVATION_TOKEN` env var
///   3. None (no animation hook — portal dialogs appear without
///      a launch transition).
pub fn init() {
    let resolved = resolve(std::env::args().skip(1));
    let _ = ACTIVATION_TOKEN.set(resolved);
}

/// Resolve an activation token from CLI args + env. Factored out
/// from `init()` so tests can drive it without mutating the
/// process-wide `OnceLock` (which would short-circuit subsequent
/// `init()` calls and silently no-op the assertions).
pub fn resolve<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let from_cli = parse_cli_token(args);
    let from_env = std::env::var("XDG_ACTIVATION_TOKEN").ok();
    from_cli
        .or(from_env)
        .filter(|s| !s.is_empty())
}

/// Look up the activation token captured at `init()` time.
/// Returns `None` if no token was provided or if the token was an
/// empty string (the desktop spec treats empty tokens as "no
/// token").
pub fn get() -> Option<&'static str> {
    ACTIVATION_TOKEN
        .get()
        .and_then(|opt| opt.as_ref())
        .map(|s| s.as_str())
}

/// Parse `--token=<value>` out of the process args. Returns `None`
/// if the flag was absent or the value was empty.
///
/// The parser is intentionally narrow: it only matches the exact
/// `--token=` prefix and the `--token <value>` two-token form.
/// Anything else is left for other CLI handlers — matching too
/// greedily here would swallow unrelated flags.
fn parse_cli_token<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--token=") {
            return (!rest.is_empty()).then(|| rest.to_string());
        }
        if arg == "--token" {
            if let Some(v) = iter.next() {
                return (!v.is_empty()).then_some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `resolve()` reads `XDG_ACTIVATION_TOKEN` from the process
    /// env. Tests that set it must serialize so they don't race
    /// on the global env var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(value: Option<&str>, body: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("XDG_ACTIVATION_TOKEN").ok();
        match value {
            Some(v) => std::env::set_var("XDG_ACTIVATION_TOKEN", v),
            None => std::env::remove_var("XDG_ACTIVATION_TOKEN"),
        }
        body();
        match prev {
            Some(v) => std::env::set_var("XDG_ACTIVATION_TOKEN", v),
            None => std::env::remove_var("XDG_ACTIVATION_TOKEN"),
        }
    }

    #[test]
    fn parse_cli_token_dash_dash_equals_form() {
        let args: Vec<String> = vec!["--token=abc123".into()];
        assert_eq!(parse_cli_token(args), Some("abc123".to_string()));
    }

    #[test]
    fn parse_cli_token_space_separated_form() {
        let args: Vec<String> = vec!["--token".into(), "abc123".into()];
        assert_eq!(parse_cli_token(args), Some("abc123".to_string()));
    }

    #[test]
    fn parse_cli_token_absent_returns_none() {
        let args: Vec<String> = vec!["--instance=foo".into()];
        assert_eq!(parse_cli_token(args), None);
        let empty: Vec<String> = vec![];
        assert_eq!(parse_cli_token(empty), None);
    }

    #[test]
    fn parse_cli_token_empty_value_is_none() {
        // Per the spec, empty tokens are equivalent to no token.
        let a: Vec<String> = vec!["--token=".into()];
        assert_eq!(parse_cli_token(a), None);
        let b: Vec<String> = vec!["--token".into(), "".into()];
        assert_eq!(parse_cli_token(b), None);
    }

    #[test]
    fn parse_cli_token_among_other_flags() {
        // The flag must be discoverable in a realistic arg list —
        // autostart launches via the .desktop file pass several
        // unrelated flags first.
        let args: Vec<String> = vec![
            "--instance=codex".into(),
            "--set-key".into(),
            "--token=launch-xyz".into(),
        ];
        assert_eq!(parse_cli_token(args), Some("launch-xyz".to_string()));
    }

    #[test]
    fn resolve_prefers_cli_over_env() {
        with_env(Some("env-token"), || {
            let args: Vec<String> = vec!["--token=cli-token".into()];
            assert_eq!(resolve(args), Some("cli-token".to_string()));
        });
    }

    #[test]
    fn resolve_falls_back_to_env() {
        with_env(Some("env-token"), || {
            let args: Vec<String> = vec!["--instance=codex".into()];
            assert_eq!(resolve(args), Some("env-token".to_string()));
        });
    }

    #[test]
    fn resolve_returns_none_when_neither_set() {
        with_env(None, || {
            let args: Vec<String> = vec!["--instance=codex".into()];
            assert_eq!(resolve(args), None);
        });
    }

    #[test]
    fn resolve_treats_empty_env_as_no_token() {
        // An unset token via the env var must not be confused
        // with an empty one — both resolve to None.
        with_env(Some(""), || {
            let args: Vec<String> = vec!["--instance=codex".into()];
            assert_eq!(resolve(args), None);
        });
    }
}
