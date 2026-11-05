//! The waits for a person acting through cmux remote desktop (`platform_linux::person`),
//! on a real Xvfb with the root property `cmux-rd host` writes.
//!
//! Needs `Xvfb` on PATH. Without it the tests are skipped with a note, unless
//! `CMUX_CUA_REQUIRE_XVFB=1` (ci-rust-linux.yml).

#![cfg(target_os = "linux")]

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use platform_linux::person::{self, Busy, PERSON_PROPERTY};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// One Xvfb server on a free display number (`-displayfd`), killed on drop.
struct Xvfb {
    child: Child,
    name: String,
}

impl Xvfb {
    fn start() -> Option<Self> {
        let spawned = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-nolisten",
                "tcp",
                "-noreset",
                "-screen",
                "0",
                "800x600x24",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                assert!(
                    std::env::var_os("CMUX_CUA_REQUIRE_XVFB").is_none(),
                    "Xvfb is required: {e}"
                );
                eprintln!("skipped: no Xvfb ({e})");
                return None;
            }
        };
        let mut stdout = child.stdout.take()?;
        let mut number = String::new();
        let mut byte = [0u8; 1];
        while matches!(stdout.read(&mut byte), Ok(1)) && byte[0] != b'\n' {
            number.push(byte[0] as char);
        }
        Some(Self {
            child,
            name: format!(":{}", number.trim()),
        })
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The host's side: writes or deletes the property on its own connection.
struct Host {
    conn: RustConnection,
    root: Window,
    atom: u32,
}

impl Host {
    fn new(display: &str) -> Self {
        let (conn, screen) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen].root;
        let atom = conn
            .intern_atom(false, PERSON_PROPERTY.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        Self { conn, root, atom }
    }

    fn write(&self, pid: u32, window: Window, button_held: bool, last_input_unix_ms: u64) {
        let body = format!(
            r#"{{"v":1,"pid":{pid},"pointer":11,"keyboard":12,"window":{window},"button_held":{button_held},"last_input_unix_ms":{last_input_unix_ms}}}"#
        );
        self.conn
            .change_property8(
                PropMode::REPLACE,
                self.root,
                self.atom,
                AtomEnum::STRING,
                body.as_bytes(),
            )
            .unwrap();
        self.conn.sync().unwrap();
    }

    fn delete(&self) {
        self.conn.delete_property(self.root, self.atom).unwrap();
        self.conn.sync().unwrap();
    }

    /// A mapped toplevel at (100,100)-(300,300).
    fn window(&self) -> Window {
        let win = self.conn.generate_id().unwrap();
        self.conn
            .create_window(
                0,
                win,
                self.root,
                100,
                100,
                200,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap();
        self.conn.map_window(win).unwrap();
        self.conn.sync().unwrap();
        win
    }
}

fn keys(display: &str) -> Result<(), Busy> {
    platform_linux::display::with_x11_display(display, person::wait_for_keys)
}

fn press(display: &str, x: i32, y: i32) -> Result<(), Busy> {
    let (conn, screen) = x11rb::connect(Some(display)).unwrap();
    let root = conn.setup().roots[screen].root;
    person::wait_for_press(&conn, root, x, y)
}

#[test]
fn keys_wait_until_the_person_master_goes() {
    let Some(x) = Xvfb::start() else { return };
    let host = Host::new(&x.name);
    assert_eq!(keys(&x.name), Ok(()), "no marker, no wait");

    host.write(std::process::id(), 0, false, now_ms());
    let display = x.name.clone();
    let start = Instant::now();
    let waiter = std::thread::spawn(move || keys(&display));
    std::thread::sleep(Duration::from_millis(400));
    host.delete();
    assert_eq!(waiter.join().unwrap(), Ok(()));
    let waited = start.elapsed();
    assert!(waited >= Duration::from_millis(350), "waited {waited:?}");
    assert!(
        waited < Duration::from_secs(2),
        "woke on the change, not a timeout: {waited:?}"
    );

    // A host that is gone left its master: no point waiting.
    host.write(u32::MAX - 1, 0, false, now_ms());
    let start = Instant::now();
    assert_eq!(keys(&x.name), Err(Busy::Stale(u32::MAX - 1)));
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn keys_give_up_after_the_limit() {
    let Some(x) = Xvfb::start() else { return };
    let host = Host::new(&x.name);
    host.write(std::process::id(), 0, false, now_ms());
    let start = Instant::now();
    assert!(matches!(keys(&x.name), Err(Busy::StillActing(_))));
    assert!(start.elapsed() >= person::KEYS_WAIT);
}

#[test]
fn a_press_waits_only_for_the_window_the_person_uses() {
    let Some(x) = Xvfb::start() else { return };
    let host = Host::new(&x.name);
    let win = host.window();
    // The person holds a button in `win`.
    host.write(std::process::id(), win, true, now_ms() - 60_000);
    let start = Instant::now();
    assert_eq!(press(&x.name, 500, 500), Ok(()), "another window goes now");
    assert!(start.elapsed() < Duration::from_millis(300));

    // Same window: waits until the person lets go.
    let display = x.name.clone();
    let start = Instant::now();
    let waiter = std::thread::spawn(move || press(&display, 150, 150));
    std::thread::sleep(Duration::from_millis(300));
    host.write(std::process::id(), win, false, now_ms() - 60_000);
    assert_eq!(waiter.join().unwrap(), Ok(()));
    assert!(start.elapsed() >= Duration::from_millis(250));
    assert!(start.elapsed() < Duration::from_secs(2));

    // Recent input in that window: waits until it ages past ACTIVE_FOR, with no change
    // to the property.
    host.write(std::process::id(), win, false, now_ms());
    let start = Instant::now();
    assert_eq!(press(&x.name, 150, 150), Ok(()));
    let waited = start.elapsed();
    assert!(
        waited >= person::ACTIVE_FOR - Duration::from_millis(100),
        "waited {waited:?}"
    );
    assert!(waited < person::PRESS_WAIT, "waited {waited:?}");
}
