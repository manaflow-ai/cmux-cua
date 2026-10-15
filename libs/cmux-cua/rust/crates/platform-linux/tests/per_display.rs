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
            // -noreset: a server reset when the last client leaves clears
            // the root window's AT_SPI_BUS, as on a session host's displays.
            .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset", "-screen", "0"])
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

/// The overlay is process-global: start it once for every test here.
fn overlay_started() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        platform_linux::overlay::init(cursor_overlay::CursorConfig::default());
        platform_linux::overlay::run_on_thread();
    });
}

/// Waits up to 10 s for the overlay window count on `display` to reach `want`.
fn wait_for_overlay_windows(display: &str, want: usize) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while overlay_windows_on(display) != want && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    overlay_windows_on(display)
}

#[test]
fn the_agent_cursor_of_a_scoped_call_shows_on_that_display() {
    let Some((small, large)) = displays() else { return };
    overlay_started();

    platform_linux::display::with_x11_display(&small.name, || {
        platform_linux::overlay::send_command_for(
            "per-display-cursor".to_owned(),
            cursor_overlay::OverlayCommand::SnapTo { x: 100.0, y: 100.0, heading_radians: None },
        );
    });
    assert_eq!(wait_for_overlay_windows(&small.name, 1), 1, "the cursor overlay opens on {}", small.name);
    assert_eq!(overlay_windows_on(&large.name), 0, "and only there");
}

#[test]
fn a_display_that_is_gone_never_starves_the_others() {
    let Some((small, _large)) = displays() else { return };
    overlay_started();
    // A workspace whose display is gone (no server on :4998) keeps moving its
    // cursor, e.g. a long drag: more commands than the overlay's whole queue.
    platform_linux::display::with_x11_display(":4998", || {
        for step in 0..5000 {
            platform_linux::overlay::send_command_for(
                "cursor-on-a-closed-display".to_owned(),
                cursor_overlay::OverlayCommand::SnapTo { x: step as f64, y: 10.0, heading_radians: None },
            );
        }
    });
    platform_linux::display::with_x11_display(&small.name, || {
        platform_linux::overlay::send_command_for(
            "cursor-on-a-live-display".to_owned(),
            cursor_overlay::OverlayCommand::SnapTo { x: 100.0, y: 100.0, heading_radians: None },
        );
    });
    assert_eq!(wait_for_overlay_windows(&small.name, 1), 1, "a live display still gets its cursor overlay");
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

/// A GTK app on one display with its own session bus and AT-SPI bus, as the
/// session host gives each workspace (bus address published on the root
/// window): one button, and a line on stdout per click. The whole process
/// group is killed on drop.
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
            echo "launcher: ${launcher:-none}" >&2
            "$launcher" --launch-immediately &
            # The display's AT-SPI bus, for the test to publish on the root
            # window as the session host does (the distro launcher can't).
            for i in $(seq 100); do
                address=$(dbus-send --session --print-reply=literal --dest=org.a11y.Bus \
                    /org/a11y/bus org.a11y.Bus.GetAddress | tr -d ' ')
                [ -n "$address" ] && break
                sleep 0.1
            done
            echo "bus $address"
            read published
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(Self::log_path(name)).expect("app log"))
            .spawn()
            .expect("start dbus-run-session");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("app stdout"));
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Only the session's own lines: the AT-SPI registry daemon it
            // starts writes to the same stdout.
            let ours = |line: &str| ["bus ", "pid ", "clicked "].iter().any(|prefix| line.starts_with(prefix));
            for line in stdout.lines().map_while(Result::ok).filter(|line| ours(line)) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stdin = child.stdin.take().expect("app stdin");
        let mut app = Self { child, pid: 0, lines };
        let bus = app.line().unwrap_or_else(|| panic!("{name}'s session did not start on {display}{}", app.report(display, name)));
        let address = bus.strip_prefix("bus ").filter(|address| !address.is_empty());
        let address = address.unwrap_or_else(|| panic!("no AT-SPI bus for {name}: {bus:?}{}", app.report(display, name)));
        publish_atspi_bus(display, address);
        std::io::Write::write_all(&mut stdin, b"published\n").expect("resume the app session");
        let first = app.line().unwrap_or_else(|| panic!("{name} did not start on {display}{}", app.report(display, name)));
        app.pid = first.strip_prefix("pid ").and_then(|pid| pid.parse().ok()).expect("app pid line");
        app
    }

    fn log_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cmux-cua-per-display-{name}-{}.log", std::process::id()))
    }

    /// The session's stderr and processes, and the display's root properties.
    fn report(&self, display: &str, name: &str) -> String {
        let run = |program: &str, args: &[&str]| {
            Command::new(program)
                .args(args)
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
                .unwrap_or_else(|error| format!("{program}: {error}"))
        };
        let log = std::fs::read_to_string(Self::log_path(name)).unwrap_or_default();
        let group = self.child.id().to_string();
        format!(
            "\n--- stderr ---\n{log}\n--- processes ---\n{}\n--- root ---\n{}",
            run("ps", &["-o", "pid,args", "-g", &group]),
            run("xprop", &["-display", display, "-root"])
        )
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

/// Write the display's AT-SPI bus to its root window's `AT_SPI_BUS`, as the
/// session host does.
fn publish_atspi_bus(display: &str, address: &str) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
    use x11rb::wrapper::ConnectionExt as _;
    let (conn, screen) = x11rb::rust_connection::RustConnection::connect(Some(display)).expect("connect to the display");
    let root = conn.setup().roots[screen].root;
    let atom = conn.intern_atom(false, b"AT_SPI_BUS").expect("intern").reply().expect("atom").atom;
    conn.change_property8(PropMode::REPLACE, root, atom, AtomEnum::STRING, address.as_bytes())
        .expect("set AT_SPI_BUS");
    conn.get_input_focus().expect("sync").reply().expect("sync reply");
}

/// The app's accessibility tree as seen from `display`, once it shows
/// `button` (any tree when `None`).
fn tree_on(display: &str, pid: u32, button: Option<&str>) -> Option<Vec<platform_linux::atspi::AtspiNode>> {
    tree_or_why(display, pid, button).ok()
}

/// [`tree_on`], or what the last attempt saw: the walk's result and the
/// display's AT-SPI bus.
fn tree_or_why(display: &str, pid: u32, button: Option<&str>) -> Result<Vec<platform_linux::atspi::AtspiNode>, String> {
    let wait = button.is_some();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(if wait { 20 } else { 0 });
    loop {
        let tree = platform_linux::display::with_x11_display(display, || {
            platform_linux::atspi::native::walk_tree(pid)
        });
        let last = match tree {
            Ok(Some((_, nodes))) => {
                if button.map_or(true, |button| nodes.iter().any(|node| node.name.as_deref() == Some(button))) {
                    return Ok(nodes);
                }
                format!("tree without the button: {nodes:?}")
            }
            Ok(None) => "no tree for the pid".to_owned(),
            Err(error) => format!("walk failed: {error:#}"),
        };
        if std::time::Instant::now() >= deadline {
            let bus = platform_linux::display::with_x11_display(display, platform_linux::display::atspi_bus_address);
            return Err(format!("{last}; AT-SPI bus: {bus:?}"));
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
            tree_or_why(&display.name, app.pid, Some(&format!("{mine}-button")))
                .unwrap_or_else(|why| {
                    panic!("{mine}'s tree is not reachable on {}: {why}{}", display.name, app.report(&display.name, mine))
                })
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
