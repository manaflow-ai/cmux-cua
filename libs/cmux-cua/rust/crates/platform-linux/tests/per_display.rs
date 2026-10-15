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

/// A GTK app on one display with its own session bus and AT-SPI bus, as the
/// session host gives each workspace: one button, and a line on stdout per
/// click. The whole process group is killed on drop.
struct App {
    child: Child,
    pid: u32,
    lines: std::sync::mpsc::Receiver<String>,
}

const APP: &str = r#"
import os, sys, gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk
name = sys.argv[1]
window = Gtk.Window(title="cmux-cua-" + name)
button = Gtk.Button(label=name + "-button")
button.connect("clicked", lambda _: print("clicked " + name, flush=True))
window.add(button)
window.show_all()
print("pid", os.getpid(), flush=True)
Gtk.main()
"#;

impl App {
    fn start(display: &str, name: &str) -> Self {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        // The launcher writes AT_SPI_BUS on this display's root window, where
        // both the app's toolkit and the daemon find the display's bus.
        let script = r#"
            for l in /usr/libexec/at-spi-bus-launcher /usr/lib/at-spi2-core/at-spi-bus-launcher; do
                [ -x "$l" ] && launcher="$l"
            done
            "$launcher" --launch-immediately &
            exec python3 -c "$CMUX_CUA_TEST_APP" "$1"
        "#;
        let mut child = Command::new("dbus-run-session")
            .args(["--", "sh", "-c", script, "sh", name])
            .env("DISPLAY", display)
            .env("CMUX_CUA_TEST_APP", APP)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("AT_SPI_BUS_ADDRESS")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start dbus-run-session");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("app stdout"));
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut app = Self { child, pid: 0, lines };
        let first = app.line().unwrap_or_else(|| panic!("{name} did not start on {display}"));
        app.pid = first.strip_prefix("pid ").and_then(|pid| pid.parse().ok()).expect("app pid line");
        app
    }

    fn line(&self) -> Option<String> {
        self.lines.recv_timeout(std::time::Duration::from_secs(20)).ok()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // "--" so the negative process-group id is not read as an option.
        let group = format!("-{}", self.child.id());
        let _ = Command::new("kill").args(["-s", "KILL", "--", &group]).status();
        let _ = self.child.wait();
    }
}

/// The app's accessibility tree as seen from `display`, once it shows
/// `button` (any tree when `None`).
fn tree_on(display: &str, pid: u32, button: Option<&str>) -> Option<Vec<platform_linux::atspi::AtspiNode>> {
    let wait = button.is_some();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(if wait { 20 } else { 0 });
    loop {
        let tree = platform_linux::display::with_x11_display(display, || {
            platform_linux::atspi::native::walk_tree(pid)
        });
        if let Ok(Some((_, nodes))) = tree {
            if button.map_or(true, |button| nodes.iter().any(|node| node.name.as_deref() == Some(button))) {
                return Some(nodes);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[test]
fn each_display_shows_and_drives_only_its_own_apps() {
    let Some((first, second)) = displays() else { return };
    let gtk = Command::new("python3")
        .args(["-c", "import gi; gi.require_version('Gtk', '3.0')"])
        .status()
        .is_ok_and(|status| status.success());
    let session = Command::new("dbus-run-session").arg("--version").output().is_ok();
    if !(gtk && session) {
        assert!(std::env::var_os("CMUX_CUA_REQUIRE_XVFB").is_none(), "dbus-run-session and python3-gi (GTK 3) are required");
        eprintln!("skipped: needs dbus-run-session and python3-gi (GTK 3)");
        return;
    }
    let alpha = App::start(&first.name, "alpha");
    let beta = App::start(&second.name, "beta");

    let trees: Vec<_> = [(&first, &alpha, "alpha"), (&second, &beta, "beta")]
        .into_iter()
        .map(|(display, app, mine)| {
            tree_on(&display.name, app.pid, Some(&format!("{mine}-button")))
                .unwrap_or_else(|| panic!("{mine}'s tree is not reachable on {}", display.name))
        })
        .collect();

    for ((display, app, mine, other), nodes) in
        [(&first, &alpha, "alpha", &beta), (&second, &beta, "beta", &alpha)].into_iter().zip(trees)
    {
        let button = nodes
            .iter()
            .find(|node| node.name.as_deref() == Some(&format!("{mine}-button")))
            .unwrap_or_else(|| panic!("no {mine}-button on {}: {nodes:?}", display.name));
        assert!(
            tree_on(&display.name, other.pid, None).is_none(),
            "{} shows an app from another display",
            display.name
        );

        let index = button.element_index.expect("the button is actionable");
        platform_linux::display::with_x11_display(&display.name, || {
            platform_linux::atspi::native::perform_action(app.pid, index)
        })
        .unwrap_or_else(|error| panic!("press {mine}-button: {error}"));
        assert_eq!(app.line().as_deref(), Some(format!("clicked {mine}").as_str()));
    }
    assert!(alpha.lines.try_recv().is_err() && beta.lines.try_recv().is_err(), "one click each");
}
