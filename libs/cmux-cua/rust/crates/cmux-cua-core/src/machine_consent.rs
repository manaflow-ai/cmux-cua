//! Machine-join consent at the cmux-cua execution boundary.
//!
//! A machine joined to a cmux team (acpmux `host join`) gives the team no
//! screen and no control until the person at the machine grants them
//! locally. acpmux keeps that state under its home (`$ACPMUX_HOME`, else
//! `~/.acpmux`):
//!
//! - `machine-credential.json`: present once the machine has joined. This
//!   module checks that it is a private file with acpmux's record shape; the
//!   credential value inside is type-checked and dropped, never kept, logged
//!   or returned.
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
//! Every gate and every capture site asks one [`ConsentSource`], right before
//! it acts. Today's source, [`StateFiles`], reads the files above. The same
//! answer is served by acpmux's `_acpmux/machine_consent_get`
//! (`{consentRequired, consent, lastRefusal}`), which [`ConsentStatus`]
//! mirrors; moving to that daemon query changes only [`default_source`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use crate::protocol::ToolResult;

/// The error code acpmux uses for the same refusal.
pub const CONSENT_REQUIRED: &str = "consent_required";

const CREDENTIAL_FILE: &str = "machine-credential.json";
const CONSENT_FILE: &str = "consent.json";
const REFUSAL_FILE: &str = "consent-refusal.json";
/// acpmux's records are a few hundred bytes; anything far larger is not one.
const MAX_RECORD_BYTES: u64 = 64 * 1024;

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
    /// Observation only.
    Screen,
    /// Acts on the machine: needs `screen` and `control`, the rule acpmux's
    /// Computer Use bridge enforces, since acting means seeing what is acted on.
    Act,
    /// Both surfaces: tools that read and act (`page`), and any tool this
    /// table does not know, so a new tool is gated until it is classified.
    Both,
}

impl Requirement {
    pub fn surfaces(self) -> &'static [Surface] {
        match self {
            Requirement::Nothing => &[],
            Requirement::Screen => &[Surface::Screen],
            Requirement::Act => &[Surface::Screen, Surface::Control],
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
        | "replay_trajectory" => Requirement::Act,

        // `page` both reads page content and acts on it.
        "page" => Requirement::Both,

        _ => Requirement::Both,
    }
}

/// The consent state for one call, shaped like acpmux's
/// `_acpmux/machine_consent_get` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsentStatus {
    /// True when the machine is joined, or its join status cannot be read.
    pub consent_required: bool,
    /// Grants that count: false whenever the consent record, or the join
    /// record it belongs to, cannot be trusted.
    pub screen: bool,
    pub control: bool,
}

impl ConsentStatus {
    /// The answer when nothing can be read: joined, nothing granted.
    pub const UNKNOWN: ConsentStatus = ConsentStatus {
        consent_required: true,
        screen: false,
        control: false,
    };

    pub fn granted(&self, surface: Surface) -> bool {
        !self.consent_required
            || match surface {
                Surface::Screen => self.screen,
                Surface::Control => self.control,
            }
    }
}

/// Where cmux-cua learns the consent state. Callers ask [`status`] right
/// before each gate and each capture, and never reuse an answer read before
/// an await.
///
/// [`status`]: ConsentSource::status
pub trait ConsentSource: Send + Sync {
    /// The state as of now. Fails closed: any error is
    /// [`ConsentStatus::UNKNOWN`], never "not joined".
    fn status(&self) -> ConsentStatus;
    /// Record a refused call for the app's consent sheet. Best effort.
    fn record_refusal(&self, surface: Surface);
}

/// The consent source this process uses: the one place to change when the
/// answer moves to the cmux app's authenticated connection.
///
/// The build that ships inside cmux.app (feature `cmux-host-consent`) holds
/// the person's macOS grants, so it trusts no file a caller could redirect:
/// it asks the verified cmux host (`host_consent`), and with no host
/// connected the answer is "joined, nothing granted".
/// Standalone and released builds keep acpmux's state files, which are
/// advisory: `$ACPMUX_HOME` is chosen by whoever starts the process.
pub fn default_source() -> Arc<dyn ConsentSource> {
    #[cfg(all(feature = "cmux-host-consent", unix))]
    {
        crate::host_consent::shared()
    }
    #[cfg(all(feature = "cmux-host-consent", not(unix)))]
    {
        Arc::new(Unavailable)
    }
    #[cfg(not(feature = "cmux-host-consent"))]
    {
        Arc::new(StateFiles::FromEnv)
    }
}

/// No trusted answer: joined, nothing granted, so everything that needs
/// consent refuses.
#[derive(Debug, Clone, Copy)]
pub struct Unavailable;

impl ConsentSource for Unavailable {
    fn status(&self) -> ConsentStatus {
        ConsentStatus::UNKNOWN
    }

    fn record_refusal(&self, _surface: Surface) {}
}

/// Consent read from acpmux's state files.
#[derive(Debug, Clone)]
pub enum StateFiles {
    /// The state directory resolved on every read: `$ACPMUX_HOME`, else
    /// `~/.acpmux`.
    FromEnv,
    /// A fixed state directory; `None` means none could be found.
    At(Option<PathBuf>),
}

impl StateFiles {
    fn home(&self) -> Option<PathBuf> {
        match self {
            Self::FromEnv => acpmux_home(),
            Self::At(home) => home.clone(),
        }
    }
}

impl ConsentSource for StateFiles {
    fn status(&self) -> ConsentStatus {
        read_status(self.home().as_deref())
    }

    fn record_refusal(&self, surface: Surface) {
        if let Some(home) = self.home() {
            record_refusal(&home, surface);
        }
    }
}

/// acpmux's state directory for this process: `$ACPMUX_HOME`, else
/// `<home>/.acpmux`, the same rule acpmux uses without a tagged-build
/// override. `None` when no home directory can be found; callers treat that
/// as an unreadable join status and refuse.
fn acpmux_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("ACPMUX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    user_home().map(|home| home.join(".acpmux"))
}

#[cfg(unix)]
fn user_home() -> Option<PathBuf> {
    user_home_from(std::env::var_os("HOME"))
}

#[cfg(unix)]
fn user_home_from(env_home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(home) = env_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    // No $HOME: the account's home from the password database, as acpmux's
    // `dirs::home_dir` does.
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer refers to live, correctly sized storage owned by
    // this frame; getpwuid_r writes only within `buffer` and `entry`.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    // SAFETY: pw_dir is a NUL-terminated string inside `buffer`.
    let dir = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes();
    (!dir.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(dir)))
}

#[cfg(not(unix))]
fn user_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Read the consent state under `home` (`None`: no home could be found).
/// Never fails: every error is the answer that refuses.
fn read_status(home: Option<&Path>) -> ConsentStatus {
    let Some(home) = home else {
        return ConsentStatus::UNKNOWN;
    };
    let credential_path = home.join(CREDENTIAL_FILE);
    match std::fs::symlink_metadata(&credential_path) {
        // Absent, and not because a link on the way to it dangles.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && cleanly_absent(&credential_path) =>
        {
            return ConsentStatus {
                consent_required: false,
                screen: false,
                control: false,
            };
        }
        // Present in some form, or unreadable: joined.
        _ => {}
    }
    // A join record acpmux itself would refuse (a link, another owner,
    // writable by others, malformed) grants nothing, whatever consent says.
    if !credential_record_is_valid(&credential_path) {
        return ConsentStatus::UNKNOWN;
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
        consent_required: true,
        screen,
        control,
    }
}

/// Whether `path` is missing because its nearest existing ancestor is a real
/// directory (directly or through a link that resolves) that lacks the rest.
/// A dangling link on the way (`$ACPMUX_HOME` or `~/.acpmux` pointing at
/// nothing) is not "never joined": it may be a home that is unmounted or
/// moved, so the answer is unknown.
fn cleanly_absent(path: &Path) -> bool {
    for ancestor in path.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            return false;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => return std::fs::metadata(ancestor).is_ok_and(|m| m.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        }
    }
    false
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

fn read_private_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let file = open_private_file(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_RECORD_BYTES).then_some(bytes)
}

fn read_consent(path: &Path) -> Option<ConsentRecord> {
    serde_json::from_slice(&read_private_bounded(path)?).ok()
}

/// The join record has acpmux's `MachineCredential` shape. The credential
/// value is type-checked and dropped; it is never kept, logged or returned.
fn credential_record_is_valid(path: &Path) -> bool {
    let Some(bytes) = read_private_bounded(path) else {
        return false;
    };
    let Ok(serde_json::Value::Object(record)) = serde_json::from_slice(&bytes) else {
        return false;
    };
    let string = |key: &str| record.get(key).is_some_and(serde_json::Value::is_string);
    [
        "credential",
        "machine_id",
        "team_id",
        "name",
        "hub_origin",
        "wgPublicKey",
    ]
    .iter()
    .all(|key| string(key))
        && record
            .get("revoked")
            .is_some_and(serde_json::Value::is_boolean)
        && record
            .get("last_heartbeat")
            .map_or(true, |v| v.is_null() || v.is_string())
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

/// Windows: open without following a reparse point, then trust the file only
/// if its owner is this user and no trustee other than this user, SYSTEM or
/// Administrators may change it or its protection (checked on the opened
/// handle). Any failure of the check itself refuses.
#[cfg(windows)]
fn open_private_file(path: &Path) -> Option<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const GENERIC_READ: u32 = 0x8000_0000;
    const READ_CONTROL: u32 = 0x0002_0000;

    let file = std::fs::OpenOptions::new()
        .access_mode(GENERIC_READ | READ_CONTROL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    let attributes = metadata.file_attributes();
    if !metadata.is_file()
        || attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
    {
        return None;
    }
    windows_acl::handle_is_private(&file).then_some(file)
}

/// Neither Unix nor Windows: nothing is verified, so no record is trusted.
#[cfg(not(any(unix, windows)))]
fn open_private_file(_path: &Path) -> Option<std::fs::File> {
    None
}

/// Who an access-control entry names, as far as this check cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Trustee {
    CurrentUser,
    System,
    Administrators,
    Other,
}

/// One entry of a file's DACL.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(windows), allow(dead_code))]
enum AccessEntry {
    Allow {
        trustee: Trustee,
        mask: u32,
    },
    Deny,
    /// Any other entry type: not understood, so not trusted.
    Unknown,
}

/// Rights that let a trustee change a file's contents or its protection:
/// FILE_WRITE_DATA, FILE_APPEND_DATA, FILE_WRITE_EA, FILE_WRITE_ATTRIBUTES,
/// DELETE, WRITE_DAC, WRITE_OWNER, MAXIMUM_ALLOWED, GENERIC_ALL, GENERIC_WRITE.
#[cfg_attr(not(windows), allow(dead_code))]
const WRITE_LIKE_RIGHTS: u32 = 0x0000_0002
    | 0x0000_0004
    | 0x0000_0010
    | 0x0000_0100
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000
    | 0x0200_0000
    | 0x1000_0000
    | 0x4000_0000;

/// The Windows rule, acpmux's (`owner_only.rs`): owned by this user (or by
/// SYSTEM or Administrators, who can rewrite any file anyway; an elevated
/// administrator's new files are owned by Administrators), a DACL present (a
/// null DACL lets everyone in), and no allow entry for anyone else that
/// carries a write-like right.
#[cfg_attr(not(windows), allow(dead_code))]
fn dacl_is_private(owner: Trustee, dacl: Option<&[AccessEntry]>) -> bool {
    owner != Trustee::Other
        && dacl.is_some_and(|entries| {
            entries.iter().all(|entry| match entry {
                AccessEntry::Allow {
                    trustee: Trustee::Other,
                    mask,
                } => mask & WRITE_LIKE_RIGHTS == 0,
                AccessEntry::Allow { .. } | AccessEntry::Deny => true,
                AccessEntry::Unknown => false,
            })
        })
}

#[cfg(windows)]
mod windows_acl {
    use super::{dacl_is_private, AccessEntry, Trustee};
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetTokenInformation,
        IsWellKnownSid, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 0x1;
    const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 0x9;
    const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 0xA;
    const INHERIT_ONLY_ACE: u8 = 0x08;

    /// This process's user SID, kept alive in its own buffer.
    struct CurrentUser {
        buffer: Vec<u64>,
    }

    impl CurrentUser {
        fn get() -> Option<Self> {
            unsafe {
                let mut token = HANDLE::default();
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
                let mut needed = 0u32;
                let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
                let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
                let filled = GetTokenInformation(
                    token,
                    TokenUser,
                    Some(buffer.as_mut_ptr().cast()),
                    (buffer.len() * 8) as u32,
                    &mut needed,
                );
                let _ = CloseHandle(token);
                filled.ok()?;
                Some(CurrentUser { buffer })
            }
        }

        fn sid(&self) -> PSID {
            // SAFETY: `buffer` holds a TOKEN_USER filled by GetTokenInformation;
            // its SID points into the same buffer, which outlives this call.
            unsafe { (*(self.buffer.as_ptr() as *const TOKEN_USER)).User.Sid }
        }
    }

    fn classify(sid: PSID, user: &CurrentUser) -> Trustee {
        unsafe {
            if sid.is_invalid() {
                Trustee::Other
            } else if EqualSid(sid, user.sid()).is_ok() {
                Trustee::CurrentUser
            } else if IsWellKnownSid(sid, WinLocalSystemSid).0 != 0 {
                Trustee::System
            } else if IsWellKnownSid(sid, WinBuiltinAdministratorsSid).0 != 0 {
                Trustee::Administrators
            } else {
                Trustee::Other
            }
        }
    }

    pub(super) fn handle_is_private(file: &std::fs::File) -> bool {
        let Some(user) = CurrentUser::get() else {
            return false;
        };
        let mut owner = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the handle is open for READ_CONTROL for the whole call; the
        // out pointers refer to locals; the descriptor is freed below.
        let status = unsafe {
            GetSecurityInfo(
                HANDLE(file.as_raw_handle() as _),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut dacl),
                None,
                Some(&mut descriptor),
            )
        };
        if status.0 != 0 {
            return false;
        }
        let verdict = (|| -> Option<bool> {
            let owner = classify(owner, &user);
            if dacl.is_null() {
                return Some(dacl_is_private(owner, None));
            }
            let mut size = ACL_SIZE_INFORMATION::default();
            // SAFETY: `dacl` points into `descriptor`, alive until LocalFree.
            unsafe {
                GetAclInformation(
                    dacl,
                    (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
                .ok()?;
            }
            let mut entries = Vec::with_capacity(size.AceCount as usize);
            for index in 0..size.AceCount {
                let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                // SAFETY: index < AceCount; GetAce returns a pointer into the ACL.
                unsafe { GetAce(dacl, index, &mut ace).ok()? };
                let header = unsafe { &*(ace as *const ACE_HEADER) };
                entries.push(match header.AceType {
                    // Applies only to children; grants nothing on this file.
                    _ if header.AceFlags & INHERIT_ONLY_ACE != 0 => AccessEntry::Deny,
                    // Plain and callback allow entries: header, mask, SID.
                    ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {
                        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
                        let sid = PSID(std::ptr::addr_of!(allowed.SidStart) as *mut _);
                        AccessEntry::Allow {
                            trustee: classify(sid, &user),
                            mask: allowed.Mask,
                        }
                    }
                    ACCESS_DENIED_ACE_TYPE | ACCESS_DENIED_CALLBACK_ACE_TYPE => AccessEntry::Deny,
                    _ => AccessEntry::Unknown,
                });
            }
            Some(dacl_is_private(owner, Some(&entries)))
        })()
        .unwrap_or(false);
        // SAFETY: GetSecurityInfo allocated the descriptor with LocalAlloc.
        unsafe {
            let _ = LocalFree(HLOCAL(descriptor.0));
        }
        verdict
    }
}

/// The first surface `tool` needs that `status` does not grant. `also_screen`
/// adds the screen surface for a call that will capture the screen as a side
/// effect (an active recording or the picture-in-picture preview).
pub fn missing_surface(status: &ConsentStatus, tool: &str, also_screen: bool) -> Option<Surface> {
    let extra: &[Surface] = if also_screen { &[Surface::Screen] } else { &[] };
    requirement_for(tool)
        .surfaces()
        .iter()
        .chain(extra)
        .copied()
        .find(|surface| !status.granted(*surface))
}

/// Check `tool` against `source`. `Err` carries the MCP tool error to
/// return in place of running the tool.
pub fn check(source: &dyn ConsentSource, tool: &str) -> Result<(), ToolResult> {
    if requirement_for(tool) == Requirement::Nothing {
        return Ok(());
    }
    match missing_surface(&source.status(), tool, false) {
        None => Ok(()),
        Some(surface) => Err(refuse(source, tool, surface)),
    }
}

/// Record a refusal with `source` and return the MCP tool error for it.
pub fn refuse(source: &dyn ConsentSource, tool: &str, surface: Surface) -> ToolResult {
    source.record_refusal(surface);
    refusal(tool, surface)
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

/// A valid join record in acpmux's `MachineCredential` shape, for tests here
/// and in other crates. The credential value is a fixture, not a secret.
#[doc(hidden)]
pub const TEST_CREDENTIAL_RECORD: &str = r#"{"credential":"cmm_test","machine_id":"m_test","team_id":"t_test","name":"test","hub_origin":"https://hub.invalid","last_heartbeat":null,"revoked":false,"wgPublicKey":"k"}"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_private(path: &Path, body: &[u8]) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn joined_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_private(
            &dir.path().join(CREDENTIAL_FILE),
            TEST_CREDENTIAL_RECORD.as_bytes(),
        );
        dir
    }

    fn grant(home: &Path, screen: &str, control: &str) {
        write_private(
            &home.join(CONSENT_FILE),
            format!(r#"{{"screen":"{screen}","control":"{control}","grantedAt":"1"}}"#).as_bytes(),
        );
    }

    fn refused_surface(result: Result<(), ToolResult>) -> Option<String> {
        result.err().map(|error| {
            let structured = error.structured_content.unwrap();
            assert_eq!(structured["code"], CONSENT_REQUIRED);
            structured["surface"].as_str().unwrap().to_owned()
        })
    }

    fn check_in(home: &Path, tool: &str) -> Result<(), ToolResult> {
        check(&StateFiles::At(Some(home.to_owned())), tool)
    }

    const NO_HOME: StateFiles = StateFiles::At(None);

    #[test]
    fn a_machine_that_is_not_joined_needs_no_consent_and_gets_no_files() {
        let home = tempfile::tempdir().unwrap();
        for tool in ["get_window_state", "click", "page", "some_future_tool"] {
            assert!(check_in(home.path(), tool).is_ok(), "{tool}");
        }
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
        let missing = home.path().join("does-not-exist");
        assert!(check_in(&missing, "click").is_ok());
        assert!(!missing.exists());
    }

    #[test]
    fn no_home_at_all_refuses_but_metadata_still_runs() {
        assert_eq!(read_status(None), ConsentStatus::UNKNOWN);
        assert_eq!(
            refused_surface(check(&NO_HOME, "click")).as_deref(),
            Some("screen")
        );
        assert_eq!(
            refused_surface(check(&NO_HOME, "zoom")).as_deref(),
            Some("screen")
        );
        assert!(check(&NO_HOME, "get_config").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_home_falls_back_to_the_password_database_without_home() {
        assert_eq!(
            user_home_from(Some("/explicit/home".into())),
            Some(PathBuf::from("/explicit/home"))
        );
        // Unset and empty $HOME both read the password database, which on
        // any test runner names this account's home directory.
        for env_home in [None, Some(std::ffi::OsString::new())] {
            let home = user_home_from(env_home).expect("password database home");
            assert!(home.is_absolute(), "{}", home.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_state_directory_link_is_not_unjoined() {
        let root = tempfile::tempdir().unwrap();
        // $ACPMUX_HOME itself is a dangling link.
        let home = root.path().join("acpmux");
        std::os::unix::fs::symlink(root.path().join("gone"), &home).unwrap();
        assert_eq!(read_status(Some(&home)), ConsentStatus::UNKNOWN);
        assert!(check_in(&home, "click").is_err());
        // ~/.acpmux under a home whose link dangles.
        let user = root.path().join("user");
        std::os::unix::fs::symlink(root.path().join("unmounted"), &user).unwrap();
        assert_eq!(
            read_status(Some(&user.join(".acpmux"))),
            ConsentStatus::UNKNOWN
        );
        // A missing ~/.acpmux in a real home, or a link that resolves, is
        // simply not joined.
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        assert!(!read_status(Some(&real.join(".acpmux"))).consent_required);
        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        assert!(!read_status(Some(&linked)).consent_required);
    }

    #[test]
    fn a_joined_machine_without_consent_refuses_screen_and_control() {
        let home = joined_home();
        assert_eq!(
            refused_surface(check_in(home.path(), "get_window_state")).as_deref(),
            Some("screen")
        );
        assert_eq!(
            refused_surface(check_in(home.path(), "click")).as_deref(),
            Some("screen")
        );
        assert_eq!(
            refused_surface(check_in(home.path(), "page")).as_deref(),
            Some("screen")
        );
        assert!(check_in(home.path(), "get_config").is_ok());

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

    #[cfg(any(unix, windows))]
    #[test]
    fn screen_only_allows_reads_and_refuses_actions() {
        let home = joined_home();
        grant(home.path(), "granted", "none");
        assert!(check_in(home.path(), "get_window_state").is_ok());
        assert!(check_in(home.path(), "zoom").is_ok());
        assert_eq!(
            refused_surface(check_in(home.path(), "click")).as_deref(),
            Some("control")
        );
        assert_eq!(
            refused_surface(check_in(home.path(), "type_text")).as_deref(),
            Some("control")
        );
        assert_eq!(
            refused_surface(check_in(home.path(), "page")).as_deref(),
            Some("control")
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_revoke_takes_effect_on_the_next_call() {
        let home = joined_home();
        grant(home.path(), "granted", "granted");
        assert!(check_in(home.path(), "click").is_ok());
        assert!(check_in(home.path(), "page").is_ok());
        grant(home.path(), "granted", "none");
        assert_eq!(
            refused_surface(check_in(home.path(), "click")).as_deref(),
            Some("control")
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn control_alone_allows_no_action() {
        // Acting needs screen and control, as acpmux's bridge enforces.
        let home = joined_home();
        grant(home.path(), "none", "granted");
        for tool in ["click", "type_text", "launch_app", "page", "get_window_state"] {
            assert_eq!(
                refused_surface(check_in(home.path(), tool)).as_deref(),
                Some("screen"),
                "{tool}"
            );
        }
        assert!(check_in(home.path(), "get_config").is_ok());
    }

    #[test]
    fn the_unavailable_source_refuses_everything_that_needs_consent() {
        assert_eq!(Unavailable.status(), ConsentStatus::UNKNOWN);
        assert_eq!(
            refused_surface(check(&Unavailable, "get_window_state")).as_deref(),
            Some("screen")
        );
        assert_eq!(
            refused_surface(check(&Unavailable, "click")).as_deref(),
            Some("screen")
        );
        assert!(check(&Unavailable, "get_config").is_ok());
    }

    /// The build inside cmux.app never falls back to a file a caller could
    /// redirect; other builds keep the advisory file reader.
    #[test]
    fn the_default_source_follows_the_build() {
        let home = tempfile::tempdir().unwrap();
        // An "unjoined" state directory the caller chose.
        let unjoined = StateFiles::At(Some(home.path().to_owned()));
        assert!(!unjoined.status().consent_required);
        let default = default_source().status();
        if cfg!(feature = "cmux-host-consent") {
            assert_eq!(default, ConsentStatus::UNKNOWN);
        }
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
            assert!(
                check_in(home.path(), "get_window_state").is_err(),
                "{shown}"
            );
            assert!(check_in(home.path(), "click").is_err(), "{shown}");
        }
        let huge = format!(
            r#"{{"screen":"granted","control":"granted","pad":"{}"}}"#,
            "x".repeat(MAX_RECORD_BYTES as usize)
        );
        write_private(&home.path().join(CONSENT_FILE), huge.as_bytes());
        assert!(check_in(home.path(), "click").is_err());
    }

    #[test]
    fn a_bad_join_record_grants_nothing_even_with_consent() {
        for credential in [
            &b"{"[..],
            br#"{"credential":"cmm_test"}"#,
            br#"{"credential":1,"machine_id":"m","team_id":"t","name":"n","hub_origin":"h","revoked":false,"wgPublicKey":"k"}"#,
            br#"[]"#,
        ] {
            let home = tempfile::tempdir().unwrap();
            write_private(&home.path().join(CREDENTIAL_FILE), credential);
            grant(home.path(), "granted", "granted");
            assert_eq!(read_status(Some(home.path())), ConsentStatus::UNKNOWN);
            assert!(check_in(home.path(), "click").is_err());
            assert!(check_in(home.path(), "zoom").is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_record_others_could_write_or_a_link_grants_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let home = joined_home();
        grant(home.path(), "granted", "granted");
        let path = home.path().join(CONSENT_FILE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(check_in(home.path(), "click").is_err());

        let real = home.path().join("elsewhere.json");
        std::fs::rename(&path, &real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert!(check_in(home.path(), "click").is_err());

        // The same for the join record itself.
        let home = joined_home();
        grant(home.path(), "granted", "granted");
        assert!(check_in(home.path(), "click").is_ok());
        let credential = home.path().join(CREDENTIAL_FILE);
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o620)).unwrap();
        assert!(check_in(home.path(), "click").is_err());
        let real = home.path().join("credential-elsewhere.json");
        std::fs::rename(&credential, &real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &credential).unwrap();
        assert!(check_in(home.path(), "click").is_err());
    }

    #[test]
    fn an_unreadable_join_status_counts_as_joined() {
        // A credential path that is a directory or a dangling link is not a
        // clean "not joined": fail closed.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(CREDENTIAL_FILE)).unwrap();
        grant(home.path(), "granted", "granted");
        assert_eq!(read_status(Some(home.path())), ConsentStatus::UNKNOWN);
        assert!(check_in(home.path(), "click").is_err());

        #[cfg(unix)]
        {
            let home = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(home.path().join("gone"), home.path().join(CREDENTIAL_FILE))
                .unwrap();
            assert_eq!(read_status(Some(home.path())), ConsentStatus::UNKNOWN);
        }
    }

    #[test]
    fn the_windows_dacl_rule_trusts_only_the_owner_system_and_administrators() {
        use AccessEntry::*;
        use Trustee::*;
        let owner_only = [
            Allow {
                trustee: CurrentUser,
                mask: 0x001f_01ff,
            },
            Allow {
                trustee: System,
                mask: 0x001f_01ff,
            },
            Allow {
                trustee: Administrators,
                mask: 0x001f_01ff,
            },
        ];
        assert!(dacl_is_private(CurrentUser, Some(&owner_only)));
        assert!(dacl_is_private(Administrators, Some(&owner_only)));
        assert!(dacl_is_private(System, Some(&owner_only)));
        // Owned by another account, or no DACL at all (everyone has access).
        assert!(!dacl_is_private(Other, Some(&owner_only)));
        assert!(!dacl_is_private(CurrentUser, None));
        // Another trustee may read but not write.
        let others_read = [
            owner_only[0],
            Allow {
                trustee: Other,
                mask: 0x0012_0089,
            },
        ];
        assert!(dacl_is_private(CurrentUser, Some(&others_read)));
        for write_like in [
            0x2,
            0x4,
            0x10,
            0x100,
            0x1_0000,
            0x4_0000,
            0x8_0000,
            0x200_0000,
            0x1000_0000,
            0x4000_0000,
        ] {
            let others_write = [
                owner_only[0],
                Allow {
                    trustee: Other,
                    mask: write_like,
                },
            ];
            assert!(
                !dacl_is_private(CurrentUser, Some(&others_write)),
                "{write_like:#x}"
            );
        }
        // Deny entries only take rights away; unknown entry types are refused.
        assert!(dacl_is_private(CurrentUser, Some(&[owner_only[0], Deny])));
        assert!(!dacl_is_private(
            CurrentUser,
            Some(&[owner_only[0], Unknown])
        ));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_record_another_account_can_write_grants_nothing() {
        let home = joined_home();
        grant(home.path(), "granted", "granted");
        assert!(
            check_in(home.path(), "click").is_ok(),
            "an owner-only temp file is trusted"
        );
        // Everyone (S-1-1-0) gets write access to the consent record.
        let status = std::process::Command::new("icacls")
            .arg(home.path().join(CONSENT_FILE))
            .args(["/grant", "*S-1-1-0:(W)"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(check_in(home.path(), "click").is_err());
        // The same for the join record.
        let home = joined_home();
        grant(home.path(), "granted", "granted");
        let status = std::process::Command::new("icacls")
            .arg(home.path().join(CREDENTIAL_FILE))
            .args(["/grant", "*S-1-1-0:(W)"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(check_in(home.path(), "click").is_err());
    }

    #[test]
    fn refusal_text_never_carries_paths_or_credentials() {
        let home = joined_home();
        let error = check_in(home.path(), "click").unwrap_err();
        let text = serde_json::to_string(&error).unwrap();
        assert!(!text.contains("cmm_"));
        assert!(!text.contains(&home.path().display().to_string()));
    }

    #[test]
    fn every_known_tool_is_classified() {
        // Every tool name that ships today is in an explicit arm, so only a
        // new, unclassified tool falls through to `Both`. The Linux
        // integration test checks the live tools/list against this too.
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
