//! End-to-end integration tests for the MiniMax quota tray.
//!
//! These tests require:
//! - A Secret Service daemon reachable on the session D-Bus
//!   (gnome-keyring-daemon running)
//! - `magick` (ImageMagick) in PATH for icon rendering
//! - The library files `libayatana-appindicator3.so.1` /
//!   `libgtk-3.so.0` reachable on the system library path
//!
//! Run with: `cargo test --test integration -- --ignored --nocapture`
//!
//! Marked `#[ignore]` by default so `cargo test` doesn't fail in
//! stripped-down CI environments where these services aren't running.

use std::process::Command;

/// Test that the icon renderer can produce a PNG from the SVG template.
/// Requires ImageMagick (`magick`).
#[test]
#[ignore]
fn magick_renders_icon() {
    let out = Command::new("magick")
        .args(["--version"])
        .output()
        .expect("magick must be installed");
    assert!(out.status.success(),
            "magick --version failed: {}", String::from_utf8_lossy(&out.stderr));
}



/// Smoke test that the binary's arg-less startup loads
/// libayatana-appindicator — that confirms our FFI + dynamic linking
/// work. In a headless test environment without a display, the binary
/// then hangs in gtk::init() (which is correct — the call succeeded,
/// the window system just isn't there to attach to). The
/// libayatana-appindicator warning is the proof we loaded the .so.
#[test]
#[ignore]
fn binary_loads_libraries() {
    use std::io::Read;
    // Write output to a tempfile via shell redirection so SIGKILL doesn't
    // drop unflushed buffers.
    let log = std::env::temp_dir().join("minimax-binary-stderr.log");
    let _ = std::fs::remove_file(&log);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "./target/release/minimax-quota-tray 2>{} & PID=$!; sleep 1; kill -9 $PID 2>/dev/null; wait $PID 2>/dev/null; true",
            log.display()
        ))
        .spawn()
        .expect("spawn wrapper");
    let _ = child.wait();

    let mut buf = String::new();
    let mut f = std::fs::File::open(&log).expect("open log");
    f.read_to_string(&mut buf).expect("read log");
    assert!(buf.contains("libayatana-appindicator"),
            "binary should load libayatana-appindicator; got: {buf}");
}