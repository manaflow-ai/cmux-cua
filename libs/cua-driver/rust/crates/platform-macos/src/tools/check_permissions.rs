use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;

use crate::permissions::status::{
    accessibility_granted, request_accessibility, request_screen_recording,
    screen_recording_granted,
};

pub struct CheckPermissionsTool;

const SCREEN_CAPTURE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenCaptureProbeBackend {
    Stream,
    ScreenshotManager,
}

fn screen_capture_probe_backend(macos_major_version: isize) -> ScreenCaptureProbeBackend {
    if macos_major_version >= 14 {
        ScreenCaptureProbeBackend::ScreenshotManager
    } else {
        ScreenCaptureProbeBackend::Stream
    }
}

/// (A) Real ScreenCaptureKit capability probe — what THIS process can
/// actually capture right now, independent of the CGPreflight cache.
///
/// `CGPreflightScreenCaptureAccess()` (used by `screen_recording_granted`)
/// answers from a per-process cache that goes stale after `tccutil reset`
/// and is unreliable for CLI / child processes — the same finding Peekaboo
/// documents. Display enumeration alone is not sufficient on macOS Tahoe:
/// `SCShareableContent::get()` can return displays while the separate
/// direct-capture consent alert is still waiting for a decision. Readiness
/// therefore requires one real frame. macOS 14 and newer use
/// `SCScreenshotManager`; macOS 13 uses its supported one-frame `SCStream`
/// equivalent.
pub(super) async fn screen_recording_capturable() -> bool {
    use screencapturekit::{
        async_api::{AsyncSCScreenshotManager, AsyncSCShareableContent},
        prelude::{SCContentFilter, SCStreamConfiguration},
    };

    let Ok(Ok(content)) = tokio::time::timeout(
        SCREEN_CAPTURE_PROBE_TIMEOUT,
        AsyncSCShareableContent::get(),
    )
    .await
    else {
        return false;
    };
    let Some(display) = content.displays().into_iter().next() else {
        return false;
    };
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();
    let configuration = SCStreamConfiguration::new().with_width(1).with_height(1);
    let major_version = objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion;
    match screen_capture_probe_backend(major_version) {
        ScreenCaptureProbeBackend::Stream => {
            capture_one_frame_with_stream(&filter, &configuration).await
        }
        ScreenCaptureProbeBackend::ScreenshotManager => {
            capture_probe_with_timeout(
                SCREEN_CAPTURE_PROBE_TIMEOUT,
                async {
                    AsyncSCScreenshotManager::capture_image(&filter, &configuration)
                        .await
                        .is_ok_and(|image| image.width() > 0 && image.height() > 0)
                },
            )
            .await
        }
    }
}

async fn capture_one_frame_with_stream(
    filter: &screencapturekit::prelude::SCContentFilter,
    configuration: &screencapturekit::prelude::SCStreamConfiguration,
) -> bool {
    use screencapturekit::{
        async_api::AsyncSCStream,
        cm::{CMSampleBufferExt, CMSampleBufferSCExt},
        prelude::SCStreamOutputType,
    };
    use std::sync::{Arc, Mutex};

    let stream = Arc::new(AsyncSCStream::new(
        filter,
        configuration,
        1,
        SCStreamOutputType::Screen,
    ));
    let lifecycle = Arc::new(Mutex::new(()));
    let start_task = {
        let stream = Arc::clone(&stream);
        let lifecycle = Arc::clone(&lifecycle);
        tokio::task::spawn_blocking(move || {
            let _guard = lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            stream.start_capture().is_ok()
        })
    };
    let receive_stream = Arc::clone(&stream);
    let stop_stream = Arc::clone(&stream);
    let stop_lifecycle = Arc::clone(&lifecycle);
    capture_stream_lifecycle_with_timeout(
        SCREEN_CAPTURE_PROBE_TIMEOUT,
        start_task,
        move || async move {
            while let Some(sample) = receive_stream.next().await {
                if crate::application_surface::capture_frame_status_is_publishable(
                    sample.frame_status(),
                )
                    && sample.image_buffer().is_some_and(|pixel_buffer| {
                        pixel_buffer.width() > 0 && pixel_buffer.height() > 0
                    })
                {
                    return true;
                }
            }
            false
        },
        move || {
            tokio::task::spawn_blocking(move || {
                let _guard = stop_lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                stop_stream.stop_capture().is_ok()
            })
        },
    )
    .await
}

async fn capture_stream_lifecycle_with_timeout<Receive, ReceiveFuture, Stop>(
    timeout: std::time::Duration,
    start_task: tokio::task::JoinHandle<bool>,
    receive: Receive,
    stop: Stop,
) -> bool
where
    Receive: FnOnce() -> ReceiveFuture,
    ReceiveFuture: std::future::Future<Output = bool>,
    Stop: FnOnce() -> tokio::task::JoinHandle<bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let started = capture_probe_task_before(deadline, start_task).await;
    let received = if started {
        tokio::time::timeout_at(deadline, receive())
            .await
            .unwrap_or(false)
    } else {
        false
    };

    // Schedule shutdown even after the readiness deadline. The lifecycle mutex
    // keeps it behind an in-flight start, while spawn_blocking keeps both
    // synchronous ScreenCaptureKit completion waits off Tokio's async workers.
    // Cleanup gets its own bound and cannot negate a frame that proved capture.
    let stop_task = stop();
    let _cleanup_task = tokio::spawn(async move {
        let _ = tokio::time::timeout(timeout, stop_task).await;
    });
    started && received
}

async fn capture_probe_task_before(
    deadline: tokio::time::Instant,
    task: tokio::task::JoinHandle<bool>,
) -> bool {
    tokio::time::timeout_at(deadline, task)
        .await
        .is_ok_and(|result| result.unwrap_or(false))
}

async fn capture_probe_with_timeout(
    timeout: std::time::Duration,
    probe: impl std::future::Future<Output = bool>,
) -> bool {
    tokio::time::timeout(timeout, probe)
        .await
        .unwrap_or(false)
}

#[cfg(test)]
fn verify_screen_capture_with<Display>(
    load_display: impl FnOnce() -> Option<Display>,
    capture_frame: impl FnOnce(&Display) -> bool,
) -> bool {
    load_display().as_ref().is_some_and(capture_frame)
}

/// Run the ScreenCaptureKit probe only on an explicitly prompt-capable path.
/// `SCShareableContent::get()` can itself register/raise TCC, so `None` is the
/// only truthful answer when a silent or host-owned flow skips that probe.
async fn maybe_screen_recording_capture_probe<Probe, ProbeFuture>(
    should_probe: bool,
    probe: Probe,
) -> Option<bool>
where
    Probe: FnOnce() -> ProbeFuture,
    ProbeFuture: std::future::Future<Output = bool>,
{
    if should_probe {
        Some(probe().await)
    } else {
        None
    }
}

/// (B) Which TCC identity the booleans in this response reflect.
///
/// macOS attributes Accessibility / Screen-Recording to the *responsible
/// process* (the LaunchServices launching app), not the executable path.
/// So `check_permissions` answered in-process reflects:
///   - the **CuaDriver daemon** (`com.trycua.driver`) when this process is
///     its own responsible process — the real driver status.
///   - the **calling app** otherwise — e.g. the terminal/IDE that spawned
///     `cua-driver call …`. That grant is NOT the driver's, which is why a
///     standalone check can read `true` while `tccutil … com.trycua.driver`
///     reports no record.
fn permission_source() -> serde_json::Value {
    let pid = unsafe { libc::getpid() };
    let ppid = unsafe { libc::getppid() };
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_default();
    let disclaimed = std::env::var_os(cua_driver_core::RESPONSIBILITY_DISCLAIMED_ENV).is_some();
    let embedded = cua_driver_core::embedded_mode();
    let host_bundle_id = std::env::var(cua_driver_core::HOST_BUNDLE_ID_ENV).unwrap_or_default();
    let bundle_identifier = super::health_report::current_bundle_identifier();
    permission_source_for_context(
        pid,
        ppid,
        &exe,
        disclaimed,
        embedded,
        &host_bundle_id,
        bundle_identifier.as_deref(),
    )
}

fn permission_source_for_context(
    pid: libc::pid_t,
    ppid: libc::pid_t,
    exe: &str,
    disclaimed: bool,
    embedded: bool,
    host_bundle_id: &str,
    bundle_identifier: Option<&str>,
) -> serde_json::Value {
    // Embedded mode: the driver is a child in a host app's responsibility
    // chain, so the probes already answer for the host's TCC identity.
    // This branch only ever downgrades attribution (host, never
    // driver-daemon), so the caller-controlled env var can't spoof an
    // elevated identity. `host_bundle_id` is advisory, not a trust signal.
    if embedded {
        return serde_json::json!({
            "attribution": "host",
            "host_bundle_id": host_bundle_id,
            "embedded": true,
            "pid": pid,
            "responsible_ppid": ppid,
            "executable": exe,
            "disclaim_env": disclaimed,
            "note": "Embedded mode: these booleans reflect the HOST app's TCC \
                     grant (the driver is a child in the host's responsibility \
                     chain). No separate driver grant exists or is needed. If a \
                     permission is NOT granted, the host app must request it — \
                     the driver never raises its own prompt.",
        });
    }
    // The trustworthy, non-spoofable signal is the executable path: a caller
    // can't run from inside the code-signed `CuaDriver.app` bundle without
    // controlling that install. The disclaim env var is caller-controlled, so
    // it is treated only as a corroborating signal that explains why a
    // bundle-resident daemon has `ppid != 1` (it re-exec'd itself with
    // responsibility disclaim, so launchd is no longer its parent). On its own
    // — outside the bundle — the env var must NOT grant daemon attribution, or
    // a caller could pre-set it and spoof the TCC source. Fail closed to
    // "caller" whenever the bundle signal is absent.
    let inside_app_bundle = exe.contains(".app/Contents/MacOS/");
    let is_responsible_app = inside_app_bundle
        && bundle_identifier.is_some_and(|identifier| !identifier.is_empty())
        && (ppid == 1 || disclaimed);

    let (attribution, note) = if is_responsible_app
        && bundle_identifier == Some(super::health_report::CANONICAL_BUNDLE_ID)
    {
        (
            "driver-daemon",
            "These booleans reflect the CuaDriver daemon's own TCC identity \
             (com.trycua.driver) because this process is its own responsible \
             process.",
        )
    } else if is_responsible_app {
        (
            "helper-daemon",
            "These booleans reflect this helper application's own TCC identity. Permission setup belongs to the embedding host; do not run the standalone cua-driver permission flow.",
        )
    } else {
        (
            "caller",
            "These booleans reflect the TCC identity of the app that launched \
             this process (e.g. your terminal/IDE), NOT the CuaDriver daemon \
             (com.trycua.driver). A standalone check can read `true` here while \
             `tccutil … com.trycua.driver` reports no record. To grant for the \
             driver, run `cua-driver permissions grant`.",
        )
    };

    serde_json::json!({
        "attribution": attribution,
        "pid": pid,
        "responsible_ppid": ppid,
        "executable": exe,
        "bundle_identifier": bundle_identifier,
        "disclaim_env": disclaimed,
        "note": note,
    })
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        // Matches Swift `CheckPermissionsTool.swift` description verbatim.
        name: "check_permissions".into(),
        description: "Report TCC permission status for Accessibility and Screen Recording. \
            By default also raises the system permission dialogs for any missing grants — \
            Apple's request APIs are no-ops when the grant is already active, so this is \
            safe to call repeatedly. Pass {\"prompt\": false} for a purely read-only \
            status check.\n\n\
            Returns: `accessibility` + `screen_recording` (booleans from the TCC \
            preflight APIs), `screen_recording_capturable` (true/false only when a \
            live ScreenCaptureKit probe ran; null for prompt:false, embedded, or \
            external permission flows), `screen_recording_probe_performed`, and \
            `source` (which TCC identity the \
            booleans reflect: the responsible daemon app vs the launching terminal/IDE). \
            macOS attributes grants to the responsible process, so a standalone call \
            from a terminal reports the terminal's grants, not the driver's.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "boolean",
                    "description": "Raise the system permission prompts for missing grants. Default true.",
                }
            },
            "additionalProperties": false,
        }),
        // Not read_only because the default path may raise a modal dialog
        // (mirrors Swift annotation `readOnlyHint: false`).
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for CheckPermissionsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        // Default to prompting — same default + rationale as Swift.
        // Embedded mode hard-disables prompting regardless of the arg (the
        // host owns the grant flow). This and the startup gate are the only
        // `request_*` call sites, so both being gated makes prompts
        // unreachable when embedded.
        let should_prompt = args.bool_or("prompt", true)
            && !cua_driver_core::embedded_mode()
            && !super::health_report::external_permission_flow_enabled();
        if should_prompt {
            let _ = request_accessibility();
            let _ = request_screen_recording();
        }
        let accessibility = accessibility_granted();
        let screen_recording = screen_recording_granted();
        // (A) Authoritative live probe — see `screen_recording_capturable`.
        //
        // CRITICAL: `SCShareableContent::get()` REGISTERS this process with TCC
        // and RAISES the Screen Recording system prompt when the grant is
        // missing. That is a real side effect, so it must only happen when the
        // caller opted into prompting (`prompt:true`). A read-only status query
        // (`prompt:false`) — e.g. a host app refreshing the helper's status when
        // an agent session merely starts — MUST stay silent; running the live
        // probe there pops a permission dialog before the user has done anything
        // with computer use. When we're not allowed to prompt, report capture
        // status as unknown instead of mislabelling the preflight boolean as a
        // live result.
        let screen_recording_capturable = maybe_screen_recording_capture_probe(
            should_prompt,
            screen_recording_capturable,
        )
        .await;
        // (B) Which identity the booleans above belong to.
        let source = permission_source();

        permission_result(
            accessibility,
            screen_recording,
            screen_recording_capturable,
            source,
        )
    }
}

fn permission_result(
    accessibility: bool,
    screen_recording: bool,
    screen_recording_capturable: Option<bool>,
    source: serde_json::Value,
) -> ToolResult {
    let is_caller = source.get("attribution").and_then(|v| v.as_str()) == Some("caller");
    // Text format mirrors Swift 1:1:
    //   "✅ Accessibility: granted.\n✅ Screen Recording: granted."
    let ax_prefix = if accessibility { "✅" } else { "❌" };
    let sr_prefix = if screen_recording { "✅" } else { "❌" };
    let ax_state = if accessibility {
        "granted"
    } else {
        "NOT granted"
    };
    let sr_state = if screen_recording {
        "granted"
    } else {
        "NOT granted"
    };
    let mut summary = format!(
        "{ax_prefix} Accessibility: {ax_state}.\n{sr_prefix} Screen Recording: {sr_state}."
    );
    // Flag a preflight/probe disagreement (the false-positive tell).
    if screen_recording && screen_recording_capturable == Some(false) {
        summary.push_str(
            "\n⚠️  Screen Recording reads granted but a live capture probe failed — \
             the grant likely belongs to a different process, not this one.",
        );
    }
    // Make the attribution explicit when answering for a host or caller
    // (not the daemon).
    if source.get("attribution").and_then(|v| v.as_str()) == Some("host") {
        summary.push_str(
            "\nℹ️  Embedded mode: status reflects the HOST app's TCC grant. \
             If a permission is missing, the host must request it — the \
             driver will not prompt.",
        );
    }
    if is_caller {
        summary.push_str(
            "\nℹ️  Status reflects the launching app's TCC identity, not the CuaDriver \
             daemon (com.trycua.driver). See `source` for details.",
        );
    }

    ToolResult::text(summary).with_structured(serde_json::json!({
        "accessibility":               accessibility,
        "screen_recording":            screen_recording,
        "screen_recording_capturable": screen_recording_capturable,
        "screen_recording_probe_performed": screen_recording_capturable.is_some(),
        "source":                      source,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::permissions::test_env_lock()
    }

    /// Set/remove `var`, returning the original for restore. Callers must
    /// hold `env_lock()`.
    fn swap_env(var: &str, value: Option<&str>) -> Option<std::ffi::OsString> {
        let original = std::env::var_os(var);
        match value {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        original
    }

    fn restore_env(var: &str, original: Option<std::ffi::OsString>) {
        match original {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn disclaim_env_var_alone_does_not_grant_daemon_attribution() {
        // The disclaim env var is caller-controlled, so on its own it must not
        // make `check_permissions` claim the booleans reflect the daemon's TCC
        // identity. Daemon attribution additionally requires the binary to live
        // inside the code-signed `CuaDriver.app` bundle — the test runner does
        // not, so even with the env var present we must fail closed to "caller".
        let _guard = env_lock();
        let name = cua_driver_core::RESPONSIBILITY_DISCLAIMED_ENV;
        let original = swap_env(name, Some("1"));
        let embedded = swap_env(cua_driver_core::EMBEDDED_ENV, None);

        let source = permission_source();
        assert_eq!(
            source.get("attribution").and_then(|v| v.as_str()),
            Some("caller"),
            "env-var presence alone must not yield daemon attribution"
        );

        restore_env(cua_driver_core::EMBEDDED_ENV, embedded);
        restore_env(name, original);
    }

    #[test]
    fn embedded_mode_reports_host_attribution() {
        let _guard = env_lock();
        let embedded = swap_env(cua_driver_core::EMBEDDED_ENV, Some("1"));
        let host = swap_env(
            cua_driver_core::HOST_BUNDLE_ID_ENV,
            Some("com.example.host"),
        );

        let source = permission_source();
        assert_eq!(
            source.get("attribution").and_then(|v| v.as_str()),
            Some("host"),
        );
        assert_eq!(
            source.get("host_bundle_id").and_then(|v| v.as_str()),
            Some("com.example.host"),
        );
        assert_eq!(source.get("embedded").and_then(|v| v.as_bool()), Some(true));

        restore_env(cua_driver_core::HOST_BUNDLE_ID_ENV, host);
        restore_env(cua_driver_core::EMBEDDED_ENV, embedded);
    }

    #[test]
    fn embedded_plus_disclaim_env_never_yields_daemon_attribution() {
        // Both caller-controlled env vars together must still not produce
        // "driver-daemon" — embedded mode may only DOWNGRADE attribution.
        let _guard = env_lock();
        let embedded = swap_env(cua_driver_core::EMBEDDED_ENV, Some("1"));
        let disclaim = swap_env(cua_driver_core::RESPONSIBILITY_DISCLAIMED_ENV, Some("1"));

        let source = permission_source();
        assert_eq!(
            source.get("attribution").and_then(|v| v.as_str()),
            Some("host"),
        );

        restore_env(cua_driver_core::RESPONSIBILITY_DISCLAIMED_ENV, disclaim);
        restore_env(cua_driver_core::EMBEDDED_ENV, embedded);
    }

    #[test]
    fn embedded_env_requires_exact_value_one() {
        let _guard = env_lock();
        let embedded = swap_env(cua_driver_core::EMBEDDED_ENV, Some("true"));
        let source = permission_source();
        assert_ne!(
            source.get("attribution").and_then(|v| v.as_str()),
            Some("host"),
            "only CUA_DRIVER_EMBEDDED=1 may enable embedded mode"
        );
        restore_env(cua_driver_core::EMBEDDED_ENV, embedded);
    }

    #[test]
    fn cmux_helper_reports_its_own_daemon_identity() {
        let source = permission_source_for_context(
            42,
            1,
            "/Library/Application Support/cmux/computer-use/helper/tag/cmux Computer Use.app/Contents/MacOS/cmux-cua-driver",
            true,
            false,
            "",
            Some("com.cmuxterm.app.debug.tag.computer-use"),
        );

        assert_eq!(
            source.get("attribution").and_then(|value| value.as_str()),
            Some("helper-daemon"),
        );
        assert_eq!(
            source
                .get("bundle_identifier")
                .and_then(|value| value.as_str()),
            Some("com.cmuxterm.app.debug.tag.computer-use"),
        );
        assert!(
            !source["note"]
                .as_str()
                .unwrap_or_default()
                .contains("permissions grant"),
            "a host-owned helper must never recommend the standalone permission flow",
        );
    }

    #[tokio::test]
    async fn silent_check_reports_capture_unknown_without_running_probe() {
        let probe_called = std::cell::Cell::new(false);
        let capturable = maybe_screen_recording_capture_probe(false, || {
            probe_called.set(true);
            std::future::ready(true)
        })
        .await;
        assert_eq!(capturable, None);
        assert!(!probe_called.get(), "silent status must not call SCShareableContent");

        let result = permission_result(
            true,
            true,
            capturable,
            serde_json::json!({ "attribution": "helper-daemon" }),
        );
        let structured = result.structured_content.as_ref().expect("structured status");
        assert!(structured["screen_recording_capturable"].is_null());
        assert_eq!(structured["screen_recording_probe_performed"], false);
        let text = result.content.iter().find_map(|content| match content {
            cua_driver_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        }).unwrap_or_default();
        assert!(!text.contains("live capture probe failed"));
    }

    #[test]
    fn capture_readiness_requires_a_real_frame_after_display_enumeration() {
        let captures = std::cell::Cell::new(0);
        assert!(!verify_screen_capture_with(
            || None::<u8>,
            |_| {
                captures.set(captures.get() + 1);
                true
            }
        ));
        assert_eq!(captures.get(), 0);

        assert!(!verify_screen_capture_with(
            || Some(7_u8),
            |_| {
                captures.set(captures.get() + 1);
                false
            }
        ));
        assert_eq!(captures.get(), 1);

        assert!(verify_screen_capture_with(
            || Some(7_u8),
            |_| {
                captures.set(captures.get() + 1);
                true
            }
        ));
        assert_eq!(captures.get(), 2);
    }

    #[test]
    fn capture_probe_keeps_the_macos_thirteen_compatible_backend() {
        assert_eq!(
            screen_capture_probe_backend(13),
            ScreenCaptureProbeBackend::Stream
        );
        assert_eq!(
            screen_capture_probe_backend(14),
            ScreenCaptureProbeBackend::ScreenshotManager
        );
    }

    #[tokio::test]
    async fn capture_probe_timeout_reports_not_ready() {
        assert!(
            !capture_probe_with_timeout(
                std::time::Duration::from_millis(10),
                std::future::pending(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn stream_probe_timeout_defers_shutdown_until_start_finishes() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let receive_called = Arc::new(AtomicBool::new(false));
        let stop_scheduled = Arc::new(AtomicBool::new(false));
        let (release_start_tx, release_start_rx) = tokio::sync::oneshot::channel();
        let receive_marker = Arc::clone(&receive_called);
        let stop_marker = Arc::clone(&stop_scheduled);
        let result = capture_stream_lifecycle_with_timeout(
            std::time::Duration::from_millis(10),
            tokio::spawn(async move { release_start_rx.await.is_ok() }),
            move || {
                receive_marker.store(true, Ordering::Release);
                std::future::ready(true)
            },
            move || {
                stop_marker.store(true, Ordering::Release);
                tokio::spawn(async { true })
            },
        )
        .await;

        assert!(!result);
        assert!(!receive_called.load(Ordering::Acquire));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!stop_scheduled.load(Ordering::Acquire));

        release_start_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !stop_scheduled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stream_probe_cleanup_cannot_negate_a_valid_frame() {
        let result = capture_stream_lifecycle_with_timeout(
            std::time::Duration::from_millis(10),
            tokio::spawn(async { true }),
            || std::future::ready(true),
            || tokio::spawn(std::future::pending::<bool>()),
        )
        .await;

        assert!(result);
    }

    #[tokio::test]
    async fn live_check_reports_probe_true_or_false_and_warns_only_on_false() {
        for (live, should_warn) in [(true, false), (false, true)] {
            let capturable =
                maybe_screen_recording_capture_probe(true, || std::future::ready(live)).await;
            assert_eq!(capturable, Some(live));
            let result = permission_result(
                true,
                true,
                capturable,
                serde_json::json!({ "attribution": "helper-daemon" }),
            );
            let structured = result.structured_content.as_ref().expect("structured status");
            assert_eq!(structured["screen_recording_capturable"], live);
            assert_eq!(structured["screen_recording_probe_performed"], true);
            let text = result.content.iter().find_map(|content| match content {
                cua_driver_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            }).unwrap_or_default();
            assert_eq!(text.contains("live capture probe failed"), should_warn);
        }
    }
}
