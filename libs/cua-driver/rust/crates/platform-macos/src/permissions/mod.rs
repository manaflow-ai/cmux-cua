//! macOS TCC permission checks + first-launch CLI gate.
//!
//! Two layers:
//!   - [`status`] — low-level booleans for Accessibility / Screen Recording.
//!   - [`gate`]   — startup-time interactive flow that walks the user through
//!                  granting the missing permissions before `serve` binds.
//!
//! The gate is a Rust port of Swift's `PermissionsGate` (SwiftUI panel).
//! Two presentation surfaces:
//!   - native NSPanel (`panel`, Phase 1+) — used when the daemon is launched
//!     from the bundled `.app` and the env-var opt-out is not set;
//!   - terminal banner (`gate::wait_for_grants`) — fallback for bare-binary
//!     invocations, headless environments, CI, and explicit opt-outs.
//!
//! Mirrors `libs/cua-driver/Sources/CuaDriverCore/Permissions/`:
//!   - `Permissions.swift`      → `permissions::status`
//!   - `PermissionsGate.swift`  → `permissions::gate` + `permissions::panel`

pub mod gate;
pub mod status;

#[cfg(target_os = "macos")]
pub mod panel;

pub use status::{PermissionsStatus, current_status};
pub use gate::{GateOpts, MissingPermission, run_if_needed};

/// Whether an embedding host owns permission onboarding and prompt timing.
/// The branded executable fallback keeps cmux's helper silent even if an
/// incomplete inherited environment omits the explicit flag.
pub(crate) fn external_permission_flow_enabled() -> bool {
    let from_environment = std::env::var("CUA_DRIVER_RS_EXTERNAL_PERMISSION_FLOW")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    let cmux_branded_executable = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_owned()))
        .and_then(|name| name.to_str().map(str::to_owned))
        .as_deref()
        == Some("cmux-cua-driver");
    from_environment || cmux_branded_executable
}

/// Crate-wide lock serializing tests that mutate process-global env vars.
/// Per-module locks are not enough: `gate` and `check_permissions` tests
/// share `CUA_DRIVER_EMBEDDED`.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    // Poison carries no stale invariant (tests restore the vars they touch).
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
