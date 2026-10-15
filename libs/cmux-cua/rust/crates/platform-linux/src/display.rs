//! Which X11 display a call targets.
//!
//! By default every X11 connection, Xlib display and X client subprocess uses
//! the process `DISPLAY`, so one daemon drives one display. The computer-use
//! plan's step i gives each workspace its own headless display under one
//! daemon (one activity store, one pane), so a call can name its display:
//! [`scope`] for an async tool call (the daemon wraps each call in it),
//! [`with_x11_display`] for the current thread. The X11 paths in this crate
//! resolve it through [`connect`], [`open_xlib`] and [`command`].
//!
//! Tools run their X11 work on blocking threads: [`spawn_blocking`] carries
//! the target there. A plain `std::thread::spawn` does not inherit it; capture
//! [`x11_display`] first and re-enter [`with_x11_display`] in the thread.

use std::cell::RefCell;
use std::ffi::CString;
use std::process::Command;

use x11rb::rust_connection::RustConnection;

thread_local! {
    static TARGET: RefCell<Option<String>> = const { RefCell::new(None) };
}

tokio::task_local! {
    static TASK_TARGET: Option<String>;
}

/// Run the future (a tool call) with its X11 calls aimed at `display`, or at
/// the process `DISPLAY` when `None`.
pub async fn scope<F: std::future::Future>(display: Option<String>, future: F) -> F::Output {
    TASK_TARGET.scope(display, future).await
}

/// `tokio::task::spawn_blocking` that keeps the caller's display target.
pub fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let display = x11_display();
    tokio::task::spawn_blocking(move || match display {
        Some(display) => with_x11_display(&display, f),
        None => f(),
    })
}

/// `std::thread::spawn` that keeps the caller's display target.
pub fn spawn_thread<F, R>(f: F) -> std::thread::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let display = x11_display();
    std::thread::spawn(move || match display {
        Some(display) => with_x11_display(&display, f),
        None => f(),
    })
}

/// Run `f` with X11 calls on this thread aimed at `display` (e.g. `":5"`).
/// Scopes nest; the previous target comes back when `f` returns or unwinds.
pub fn with_x11_display<R>(display: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            TARGET.with(|target| *target.borrow_mut() = previous);
        }
    }
    let previous = TARGET.with(|target| target.replace(Some(display.to_owned())));
    let _restore = Restore(previous);
    f()
}

/// The display this call targets (thread scope first, then the task's), or
/// `None` for the process `DISPLAY`. A thread scope wins over a task scope
/// entered inside it (`with_x11_display(a, || block_on(scope(b, ..)))` is
/// `a`); nest them the other way round.
pub fn x11_display() -> Option<String> {
    TARGET
        .with(|target| target.borrow().clone())
        .or_else(|| TASK_TARGET.try_with(|target| target.clone()).ok().flatten())
}

/// The display name an X11 call on this thread uses: the target, else the
/// process `DISPLAY`. For lock-file lookups and error messages.
pub fn effective_x11_display() -> Option<String> {
    x11_display().or_else(|| std::env::var("DISPLAY").ok())
}

/// True when a call names its display. A named display is X11, so the native
/// Wayland paths step aside for it.
pub fn is_targeted() -> bool {
    x11_display().is_some()
}

/// An x11rb connection to this thread's display.
pub fn connect() -> Result<(RustConnection, usize), x11rb::errors::ConnectError> {
    RustConnection::connect(x11_display().as_deref())
}

/// An Xlib `Display` for this thread's display; null when it cannot be opened.
pub fn open_xlib() -> *mut x11::xlib::Display {
    match x11_display().and_then(|name| CString::new(name).ok()) {
        Some(name) => unsafe { x11::xlib::XOpenDisplay(name.as_ptr()) },
        None => unsafe { x11::xlib::XOpenDisplay(std::ptr::null()) },
    }
}

/// A subprocess that talks to this thread's display (an X client such as
/// ImageMagick's `import`, or an app the agent launches).
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    if let Some(name) = x11_display() {
        command.env("DISPLAY", name).env_remove("WAYLAND_DISPLAY");
        // AT_SPI_BUS_ADDRESS wins over every other lookup, so the app joins
        // the display's AT-SPI bus even on its process's session bus.
        match atspi_bus_address() {
            Ok(Some(address)) => command.env("AT_SPI_BUS_ADDRESS", address),
            _ => command.env_remove("AT_SPI_BUS_ADDRESS"),
        };
    }
    command
}

/// The AT-SPI bus of the display this call targets, from the root window's
/// `AT_SPI_BUS` property, where toolkits also look. An `at-spi-bus-launcher`
/// built with X11 writes it; Debian and Ubuntu build theirs without, so the
/// session host writes it after starting the display's launcher. `None` when
/// the call targets no display. A targeted display without one is an error,
/// never the process's bus, which would show another display's apps.
pub fn atspi_bus_address() -> anyhow::Result<Option<String>> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let Some(display) = x11_display() else { return Ok(None) };
    let missing = || {
        anyhow::anyhow!(
            "display {display} has no AT-SPI bus: start at-spi-bus-launcher with DISPLAY={display}"
        )
    };
    let (conn, screen) = connect().map_err(|error| anyhow::anyhow!("connect to display {display}: {error}"))?;
    let root = conn.setup().roots[screen].root;
    let atom = conn.intern_atom(true, b"AT_SPI_BUS")?.reply()?.atom;
    if atom == x11rb::NONE {
        return Err(missing());
    }
    let reply = conn
        .get_property(false, root, atom, AtomEnum::STRING, 0, 4096)?
        .reply()?;
    let address = String::from_utf8(reply.value).map_err(|_| missing())?;
    if address.is_empty() {
        return Err(missing());
    }
    Ok(Some(address))
}
