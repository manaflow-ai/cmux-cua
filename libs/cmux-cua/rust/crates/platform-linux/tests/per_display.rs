//! Per-display targeting (computer-use plan step i): a call scoped to an X11
//! display reaches that display, not the process `DISPLAY`, so one daemon can
//! serve several workspaces, each with its own headless display.
//!
//! Needs `Xvfb` on PATH. Without it the test is skipped with a note, unless
//! `CMUX_CUA_REQUIRE_XVFB=1` (the "per-display" step of ci-rust-linux.yml).

#![cfg(target_os = "linux")]

use std::io::Read;
use std::process::{Child, Command, Stdio};

/// One Xvfb server on a free display number (`-displayfd`), killed on drop.
struct Xvfb {
    child: Child,
    name: String,
}

impl Xvfb {
    fn start(width: u32, height: u32) -> Option<Self> {
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-nolisten", "tcp", "-screen", "0"])
            .arg(format!("{width}x{height}x24"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Xvfb writes the display number and a newline once it accepts clients.
        let mut stdout = child.stdout.take()?;
        let mut number = String::new();
        let mut byte = [0u8; 1];
        while stdout.read(&mut byte).ok()? == 1 && byte[0] != b'\n' {
            number.push(byte[0] as char);
        }
        let number = number.trim().to_owned();
        if number.is_empty() {
            let _ = child.kill();
            return None;
        }
        Some(Self { child, name: format!(":{number}") })
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn displays() -> Option<(Xvfb, Xvfb)> {
    let small = Xvfb::start(640, 480);
    let large = Xvfb::start(800, 600);
    match (small, large) {
        (Some(small), Some(large)) => Some((small, large)),
        _ => {
            assert!(std::env::var_os("CMUX_CUA_REQUIRE_XVFB").is_none(), "Xvfb is required but did not start");
            eprintln!("skipped: Xvfb is not available");
            None
        }
    }
}

#[test]
fn a_scoped_call_captures_its_own_display_not_the_process_display() {
    let Some((small, large)) = displays() else { return };
    // A process DISPLAY that names no server: an unscoped connect would fail.
    std::env::set_var("DISPLAY", ":65000");
    std::env::remove_var("WAYLAND_DISPLAY");

    let (_, w, h) = platform_linux::display::with_x11_display(&small.name, || {
        platform_linux::capture::screenshot_display()
    })
    .expect("capture the small display");
    assert_eq!((w, h), (640, 480));

    let (_, w, h) = platform_linux::display::with_x11_display(&large.name, || {
        platform_linux::capture::screenshot_display()
    })
    .expect("capture the large display");
    assert_eq!((w, h), (800, 600));

    // Scopes nest and unwind: the inner call sees its own display, the outer
    // scope is restored after it, and nothing leaks past the outer scope.
    platform_linux::display::with_x11_display(&small.name, || {
        let inner = platform_linux::display::with_x11_display(&large.name, || {
            platform_linux::display::x11_display()
        });
        assert_eq!(inner.as_deref(), Some(large.name.as_str()));
        assert_eq!(platform_linux::display::x11_display().as_deref(), Some(small.name.as_str()));
    });
    assert_eq!(platform_linux::display::x11_display(), None);
    assert!(platform_linux::capture::screenshot_display().is_err(), "unscoped calls keep using the process DISPLAY");
}

#[test]
fn a_tool_call_scoped_to_a_display_runs_on_that_display() {
    let Some((small, large)) = displays() else { return };
    std::env::set_var("DISPLAY", ":65000");
    std::env::remove_var("WAYLAND_DISPLAY");
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let registry = platform_linux::tools::build_registry(false);
    // get_screen_size does its X11 work on a blocking thread, so the target
    // has to cross from the call's task to that thread.
    for (display, size) in [(&small, "640x480"), (&large, "800x600")] {
        let result = runtime.block_on(platform_linux::display::scope(
            Some(display.name.clone()),
            registry.invoke("get_screen_size", serde_json::json!({})),
        ));
        let text = format!("{result:?}");
        assert!(text.contains(size), "{}: {text}", display.name);
    }
}
