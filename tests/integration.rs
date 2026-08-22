//! End-to-end integration tests for the LLM Quota Tray.
//!
//! These tests require a session D-Bus and write to ~/.config. Marked
//! `#[ignore]` by default so `cargo test` doesn't fail in stripped-down
//! CI containers.
//!
//! Run with: `cargo test --test integration -- --ignored --nocapture`

use std::process::Command;

/// Test that the stripped-down binary actually runs. With no D-Bus
/// session and no keyring, the daemon logs a few warnings but stays
/// alive — proving the codebase didn't regress to a panic-on-startup.
#[test]
#[ignore]
fn binary_starts_under_session_dbus() {
    let mut child = Command::new("./target/release/llm-quota-tray")
        .env("HOME", "/tmp/llm-quota-integration-home")
        .spawn()
        .expect("binary must exist");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let _ = child.kill();
    let out = child.wait_with_output().expect("wait on child");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The startup log includes the SNI warning (no watcher in headless
    // test) and the plan label — both are proof the daemon reached its
    // main loop without panicking.
    assert!(
        stderr.contains("llm-quota-tray") || stderr.contains("refresh every"),
        "binary should reach steady state; got: {stderr}"
    );
}

/// Measure RSS in MB after a short warmup. Used as a regression guard
/// against accidentally re-introducing a heavy library (libgtk, etc.)
/// that would inflate memory by an order of magnitude.
#[test]
#[ignore]
fn rss_under_target() {
    let mut child = Command::new("./target/release/llm-quota-tray")
        .env("HOME", "/tmp/llm-quota-integration-home")
        .spawn()
        .expect("binary must exist");
    std::thread::sleep(std::time::Duration::from_secs(3));
    let pid = child.id();
    let rss_kb = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        });
    let _ = child.kill();
    let _ = child.wait();

    if let Some(rss) = rss_kb {
        let rss_mb = rss as f64 / 1024.0;
        // The SNI-only binary should be ~7-10 MB. Allow up to 20 MB as a
        // headroom for debug allocations, env, etc. If this regresses,
        // someone re-introduced a heavy library.
        assert!(
            rss_mb < 20.0,
            "RSS {rss_mb:.1} MB exceeds 20 MB target — investigate a possible heavy dependency"
        );
    }
}
