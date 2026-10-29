//! Machine-join consent at the cmux-cua execution boundary.
//!
//! A machine joined to a cmux team (acpmux `host join`) gives the team no
//! screen and no control until the person at the machine grants them
//! locally. acpmux keeps that state under its home (`$ACPMUX_HOME`, else
//! `~/.acpmux`):
//!
//! - `machine-credential.json`: present once the machine has joined. This
//!   module only checks that the path exists; it never opens the file, so the
//!   credential inside never enters this process.
//! - `consent.json`: `{"screen": "granted"|"none", "control": ...}`, written
//!   only by acpmux's person-proven local consent RPC.
//! - `consent-refusal.json`: the last refusal, which the app's consent sheet
//!   reads. This module writes it when it refuses a call.
//!
//! The check runs on every tool call, not at startup, so a grant or a revoke
//! takes effect on the next call of a process that is already running.
//! It fails closed: a joined machine whose consent record is missing,
//! unreadable, malformed, not owned by this user or writable by others is
//! treated as having granted nothing, and a join status that cannot be read
//! counts as joined. A machine with no join record behaves exactly as before.
//!
//! The same answer is served by acpmux's `_acpmux/machine_consent_get`
//! (`{consentRequired, consent, lastRefusal}`); [`ConsentStatus`] mirrors that
//! shape. It is read from the files here so the check needs no running daemon
//! and an unjoined machine never depends on one.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::protocol::ToolResult;

/// The error code acpmux uses for the same refusal.
pub const CONSENT_REQUIRED: &str = "consent_required";

const CREDENTIAL_FILE: &str = "machine-credential.json";
const CONSENT_FILE: &str = "consent.json";
const REFUSAL_FILE: &str = "consent-refusal.json";
/// A consent record is a few dozen bytes; anything far larger is not one.
const MAX_CONSENT_BYTES: u64 = 64 * 1024;

/// A consent surface, as acpmux names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Reading the screen: pixels, window lists, accessibility text.
    Screen,
    /// Acting on the machine: input, launching and focusing apps, settings.
    Control,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Screen => "screen",
            Surface::Control => "control",
        }
    }
}

/// What a tool needs before it may run on a joined machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Reads no screen content and changes nothing the person sees.
    Nothing,
    Screen,
    Control,
    /// Both surfaces: tools that read and act (`page`), and any tool this
    /// table does not know, so a new tool is gated until it is classified.
    Both,
}

impl Requirement {
    pub fn surfaces(self) -> &'static [Surface] {
        match self {
            Requirement::Nothing => &[],
            Requirement::Screen => &[Surface::Screen],
            Requirement::Control => &[Surface::Control],
            Requirement::Both => &[Surface::Screen, Surface::Control],
        }
    }
}

/// The surfaces a tool needs, by its resolved name.
pub fn requirement_for(tool: &str) -> Requirement {
    match tool {
        // Metadata about the driver itself: no pixels, no accessibility text,
        // no effect on the person's apps or screen.
        "get_config"
        | "check_permissions"
        | "check_for_update"
        | "health_report"
        | "get_recording_state"
        | "get_agent_cursor_state"
        | "start_session"
        | "end_session"
        | "unsupported_platform" => Requirement::Nothing,

        // Reads the screen: captures, accessibility trees, window and app
        // lists, cursor position, display geometry, and recordings.
        "get_window_state"
        | "get_accessibility_tree"
        | "get_desktop_state"
        | "get_app_state"
        | "zoom"
        | "screenshot"
        | "get_screen_size"
        | "get_cursor_position"
        | "list_windows"
        | "list_apps"
        | "debug_window_info"
        | "start_recording"
        | "stop_recording" => Requirement::Screen,

        // Acts: pointer and keyboard input, app and window control, the
        // agent cursor drawn over the person's screen, driver settings,
        // installs, and replaying recorded actions.
        "click"
        | "double_click"
        | "right_click"
        | "drag"
        | "mouse_drag"
        | "parallel_mouse_drag"
        | "mouse_button_down"
        | "mouse_button_up"
        | "scroll"
        | "move_cursor"
        | "type_text"
        | "type_text_chars"
        | "press_key"
        | "hotkey"
        | "set_value"
        | "select_text"
        | "perform_secondary_action"
        | "launch_app"
        | "kill_app"
        | "bring_to_front"
        | "set_agent_cursor_enabled"
        | "set_agent_cursor_motion"
        | "set_agent_cursor_style"
        | "set_config"
        | "install_ffmpeg"
        | "replay_trajectory" => Requirement::Control,

        // `page` both reads page content and acts on it.
        "page" => Requirement::Both,

        _ => Requirement::Both,
    }
}

/// The consent state for one call, shaped like acpmux's
/// `_acpmux/machine_consent_get` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentStatus {
    /// True when the machine is joined, or its join status cannot be read.
    pub consent_required: bool,
    pub screen: bool,
    pub control: bool,
}

impl ConsentStatus {
    pub fn granted(&self, surface: Surface) -> bool {
        !self.consent_required
            || match surface {
                Surface::Screen => self.screen,
                Surface::Control => self.control,
            }
    }
}

/// acpmux's state directory for this process: `$ACPMUX_HOME`, else
/// `~/.acpmux`, the same rule acpmux uses without a tagged-build override.
pub fn acpmux_home() -> PathBuf {
    if let Some(home) = std::env::var_os("ACPMUX_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    let user_home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    user_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".acpmux")
}

/// Read the consent state under `home`. Never fails: every error is the
/// answer that refuses.
pub fn read_status(home: &Path) -> ConsentStatus {
    let consent_required = match std::fs::symlink_metadata(home.join(CREDENTIAL_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        // Present (whatever it is), or unreadable: treat as joined.
        _ => true,
    };
    if !consent_required {
        return ConsentStatus {
            consent_required,
            screen: false,
            control: false,
        };
    }
    let (screen, control) = read_consent(&home.join(CONSENT_FILE))
        .map(|consent| {
            (
                consent.screen == ConsentState::Granted,
                consent.control == ConsentState::Granted,
            )
        })
        .unwrap_or((false, false));
    ConsentStatus {
        consent_required,
        screen,
        control,
    }
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ConsentState {
    None,
    Granted,
}

#[derive(Deserialize)]
struct ConsentRecord {
    screen: ConsentState,
    control: ConsentState,
}

fn read_consent(path: &Path) -> Option<ConsentRecord> {
    use std::io::Read;

    let file = open_private_file(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_CONSENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CONSENT_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// Open a state file the way acpmux trusts one: a regular file, not a
/// symlink, owned by this user, and not writable by group or others (so no
/// one else can have granted consent by editing it).
#[cfg(unix)]
fn open_private_file(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    (metadata.is_file() && metadata.uid() == uid && metadata.mode() & 0o022 == 0).then_some(file)
}

#[cfg(not(unix))]
fn open_private_file(path: &Path) -> Option<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    std::fs::File::open(path).ok()
}

/// Check `tool` against the consent state under `home`. `Err` carries the
/// MCP tool error to return in place of running the tool.
pub fn check(home: &Path, tool: &str) -> Result<(), ToolResult> {
    let surfaces = requirement_for(tool).surfaces();
    if surfaces.is_empty() {
        return Ok(());
    }
    let status = read_status(home);
    let Some(&missing) = surfaces.iter().find(|surface| !status.granted(**surface)) else {
        return Ok(());
    };
    record_refusal(home, missing);
    Err(refusal(tool, missing))
}

/// The MCP tool error for a refused call. It names the tool and the surface
/// and nothing else: no path, no credential.
pub fn refusal(tool: &str, surface: Surface) -> ToolResult {
    let surface = surface.as_str();
    ToolResult::error(format!(
        "{CONSENT_REQUIRED}: this machine is joined to a cmux team, and the person at it \
         has not allowed {surface} access. `{tool}` did not run. Ask them to allow \
         {surface} access on this machine."
    ))
    .with_structured(json!({
        "code": CONSENT_REQUIRED,
        "surface": surface,
        "tool": tool,
    }))
}

/// Record the refusal for the app's consent sheet, in acpmux's format.
/// Best effort: a failure here never changes the refusal. Writes only into
/// an existing state directory and never follows a link at the final path.
fn record_refusal(home: &Path, surface: Surface) {
    let Ok(metadata) = std::fs::symlink_metadata(home) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let Ok(body) = serde_json::to_vec(&json!({
        "event": "consent_refused",
        "code": CONSENT_REQUIRED,
        "surface": surface.as_str(),
        "at": at,
    })) else {
        return;
    };
    let _ = write_atomic(home, REFUSAL_FILE, &body);
}

fn write_atomic(dir: &Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{name}.cmux-cua.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options.open(&tmp).and_then(|mut file| {
        file.write_all(body)?;
        file.sync_all()
    });
    let result = result.and_then(|()| std::fs::rename(&tmp, dir.join(name)));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_private(
            &dir.path().join(CREDENTIAL_FILE),
            br#"{"credential":"cmm_test"}"#,
        );
        dir
    }

    fn write_private(path: &Path, body: &[u8]) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn grant(home: &Path, screen: &str, control: &str) {
        write_private(
            &home.join(CONSENT_FILE),
            format!(r#"{{"screen":"{screen}","control":"{control}"}}"#).as_bytes(),
        );
    }

    fn refused_surface(result: Result<(), ToolResult>) -> Option<String> {
        result.err().map(|error| {
            let structured = error.structured_content.unwrap();
            assert_eq!(structured["code"], CONSENT_REQUIRED);
            structured["surface"].as_str().unwrap().to_owned()
        })
    }

    #[test]
    fn a_machine_that_is_not_joined_needs_no_consent_and_gets_no_files() {
        let home = tempfile::tempdir().unwrap();
        for tool in ["get_window_state", "click", "page", "some_future_tool"] {
            assert!(check(home.path(), tool).is_ok(), "{tool}");
        }
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
        let missing = home.path().join("does-not-exist");
        assert!(check(&missing, "click").is_ok());
        assert!(!missing.exists());
    }

    #[test]
    fn a_joined_machine_without_consent_refuses_screen_and_control() {
        let home = joined_home();
        assert_eq!(
            refused_surface(check(home.path(), "get_window_state")).as_deref(),
            Some("screen")
        );
        assert_eq!(
            refused_surface(check(home.path(), "click")).as_deref(),
            Some("control")
        );
        assert_eq!(
            refused_surface(check(home.path(), "page")).as_deref(),
            Some("screen")
        );
        assert!(check(home.path(), "get_config").is_ok());

        let refusal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.path().join(REFUSAL_FILE)).unwrap())
                .unwrap();
        assert_eq!(refusal["event"], "consent_refused");
        assert_eq!(refusal["code"], CONSENT_REQUIRED);
        assert_eq!(refusal["surface"], "screen");
        assert!(refusal["at"].as_u64().unwrap() > 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.path().join(REFUSAL_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the refusal record is owner-only");
        }
    }

    #[test]
    fn screen_only_allows_reads_and_refuses_actions() {
        let home = joined_home();
        grant(home.path(), "granted", "none");
        assert!(check(home.path(), "get_window_state").is_ok());
        assert!(check(home.path(), "zoom").is_ok());
        assert_eq!(
            refused_surface(check(home.path(), "click")).as_deref(),
            Some("control")
        );
        assert_eq!(
            refused_surface(check(home.path(), "type_text")).as_deref(),
            Some("control")
        );
        assert_eq!(
            refused_surface(check(home.path(), "page")).as_deref(),
            Some("control")
        );
    }

    #[test]
    fn a_revoke_takes_effect_on_the_next_call() {
        let home = joined_home();
        grant(home.path(), "granted", "granted");
        assert!(check(home.path(), "click").is_ok());
        assert!(check(home.path(), "page").is_ok());
        grant(home.path(), "granted", "none");
        assert_eq!(
            refused_surface(check(home.path(), "click")).as_deref(),
            Some("control")
        );
    }

    #[test]
    fn a_bad_consent_record_grants_nothing() {
        let home = joined_home();
        for body in [
            &b"not json"[..],
            br#"{"screen":"granted"}"#,
            br#"{"screen":"GRANTED","control":"granted"}"#,
            br#"{"screen":true,"control":true}"#,
        ] {
            write_private(&home.path().join(CONSENT_FILE), body);
            let shown = String::from_utf8_lossy(body);
            assert!(check(home.path(), "get_window_state").is_err(), "{shown}");
            assert!(check(home.path(), "click").is_err(), "{shown}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_consent_record_others_could_write_or_a_link_grants_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let home = joined_home();
        grant(home.path(), "granted", "granted");
        let path = home.path().join(CONSENT_FILE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(check(home.path(), "click").is_err());

        let real = home.path().join("elsewhere.json");
        std::fs::rename(&path, &real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert!(check(home.path(), "click").is_err());
    }

    #[test]
    fn an_unreadable_join_status_counts_as_joined() {
        // A credential path that is a directory or a dangling link is not a
        // clean "not joined": fail closed.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(CREDENTIAL_FILE)).unwrap();
        assert!(read_status(home.path()).consent_required);
        assert!(check(home.path(), "click").is_err());

        #[cfg(unix)]
        {
            let home = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(home.path().join("gone"), home.path().join(CREDENTIAL_FILE))
                .unwrap();
            assert!(read_status(home.path()).consent_required);
        }
    }

    #[test]
    fn refusal_text_never_carries_paths_or_credentials() {
        let home = joined_home();
        let error = check(home.path(), "click").unwrap_err();
        let text = serde_json::to_string(&error).unwrap();
        assert!(!text.contains("cmm_"));
        assert!(!text.contains(&home.path().display().to_string()));
    }

    #[test]
    fn every_known_tool_is_classified() {
        // Every tool name that ships today is in an explicit arm, so only a
        // new, unclassified tool falls through to `Both`.
        const SHIPPED: &[&str] = &[
            "bring_to_front",
            "check_for_update",
            "check_permissions",
            "click",
            "debug_window_info",
            "double_click",
            "drag",
            "end_session",
            "get_accessibility_tree",
            "get_agent_cursor_state",
            "get_app_state",
            "get_config",
            "get_cursor_position",
            "get_desktop_state",
            "get_recording_state",
            "get_screen_size",
            "get_window_state",
            "health_report",
            "hotkey",
            "install_ffmpeg",
            "kill_app",
            "launch_app",
            "list_apps",
            "list_windows",
            "mouse_button_down",
            "mouse_button_up",
            "mouse_drag",
            "move_cursor",
            "parallel_mouse_drag",
            "perform_secondary_action",
            "press_key",
            "replay_trajectory",
            "right_click",
            "screenshot",
            "scroll",
            "select_text",
            "set_agent_cursor_enabled",
            "set_agent_cursor_motion",
            "set_agent_cursor_style",
            "set_config",
            "set_value",
            "start_recording",
            "start_session",
            "stop_recording",
            "type_text",
            "type_text_chars",
            "unsupported_platform",
            "zoom",
        ];
        for tool in SHIPPED {
            assert_ne!(
                requirement_for(tool),
                Requirement::Both,
                "{tool} is unclassified"
            );
        }
        assert_eq!(requirement_for("page"), Requirement::Both);
        assert_eq!(requirement_for("a_tool_added_later"), Requirement::Both);
    }
}
