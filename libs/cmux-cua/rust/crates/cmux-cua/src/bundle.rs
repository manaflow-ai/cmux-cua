//! macOS bundle-context detection for the TCC auto-relaunch path.
//!
//! Mirrors `libs/cmux-cua/Sources/CmuxCuaCLI/BundleHelpers.swift`'s
//! `isExecutableInsideCmuxCuaApp()` — the heuristic that decides
//! whether `cmux-cua mcp` was spawned from an IDE terminal as a
//! bare CLI symlinked into our .app bundle. When true and the parent
//! isn't launchd, we re-launch the daemon via `open -n -g -a
//! CmuxCua --args serve` so it picks up the bundle's TCC grants,
//! then proxy stdio MCP traffic through the daemon's Unix socket.
//!
//! Non-macOS targets compile to no-ops so the cross-platform call
//! sites stay tidy.

/// Returns `true` when the currently-running binary resolves into an
/// installed `cmux Computer Use.app` bundle (Rust port). The check is the
/// same shape as the Swift driver's `isExecutableInsideCmuxCuaApp`
/// (`/cmux Computer Use.app/Contents/MacOS/`) but keyed on the Rust port's
/// distinct bundle name so the two installs don't collide.
///
/// `false` for raw `cargo run` / `target/release/cmux-cua` dev
/// invocations — there's no installed bundle to relaunch into, so the
/// caller should stay in-process.
///
/// Implementation:
///   1. Resolve `std::env::current_exe()` (preferred; absolute path
///      to the running image).
///   2. Walk symlinks via `std::fs::canonicalize` — the install layout
///      is `~/.local/bin/cmux-cua` → `/Applications/cmux Computer Use.app/
///      Contents/MacOS/cmux-cua`, so without the canonicalize step
///      we'd see the bare symlink path and miss the bundle.
///   3. Substring-match the canonical path for the bundle marker.
#[cfg(target_os = "macos")]
pub fn is_executable_inside_cmux_cua_app() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let canonical = match std::fs::canonicalize(&exe) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let s = match canonical.to_str() {
        Some(s) => s,
        None => return false,
    };
    s.contains("/cmux Computer Use.app/Contents/MacOS/")
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)] // Non-macOS stub kept for API symmetry — see module header.
pub fn is_executable_inside_cmux_cua_app() -> bool {
    false
}

/// Returns `true` when the parent process is *not* `launchd` (pid 1).
/// Combined with [`is_executable_inside_cmux_cua_app`], a `true`
/// here means the binary was spawned from a shell / IDE terminal that
/// inherits the wrong TCC responsibility — i.e. the case we want to
/// auto-relaunch from.
///
/// `ppid == 1` means launchd reparented us (we're already running as
/// the LaunchServices-spawned daemon). In that case we stay
/// in-process: TCC grants are already correct, and relaunching would
/// fork-bomb the daemon back into existence on every `mcp` startup.
///
/// Mirrors Swift's `if getppid() == 1 { return false }` gate in
/// `MCPCommand.shouldUseDaemonProxy()`.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parent_is_not_launchd() -> bool {
    // SAFETY: `libc::getppid` is a thread-safe POSIX getter that
    // takes no args and returns the parent pid. No invariants to
    // uphold, no UB to risk.
    let ppid = unsafe { libc::getppid() };
    ppid != 1
}

#[cfg(not(unix))]
#[allow(dead_code)] // Non-unix stub kept for API symmetry — see module header.
pub fn parent_is_not_launchd() -> bool {
    // No launchd on non-Unix; the heuristic is macOS-only anyway.
    // Returning false keeps the caller in-process on unsupported
    // platforms (same effective outcome as the macOS check failing).
    false
}

/// Returns `true` when the env var is one of `1|true|yes|on`
/// (case-insensitive). Anything else, including unset, is falsy.
///
/// Mirrors Swift's `isEnvTruthy` helper on `MCPCommand`.
pub fn is_env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Returns `true` for the executable name cmux uses for every bundled proxy
/// and helper binary. Unlike an environment variable, the current executable
/// name cannot be changed by an ambient agent-session environment.
pub fn is_cmux_branded_executable() -> bool {
    std::env::current_exe()
        .ok()
        .as_deref()
        .is_some_and(is_cmux_branded_executable_path)
}

fn is_cmux_branded_executable_path(path: &std::path::Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("cmux-cua")
}

/// Returns whether the current image is the executable of cmux's branded
/// helper app. A copy under `Contents/Resources/bin` is only a proxy artifact;
/// allowing it to own TCC-protected work creates a second path-based Privacy &
/// Security identity beside `cmux Computer Use`.
#[cfg(target_os = "macos")]
pub fn is_executable_inside_cmux_helper_app() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .as_deref()
        .is_some_and(is_executable_inside_cmux_helper_app_path)
}

#[cfg(not(target_os = "macos"))]
pub fn is_executable_inside_cmux_helper_app() -> bool {
    false
}

fn is_executable_inside_cmux_helper_app_path(path: &std::path::Path) -> bool {
    path.ends_with(std::path::Path::new(
        "cmux Computer Use.app/Contents/MacOS/cmux-cua",
    ))
}

/// Recovery text returned to agents that encounter a missing cmux-owned
/// daemon. It intentionally names the supported host action and forbids the
/// raw CLI fallback that would create another macOS permission identity.
pub const CMUX_RUNTIME_RECOVERY_GUIDANCE: &str = "Do not run `cmux-cua serve`, `call`, or `permissions` directly. Open cmux Settings > Computer Use, or toggle Computer Use off and on, to recover the branded cmux Computer Use helper.";

/// Whether an embedding host owns daemon lifecycle and permission UX.
///
/// cmux sets both environment flags on its MCP proxy. The executable-name
/// fallback keeps a directly invoked bundled binary fail-closed even when an
/// agent supplies a hostile or incomplete environment.
pub fn requires_external_daemon() -> bool {
    is_cmux_branded_executable()
        || is_env_truthy("CMUX_CUA_MCP_FORCE_PROXY")
        || is_env_truthy("CMUX_CUA_EXTERNAL_PERMISSION_FLOW")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_run_is_not_inside_bundle() {
        // The unit-test runner image lives under `target/<config>/
        // deps/`, never inside a .app bundle. Should always return
        // false in CI / local dev, which is exactly the behavior we
        // want so `cargo run` callers stay in-process.
        assert!(!is_executable_inside_cmux_cua_app());
    }

    #[test]
    fn unset_env_is_falsy() {
        // Use a deliberately unlikely name so we don't depend on the
        // surrounding shell environment.
        std::env::remove_var("CMUX_CUA_TEST_UNSET_NAME");
        assert!(!is_env_truthy("CMUX_CUA_TEST_UNSET_NAME"));
    }

    #[test]
    fn truthy_env_values_recognized() {
        let name = "CMUX_CUA_TEST_TRUTHY";
        for v in ["1", "true", "TRUE", "Yes", "on", " 1 "] {
            std::env::set_var(name, v);
            assert!(is_env_truthy(name), "expected truthy for {v:?}");
        }
        for v in ["0", "false", "no", "off", ""] {
            std::env::set_var(name, v);
            assert!(!is_env_truthy(name), "expected falsy for {v:?}");
        }
        std::env::remove_var(name);
    }

    #[test]
    #[cfg(unix)]
    fn parent_is_not_launchd_in_tests() {
        // The cargo test harness is reparented under whatever
        // launched it (cargo / IDE / shell), not directly under
        // launchd. The helper should report true.
        assert!(parent_is_not_launchd());
    }

    #[test]
    fn cmux_branded_binary_requires_external_daemon() {
        assert!(is_cmux_branded_executable_path(std::path::Path::new(
            "/Applications/cmux.app/Contents/Resources/bin/cmux-cua"
        )));
        assert!(is_cmux_branded_executable_path(std::path::Path::new(
            "/Library/Application Support/cmux/computer-use/helper/tag/cmux Computer Use.app/Contents/MacOS/cmux-cua"
        )));
        assert!(!is_cmux_branded_executable_path(std::path::Path::new(
            "/Applications/cmux Computer Use.app/Contents/MacOS/cmux-cua"
        )));
    }

    #[test]
    fn only_the_branded_helper_bundle_is_a_cmux_permission_owner() {
        assert!(is_executable_inside_cmux_helper_app_path(std::path::Path::new(
            "/Applications/cmux.app/Contents/Library/cmux Computer Use.app/Contents/MacOS/cmux-cua"
        )));
        assert!(is_executable_inside_cmux_helper_app_path(std::path::Path::new(
            "/Library/Application Support/cmux/computer-use/helper/tag/cmux Computer Use.app/Contents/MacOS/cmux-cua"
        )));
        assert!(!is_executable_inside_cmux_helper_app_path(std::path::Path::new(
            "/Applications/cmux.app/Contents/Resources/bin/cmux-cua"
        )));
    }
}
