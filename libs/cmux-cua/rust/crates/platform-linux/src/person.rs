//! Waiting for a person who acts on the same X display through cmux remote desktop.
//!
//! On a cmux Cloud machine the person can take over the agent's display (`cmux-rd host`).
//! Their input goes through an XI2 master of its own (`cmux person`), so the agent keeps
//! the core pointer and both can act at once. While that master exists, the root window
//! carries [`PERSON_PROPERTY`] (STRING, JSON: `pid`, `window`, `button_held`,
//! `last_input_unix_ms`); the host removes both 3 s after the person's last input.
//!
//! Two waits follow from it, each bounded:
//! - Keys into Chromium wait while the property exists. Chromium takes key events from one
//!   master keyboard only (ui/events/devices/x11/touch_factory_x11.cc keeps the last one
//!   listed), so the agent's XTest keys are dropped while the person's master exists.
//! - A press (click, drag) waits while the person is using the window under it: they
//!   hold a button there, or their last input there was under [`ACTIVE_FOR`] ago. Chrome
//!   drops both clicks when two masters press at once.
//!
//! The waits watch the property with PropertyNotify, not a polling loop.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Window,
};
use x11rb::rust_connection::RustConnection;

/// Root window property `cmux-rd host` keeps while the person's master exists.
pub const PERSON_PROPERTY: &str = "_CMUX_RD_PERSON";
/// Keys into Chromium wait at most this long; the host lets go 3 s after the person's
/// last input.
pub const KEYS_WAIT: Duration = Duration::from_secs(5);
/// A press waits at most this long for the person to leave its window.
pub const PRESS_WAIT: Duration = Duration::from_secs(3);
/// The person counts as using a window for this long after their last input there.
pub const ACTIVE_FOR: Duration = Duration::from_millis(1000);

/// The parsed [`PERSON_PROPERTY`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub pid: u32,
    /// The toplevel (root child) under the person's pointer, 0 when none.
    pub window: u32,
    pub button_held: bool,
    pub last_input_unix_ms: u64,
}

impl Person {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        Some(Self {
            pid: v.get("pid")?.as_u64()? as u32,
            window: v.get("window").and_then(|w| w.as_u64()).unwrap_or(0) as u32,
            button_held: v
                .get("button_held")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
            last_input_unix_ms: v
                .get("last_input_unix_ms")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
        })
    }

    /// Whether the person is using `window` at `now_unix_ms`.
    pub fn uses(&self, window: u32, now_unix_ms: u64) -> bool {
        window != 0
            && self.window == window
            && (self.button_held
                || now_unix_ms.saturating_sub(self.last_input_unix_ms)
                    < ACTIVE_FOR.as_millis() as u64)
    }
}

/// Why an input call did not go ahead.
#[derive(Debug, PartialEq, Eq)]
pub enum Busy {
    /// The person still acted when the wait ran out.
    StillActing(Duration),
    /// The property names a host that no longer runs: its master (and the lost keys)
    /// stays until the next remote desktop session or a display restart.
    Stale(u32),
}

impl Busy {
    pub fn keys_message(&self) -> String {
        match self {
            Busy::StillActing(waited) => format!(
                "A person is using this desktop through cmux remote desktop (Take over). \
                 Chromium takes keys from one keyboard at a time, so keys sent now would be \
                 lost. Waited {} s for them to stop; try again shortly.",
                waited.as_secs()
            ),
            Busy::Stale(pid) => format!(
                "A cmux remote desktop host (pid {pid}) ended without removing the person's \
                 keyboard, so Chromium drops keys sent now. A new Take over or Hand back in \
                 the remote desktop clears it."
            ),
        }
    }

    pub fn press_message(&self) -> String {
        match self {
            Busy::StillActing(waited) => format!(
                "A person is using this window through cmux remote desktop (Take over). \
                 Waited {} s for them to stop; try again shortly, or act on another window.",
                waited.as_secs()
            ),
            Busy::Stale(pid) => format!(
                "A cmux remote desktop host (pid {pid}) ended while the person held a \
                 button in this window."
            ),
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Watches [`PERSON_PROPERTY`] on the root window of one connection.
struct Watch<'a> {
    conn: &'a RustConnection,
    root: Window,
    atom: u32,
}

impl<'a> Watch<'a> {
    fn new(conn: &'a RustConnection, root: Window) -> anyhow::Result<Self> {
        let atom = conn
            .intern_atom(false, PERSON_PROPERTY.as_bytes())?
            .reply()?
            .atom;
        Ok(Self { conn, root, atom })
    }

    fn read(&self) -> anyhow::Result<Option<Person>> {
        let r = self
            .conn
            .get_property(false, self.root, self.atom, AtomEnum::STRING, 0, 1024)?
            .reply()?;
        Ok((!r.value.is_empty())
            .then(|| Person::parse(&r.value))
            .flatten())
    }

    /// Waits while `busy(person, now_unix_ms)` holds, at most `limit`. Wakes on each
    /// change of the property, and when a recent input ages past [`ACTIVE_FOR`].
    fn wait(&self, limit: Duration, busy: impl Fn(&Person, u64) -> bool) -> Result<(), Busy> {
        let start = Instant::now();
        let mut selected = false;
        loop {
            let Ok(Some(person)) = self.read() else {
                return Ok(());
            };
            if !busy(&person, now_unix_ms()) {
                return Ok(());
            }
            if !pid_alive(person.pid) {
                return Err(Busy::Stale(person.pid));
            }
            let waited = start.elapsed();
            if waited >= limit {
                return Err(Busy::StillActing(waited));
            }
            if !selected {
                // PropertyNotify on the root, on this short-lived connection only.
                let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
                if self.conn.change_window_attributes(self.root, &aux).is_err() {
                    return Ok(());
                }
                selected = true;
                // Read again: the property may have changed before the mask applied.
                continue;
            }
            // Recent input ages out with no PropertyNotify, so wake for it too.
            let age = now_unix_ms().saturating_sub(person.last_input_unix_ms);
            let ages_out =
                Duration::from_millis((ACTIVE_FOR.as_millis() as u64).saturating_sub(age));
            let mut next = limit - waited;
            if !person.button_held && !ages_out.is_zero() {
                next = next.min(ages_out + Duration::from_millis(5));
            }
            if self.wait_event(next).is_err() {
                return Ok(());
            }
        }
    }

    /// Blocks until an event arrives or `timeout` passes, then drops the events read.
    fn wait_event(&self, timeout: Duration) -> anyhow::Result<()> {
        self.conn.flush()?;
        if self.conn.poll_for_event()?.is_none() {
            let mut fd = libc::pollfd {
                fd: self.conn.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd for the duration of the call.
            unsafe { libc::poll(&mut fd, 1, ms) };
        }
        while self.conn.poll_for_event()?.is_some() {}
        Ok(())
    }
}

/// Before XTest keys go to a Chromium window: waits (at most [`KEYS_WAIT`]) while the
/// person's master exists. Ok when there is no marker or the display cannot be read.
pub fn wait_for_keys() -> Result<(), Busy> {
    let Ok((conn, screen)) = crate::display::connect() else {
        return Ok(());
    };
    let root = conn.setup().roots[screen].root;
    let Ok(watch) = Watch::new(&conn, root) else {
        return Ok(());
    };
    watch.wait(KEYS_WAIT, |_, _| true)
}

/// Before an XTest press at screen (`x`, `y`) on `conn`: waits (at most [`PRESS_WAIT`])
/// while the person uses the toplevel under that point.
pub fn wait_for_press(conn: &RustConnection, root: Window, x: i32, y: i32) -> Result<(), Busy> {
    let Ok(watch) = Watch::new(conn, root) else {
        return Ok(());
    };
    // Cheap first read: no marker, no lookup.
    if !matches!(watch.read(), Ok(Some(_))) {
        return Ok(());
    }
    let window = conn
        .translate_coordinates(
            root,
            root,
            x.clamp(0, i16::MAX as i32) as i16,
            y.clamp(0, i16::MAX as i32) as i16,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .map_or(0, |r| r.child);
    watch.wait(PRESS_WAIT, |p, now| p.uses(window, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_host_property() {
        let p = Person::parse(
            br#"{"v":1,"pid":42,"pointer":11,"keyboard":12,"window":4194305,"button_held":true,"last_input_unix_ms":1700000000000}"#,
        )
        .unwrap();
        assert_eq!(
            p,
            Person {
                pid: 42,
                window: 4194305,
                button_held: true,
                last_input_unix_ms: 1_700_000_000_000
            }
        );
        assert_eq!(Person::parse(b"not json"), None);
        assert_eq!(Person::parse(br#"{"v":1}"#), None, "a pid is required");
    }

    #[test]
    fn uses_only_the_same_window_while_active() {
        let p = Person {
            pid: 1,
            window: 7,
            button_held: false,
            last_input_unix_ms: 10_000,
        };
        assert!(p.uses(7, 10_500));
        assert!(!p.uses(7, 11_000), "idle for ACTIVE_FOR");
        assert!(!p.uses(8, 10_500), "another window");
        assert!(!p.uses(0, 10_500), "no window under the press");
        let held = Person {
            button_held: true,
            ..p
        };
        assert!(held.uses(7, 60_000), "a held button keeps the window busy");
    }
}
