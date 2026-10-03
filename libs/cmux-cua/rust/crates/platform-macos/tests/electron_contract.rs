//! Regression coverage at the public dispatch boundary for Electron recovery.
#![cfg(target_os = "macos")]

use cmux_cua_core::tool::{validate_dispatch_args, ToolRegistry};
use serde_json::json;

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    platform_macos::tools::register_all(&mut registry, false);
    registry
}

#[test]
fn click_accepts_explicit_foreground_fallback() {
    let registry = registry();
    let def = registry.get_def("click").unwrap();
    assert!(validate_dispatch_args(def, &json!({
        "pid": 42, "window_id": 7, "x": 20, "y": 30,
        "fallback": "foreground"
    })).is_ok(), "Electron recovery must be reachable through normal tool dispatch");
    assert!(validate_dispatch_args(def, &json!({
        "pid": 42, "window_id": 7, "x": 20, "y": 30,
        "fallback": "automatic"
    })).is_err(), "foreground fallback requires an explicit supported opt-in");
}

#[test]
fn state_accepts_screenshot_without_accessibility() {
    let registry = registry();
    assert!(validate_dispatch_args(registry.get_def("get_window_state").unwrap(), &json!({
        "pid": 42, "window_id": 7, "include_accessibility": false
    })).is_ok(), "Screenshot-only state must not be rejected before the AX-free path");
}

#[test]
fn state_accepts_a_bounded_accessibility_time_budget() {
    let registry = registry();
    assert!(validate_dispatch_args(registry.get_def("get_window_state").unwrap(), &json!({
        "pid": 42, "window_id": 7, "max_ax_time_ms": 50
    })).is_ok(), "Agents must be able to bound a slow Electron AX walk");
}
