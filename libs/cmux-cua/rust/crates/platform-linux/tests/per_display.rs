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
        while matches!(stdout.read(&mut byte), Ok(1)) && byte[0] != b'\n' {
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

/// The tests run in parallel threads: the process env is set once, before
/// either reads it (setenv racing a getenv in x11rb or `import` is unsafe).
fn process_display_without_server() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // A process DISPLAY that names no server: an unscoped connect would fail.
        // (Under 59535: x11rb adds 6000 for the TCP port in a u16.)
        std::env::set_var("DISPLAY", ":4999");
        std::env::remove_var("WAYLAND_DISPLAY");
    });
}

fn displays() -> Option<(Xvfb, Xvfb)> {
    process_display_without_server();
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

/// The agent-cursor overlay's top-level windows on `display` (by title).
fn overlay_windows_on(display: &str) -> usize {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    let (conn, screen) = x11rb::rust_connection::RustConnection::connect(Some(display)).expect("connect");
    let root = conn.setup().roots[screen].root;
    let children = conn.query_tree(root).expect("query_tree").reply().expect("tree").children;
    children
        .into_iter()
        .filter(|&window| {
            conn.get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some_and(|reply| reply.value.starts_with(b"Cua.AgentCursorOverlay."))
        })
        .count()
}

#[test]
fn the_agent_cursor_of_a_scoped_call_shows_on_that_display() {
    let Some((small, large)) = displays() else { return };
    platform_linux::overlay::init(cursor_overlay::CursorConfig::default());
    platform_linux::overlay::run_on_thread();

    platform_linux::display::with_x11_display(&small.name, || {
        platform_linux::overlay::send_command_for(
            "per-display-cursor".to_owned(),
            cursor_overlay::OverlayCommand::SnapTo { x: 100.0, y: 100.0, heading_radians: None },
        );
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while overlay_windows_on(&small.name) == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(overlay_windows_on(&small.name), 1, "the cursor overlay opens on {}", small.name);
    assert_eq!(overlay_windows_on(&large.name), 0, "and only there");
}

/// Width and height of the first video stream in `path`.
fn video_size(path: &std::path::Path) -> Option<(u32, u32)> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (w, h) = text.trim().split_once(',')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

#[test]
fn a_recording_started_by_a_scoped_call_captures_that_display() {
    use cmux_cua_core::video::VideoBackendFactory;
    let Some((small, large)) = displays() else { return };
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        assert!(std::env::var_os("CMUX_CUA_REQUIRE_XVFB").is_none(), "ffmpeg is required but missing");
        eprintln!("skipped: ffmpeg is not available");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cmux-cua-per-display-video-{}", std::process::id()));
    for (display, size) in [(&small, (640, 480)), (&large, (800, 600))] {
        let path = dir.join(format!("{}.mp4", display.name.trim_start_matches(':')));
        let recorder = platform_linux::display::with_x11_display(&display.name, || {
            cmux_cua_core::video_ffmpeg::FfmpegVideoBackendFactory.start(&path)
        })
        .expect("start recording");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let stopped = recorder.stop();
        assert_eq!(video_size(&path), Some(size), "{}: {stopped:?}", display.name);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
