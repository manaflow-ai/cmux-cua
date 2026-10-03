//! macOS `bring_to_front`.
//!
//! `bring_to_front` exists to let an agent pay a one-shot, persistent foreground
//! swap before driving a focus-proxy target — a window that only accepts input
//! while its host app genuinely holds activation. The macOS input rungs never
//! need this internally: every `CGEvent.postToPid` dispatch reaches a
//! backgrounded window, and the `dispatch:"foreground"` rung does its own
//! sub-millisecond front→act→restore flash. The one surface that flash can't
//! satisfy is a remote-desktop client (e.g. Microsoft's Windows App / RDP),
//! which re-establishes its keyboard channel with the remote host *on
//! activation* and needs the app to stay frontmost across the whole interaction
//! — not flashed and restored. `bring_to_front` is that explicit, persistent
//! activation.
//!
//! It activates the owning app by pid via `-[NSRunningApplication
//! activateWithOptions:]` (the same Cocoa call `focus_steal::restore_focus`
//! uses, and the same effect as `open -b <bundle-id>`). `window_id` is accepted
//! for cross-platform parity but activation is app-level: the app's key window
//! comes forward. Unlike the rest of the macOS driver this DOES steal
//! foreground — it is an explicit opt-in, never called by the input ladder.

use async_trait::async_trait;
use cmux_cua_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use serde_json::Value;
use std::time::Duration;

const WINDOW_DISCOVERY_ATTEMPTS: usize = 20;
const WINDOW_DISCOVERY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
enum WindowTargetError {
    NotFound {
        pid: i32,
        requested: Option<u32>,
    },
    OwnerMismatch {
        window_id: u32,
        requested_pid: i32,
        owner_pid: i32,
    },
}

/// Wait for a freshly launched process to publish its first WindowServer
/// record. The closure is injected so the retry policy is unit-testable without
/// touching WindowServer or foreground state.
fn wait_for_window_target<F>(
    pid: i32,
    requested: Option<u32>,
    attempts: usize,
    delay: Duration,
    mut list: F,
) -> Result<Option<u32>, WindowTargetError>
where
    F: FnMut() -> Vec<(u32, i32)>,
{
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        let windows = list();
        if let Some(window_id) = requested {
            if let Some((_, owner_pid)) = windows.iter().find(|(id, _)| *id == window_id) {
                if *owner_pid != pid {
                    return Err(WindowTargetError::OwnerMismatch {
                        window_id,
                        requested_pid: pid,
                        owner_pid: *owner_pid,
                    });
                }
                return Ok(Some(window_id));
            }
        } else if let Some((window_id, _)) = windows.iter().find(|(_, owner_pid)| *owner_pid == pid)
        {
            return Ok(Some(*window_id));
        }
        if attempt + 1 < attempts {
            std::thread::sleep(delay);
        }
    }
    match requested {
        Some(requested) => Err(WindowTargetError::NotFound {
            pid,
            requested: Some(requested),
        }),
        // App-level activation remains valid for processes that have no
        // WindowServer record yet (or are menu-bar/background-only apps).
        None => Ok(None),
    }
}

pub struct BringToFrontTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "bring_to_front".into(),
        description: "Persistently activate an app so it genuinely holds macOS foreground, \
             then leave it there. Most input does NOT need this — every macOS \
             dispatch reaches backgrounded windows, and `dispatch:\"foreground\"` \
             does its own brief front→act→restore. Reach for `bring_to_front` only \
             for a focus-proxy surface that re-arms its own input channel on \
             activation and must stay frontmost across the interaction — chiefly a \
             remote-desktop client (Microsoft Windows App / RDP), where the brief \
             flash drops keystrokes. Activates the owning app by pid (\
             `NSRunningApplication.activate`); `window_id` is accepted for parity \
             but activation is app-level. This DOES steal foreground — explicit \
             opt-in, never used by the input ladder."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid"],
            "properties": {
                "pid": { "type": "integer" },
                "window_id": { "type": "integer" }
            },
            "additionalProperties": false,
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for BringToFrontTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let pid = match args.get("pid").and_then(Value::as_i64) {
            Some(p) => match libc::pid_t::try_from(p) {
                Ok(pid) => pid,
                Err(_) => {
                    return ToolResult::error(format!(
                        "bring_to_front: `pid` {p} is out of range for a process identifier."
                    ))
                    .with_structured(serde_json::json!({
                        "code": "bring_to_front_pid_out_of_range",
                        "pid": p,
                    }));
                }
            },
            None => return ToolResult::error("Missing required integer field: pid".to_string()),
        };
        let requested_window_id = match args.get("window_id").and_then(Value::as_i64) {
            None => None,
            Some(window_id) if window_id > 0 && window_id <= u32::MAX as i64 => {
                Some(window_id as u32)
            }
            Some(window_id) => {
                return ToolResult::error(format!(
                    "bring_to_front: `window_id` {window_id} is not a valid window identifier."
                ))
                .with_structured(serde_json::json!({
                    "code": "bring_to_front_window_id_invalid",
                    "window_id": window_id,
                }));
            }
        };

        // Preserve the original PID-not-found result before waiting on
        // WindowServer: command-line and background-only apps may have no
        // windows, while an exited process should still report the PID error.
        let app_exists =
            unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_some() };
        if !app_exists {
            return ToolResult::error(format!(
                "bring_to_front: no running application for pid {pid} \
                 (process not found or already exited)."
            ))
            .with_structured(serde_json::json!({
                "code": "bring_to_front_pid_not_found",
                "pid": pid,
            }));
        }

        let target_window_id = match tokio::task::spawn_blocking(move || {
            wait_for_window_target(
                pid,
                requested_window_id,
                WINDOW_DISCOVERY_ATTEMPTS,
                WINDOW_DISCOVERY_DELAY,
                || {
                    crate::windows::all_windows()
                        .into_iter()
                        .map(|window| (window.window_id, window.pid))
                        .collect()
                },
            )
        })
        .await
        {
            Ok(Ok(window_id)) => window_id,
            Ok(Err(WindowTargetError::OwnerMismatch {
                window_id,
                requested_pid,
                owner_pid,
            })) => {
                return ToolResult::error(format!(
                    "bring_to_front: window_id={window_id} belongs to pid={owner_pid}, not pid={requested_pid}."
                ))
                .with_structured(serde_json::json!({
                    "code": "bring_to_front_window_owner_mismatch",
                    "window_id": window_id,
                    "pid": requested_pid,
                    "owner_pid": owner_pid,
                }));
            }
            Ok(Err(WindowTargetError::NotFound { pid, requested })) => {
                return ToolResult::error(format!(
                    "bring_to_front: no window for pid {pid} became available before activation."
                ))
                .with_structured(serde_json::json!({
                    "code": "window_target_not_found",
                    "pid": pid,
                    "window_id": requested,
                }));
            }
            Err(join) => {
                return ToolResult::error(format!(
                    "bring_to_front: window discovery task failed: {join}"
                ))
                .with_structured(serde_json::json!({
                    "code": "bring_to_front_window_discovery_failed",
                    "pid": pid,
                }));
            }
        };

        // `-[NSRunningApplication activateWithOptions:]` is documented
        // thread-safe. ActivateAllWindows brings the app's windows forward (not
        // just the key one) so a multi-window target lands fully frontmost.
        // The BOOL return tells us whether Cocoa actually accepted the swap —
        // `None` here means the pid has no running app at all.
        let activation = unsafe {
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid).map(|app| {
                app.activateWithOptions(
                    NSApplicationActivationOptions::NSApplicationActivateAllWindows,
                )
            })
        };

        let activated = match activation {
            None => {
                return ToolResult::error(format!(
                    "bring_to_front: no running application for pid {pid} \
                     (process not found or already exited)."
                ))
                .with_structured(serde_json::json!({
                    "code": "bring_to_front_pid_not_found",
                    "pid": pid,
                }));
            }
            Some(activated) => activated,
        };

        if !activated {
            return ToolResult::error(format!(
                "bring_to_front: macOS rejected activation for pid {pid} \
                 (activateWithOptions returned NO — the app may be hidden or \
                 terminating, or the system denied the foreground swap)."
            ))
            .with_structured(serde_json::json!({
                "code": "bring_to_front_activation_rejected",
                "pid": pid,
                "activated": false,
            }));
        }

        ToolResult::text(format!("Brought pid {pid} to the foreground.")).with_structured(
            serde_json::json!({
                "pid": pid,
                "window_id": target_window_id,
                "activated": true,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn window_discovery_retries_until_fresh_window_registers() {
        let mut observations = VecDeque::from([Vec::new(), Vec::new(), vec![(7001, 321)]]);
        let result = wait_for_window_target(321, None, 5, Duration::ZERO, || {
            observations.pop_front().unwrap_or_default()
        });
        assert_eq!(result, Ok(Some(7001)));
        assert!(observations.is_empty());
    }

    #[test]
    fn window_discovery_times_out_with_structured_target_error() {
        let result =
            wait_for_window_target(321, None, 3, Duration::ZERO, || Vec::<(u32, i32)>::new());
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn explicit_window_discovery_times_out_with_structured_target_error() {
        let result = wait_for_window_target(321, Some(7001), 3, Duration::ZERO, || {
            Vec::<(u32, i32)>::new()
        });
        assert_eq!(
            result,
            Err(WindowTargetError::NotFound {
                pid: 321,
                requested: Some(7001),
            })
        );
    }

    #[test]
    fn explicit_window_owner_mismatch_is_not_retried_as_missing() {
        let mut calls = 0;
        let result = wait_for_window_target(321, Some(7001), 5, Duration::ZERO, || {
            calls += 1;
            vec![(7001, 999)]
        });
        assert_eq!(
            result,
            Err(WindowTargetError::OwnerMismatch {
                window_id: 7001,
                requested_pid: 321,
                owner_pid: 999,
            })
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn explicit_window_id_waits_for_its_owner() {
        let mut observations = VecDeque::from([vec![], vec![(7001, 321)]]);
        let result = wait_for_window_target(321, Some(7001), 3, Duration::ZERO, || {
            observations.pop_front().unwrap_or_default()
        });
        assert_eq!(result, Ok(Some(7001)));
    }
}
