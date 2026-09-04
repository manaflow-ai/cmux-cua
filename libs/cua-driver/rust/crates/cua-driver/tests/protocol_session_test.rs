//! Multi-client / multi-cursor / overlay session tests.
//!
//! Concurrent driver processes, per-cursor instance state, cross-process
//! isolation, the live overlay render loop, and the `set_agent_cursor_motion`
//! Bezier knobs. Split out of the old monolithic `mcp_protocol_test.rs`;
//! mac/windows pairs merge and branch only where assertions differ.
//!
//! Note: the originals asserted process liveness with `child.try_wait()`.
//! `RawDriver` owns and kills its child (and exposes only its pid for window
//! attribution), so the liveness check is replaced with an equivalent
//! post-sleep request probe — a crashed process would EOF and panic inside
//! `recv()`.

#![cfg(any(target_os = "macos", target_os = "windows"))]

use cua_driver_testkit::RawDriver;

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn concurrent_clients() {
    //! Verify two concurrent cua-driver-rs processes both respond correctly.
    //! This tests the "multiple cua-driver processes" scenario relevant to
    //! the multi-cursor use case.
    let mut drivers: Vec<RawDriver> = Vec::new();
    for _ in 0..2 {
        let Some(d) = RawDriver::spawn() else { return; };
        drivers.push(d);
    }

    for (i, d) in drivers.iter_mut().enumerate() {
        d.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": "initialize",
            "params": {}
        }));

        let resp = d.recv();
        assert_eq!(resp["id"], (i + 1) as i64);
        assert!(resp["result"]["protocolVersion"].is_string(),
            "Process {i} failed to initialize");
    }
}

#[test]
#[cfg(target_os = "macos")]
fn concurrent_clients_with_cursor_moves() {
    //! Two concurrent cua-driver processes, each driving their own cursor, should not crash.
    //! This covers the multi-cursor use case where two Codex agents run simultaneously.
    let mut drivers: Vec<RawDriver> = Vec::new();
    for _ in 0..2 {
        let Some(d) = RawDriver::spawn() else { return; };
        drivers.push(d);
    }

    // Initialize all processes.
    for (i, d) in drivers.iter_mut().enumerate() {
        d.send(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
        let resp = d.recv();
        assert_eq!(resp["id"], 1, "Process {i} initialize failed");
    }

    // Each process moves its cursor to different positions.
    let positions = vec![(100.0_f64, 200.0_f64), (500.0, 600.0)];
    for ((i, d), (px, py)) in drivers.iter_mut().enumerate().zip(positions.iter()) {
        d.send(&serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"move_cursor","arguments":{"x":px,"y":py}}
        }));
        let resp = d.recv();
        assert!(!resp["result"]["isError"].as_bool().unwrap_or(false),
            "Process {i} move_cursor failed: {resp:?}");
    }

    std::thread::sleep(std::time::Duration::from_millis(100));

    // Each process must still be alive: probe with one more request (a crashed
    // process would EOF and panic in recv()).
    for (i, d) in drivers.iter_mut().enumerate() {
        d.send(&serde_json::json!({
            "jsonrpc":"2.0","id":99,"method":"tools/call",
            "params":{"name":"get_agent_cursor_state","arguments":{}}
        }));
        let resp = d.recv();
        assert_eq!(resp["id"], 99, "cua-driver process {i} crashed during concurrent cursor test");
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn concurrent_multi_driver_isolation() {
    //! Two cua-driver processes running simultaneously with different cursor IDs.
    //! Verifies that concurrent sessions don't interfere — each tracks its own
    //! cursor state and the overlay stays alive under concurrent load.
    let Some(mut child_a) = RawDriver::spawn() else { return; };
    let Some(mut child_b) = RawDriver::spawn() else { return; };

    // Initialize both.
    child_a.send(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    child_b.send(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    child_a.recv();
    child_b.recv();

    // Driver A: set_agent_cursor_enabled for cursor "alpha", Driver B for cursor "beta".
    child_a.send(&serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"set_agent_cursor_enabled","arguments":{"cursor_id":"alpha","enabled":true}}
    }));
    child_b.send(&serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"set_agent_cursor_enabled","arguments":{"cursor_id":"beta","enabled":false}}
    }));
    let resp_a = child_a.recv();
    let resp_b = child_b.recv();
    assert!(resp_a["error"].is_null(), "Driver A set_agent_cursor_enabled failed: {resp_a:?}");
    assert!(resp_b["error"].is_null(), "Driver B set_agent_cursor_enabled failed: {resp_b:?}");

    // Each driver queries its own cursor state — should reflect what it set.
    // get_agent_cursor_state is session-scoped, so pass the cursor_id each
    // driver owns; the response carries { "cursors": [<that one>], "enabled" }.
    child_a.send(&serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"get_agent_cursor_state","arguments":{"cursor_id":"alpha"}}
    }));
    child_b.send(&serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"get_agent_cursor_state","arguments":{"cursor_id":"beta"}}
    }));
    let state_a = child_a.recv();
    let state_b = child_b.recv();
    assert!(state_a["error"].is_null(), "Driver A get_agent_cursor_state failed: {state_a:?}");
    assert!(state_b["error"].is_null(), "Driver B get_agent_cursor_state failed: {state_b:?}");

    // cursors is an array of { config: { cursor_id, enabled, ... }, x, y }.
    // Cursor alpha should be enabled (we set true), beta disabled (we set false).
    // The state lives per process — processes don't share CursorRegistry.
    let cursors_a = state_a["result"]["structuredContent"]["cursors"].as_array();
    let cursors_b = state_b["result"]["structuredContent"]["cursors"].as_array();

    let alpha_enabled = cursors_a.and_then(|arr| arr.iter().find(|c| {
        c["config"]["cursor_id"].as_str() == Some("alpha")
    })).and_then(|c| c["config"]["enabled"].as_bool());
    let beta_enabled = cursors_b.and_then(|arr| arr.iter().find(|c| {
        c["config"]["cursor_id"].as_str() == Some("beta")
    })).and_then(|c| c["config"]["enabled"].as_bool());

    assert_eq!(alpha_enabled, Some(true),  "Cursor alpha should be enabled in driver A");
    assert_eq!(beta_enabled,  Some(false), "Cursor beta should be disabled in driver B");

    // macOS additionally exercises move_cursor on both drivers to confirm
    // neither crashes the other (the Windows variant stops after the state check).
    if !cfg!(target_os = "windows") {
        child_a.send(&serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"move_cursor","arguments":{"cursor_id":"alpha","x":100,"y":100}}
        }));
        child_b.send(&serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"move_cursor","arguments":{"cursor_id":"beta","x":200,"y":200}}
        }));
        let mv_a = child_a.recv();
        let mv_b = child_b.recv();
        assert!(mv_a["error"].is_null(), "Driver A move_cursor failed: {mv_a:?}");
        assert!(mv_b["error"].is_null(), "Driver B move_cursor failed: {mv_b:?}");
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn multi_cursor_instance_state() {
    //! Two cursor instances can be created with different IDs; each has independent state.
    let Some(mut d) = RawDriver::spawn() else { return; };

    d.send(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    d.recv();

    // Move cursor "agent1" to (100, 200).
    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"move_cursor","arguments":{"x":100.0,"y":200.0,"cursor_id":"agent1"}}
    }));
    let r2 = d.recv();
    assert!(!r2["result"]["isError"].as_bool().unwrap_or(false));

    // Move cursor "agent2" to (300, 400).
    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"move_cursor","arguments":{"x":300.0,"y":400.0,"cursor_id":"agent2"}}
    }));
    let r3 = d.recv();
    assert!(!r3["result"]["isError"].as_bool().unwrap_or(false));

    // Configure cursor "agent1" with a custom color.
    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":4,"method":"tools/call",
        "params":{"name":"set_agent_cursor_motion","arguments":{
            "cursor_id":"agent1","cursor_color":"#FF0000","cursor_label":"AI-1"
        }}
    }));
    let r4 = d.recv();
    assert!(!r4["result"]["isError"].as_bool().unwrap_or(false));

    // Hide cursor "agent2".
    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":5,"method":"tools/call",
        "params":{"name":"set_agent_cursor_enabled","arguments":{"enabled":false,"cursor_id":"agent2"}}
    }));
    let r5 = d.recv();
    assert!(!r5["result"]["isError"].as_bool().unwrap_or(false));

    // get_agent_cursor_state reports independent per-cursor state. macOS scopes
    // the response to the resolved cursor_id (returns ONLY that cursor); Windows
    // returns ALL cursors in one list. Either way, agent1 must read back enabled
    // and agent2 disabled.
    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":6,"method":"tools/call",
        "params":{"name":"get_agent_cursor_state","arguments":{"cursor_id":"agent1"}}
    }));
    let resp1 = d.recv();
    assert!(!resp1["result"]["isError"].as_bool().unwrap_or(false));
    let cursors1 = resp1["result"]["structuredContent"]["cursors"].as_array().cloned().unwrap_or_default();
    if cfg!(target_os = "windows") {
        // Windows returns the full cursor list; find agent1 within it.
        let a1 = cursors1.iter()
            .find(|c| c["config"]["cursor_id"].as_str() == Some("agent1"))
            .expect("agent1 in returned cursor list");
        assert_eq!(a1["config"]["enabled"].as_bool(), Some(true), "agent1 was never disabled");
    } else {
        assert_eq!(cursors1.len(), 1, "agent1 query must return exactly its own cursor, got: {cursors1:?}");
        assert_eq!(cursors1[0]["config"]["cursor_id"].as_str(), Some("agent1"));
        assert_eq!(resp1["result"]["structuredContent"]["enabled"].as_bool(), Some(true),
            "agent1 was never disabled");
    }

    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":7,"method":"tools/call",
        "params":{"name":"get_agent_cursor_state","arguments":{"cursor_id":"agent2"}}
    }));
    let resp2 = d.recv();
    assert!(!resp2["result"]["isError"].as_bool().unwrap_or(false));
    let cursors2 = resp2["result"]["structuredContent"]["cursors"].as_array().cloned().unwrap_or_default();
    if cfg!(target_os = "windows") {
        // Windows returns the full cursor list; find agent2 within it.
        let a2 = cursors2.iter()
            .find(|c| c["config"]["cursor_id"].as_str() == Some("agent2"))
            .expect("agent2 in returned cursor list");
        assert_eq!(a2["config"]["enabled"].as_bool(), Some(false), "agent2 was hidden");
    } else {
        assert_eq!(cursors2.len(), 1, "agent2 query must return exactly its own cursor, got: {cursors2:?}");
        assert_eq!(cursors2[0]["config"]["cursor_id"].as_str(), Some("agent2"));
        assert_eq!(resp2["result"]["structuredContent"]["enabled"].as_bool(), Some(false),
            "agent2 was hidden");
    }

    std::thread::sleep(std::time::Duration::from_millis(50));
    // Process must still be alive: probe with one more request (a crashed
    // process would EOF and panic in recv()).
    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":99,"method":"tools/call",
        "params":{"name":"get_agent_cursor_state","arguments":{"cursor_id":"agent1"}}
    }));
    let resp = d.recv();
    assert_eq!(resp["id"], 99, "cua-driver crashed during multi-cursor test");
}

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn overlay_move_cursor_stays_alive() {
    //! Verify that calling move_cursor with the overlay enabled does not crash the process.
    //! The overlay renders to a transparent NSWindow on the main thread; the MCP server
    //! sends commands via the global channel.  Any crash in the render thread (e.g. wrong
    //! GCD queue pointer, bad CGImage argument types) would cause this test to fail with
    //! an early EOF or a non-zero exit code.
    let Some(mut d) = RawDriver::spawn() else { return; };

    d.send(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    d.recv();

    // Call move_cursor several times to drive the overlay render loop.
    for (id, (x, y)) in [(2u64, (100.0_f64, 200.0_f64)), (3, (400.0, 300.0)), (4, (800.0, 600.0))] {
        d.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "move_cursor", "arguments": { "x": x, "y": y } }
        }));
        let resp = d.recv();
        assert_eq!(resp["id"], id);
        // Should succeed (isError absent or false).
        let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
        assert!(!is_error, "move_cursor failed: {:?}", resp);
    }

    // Also call get_agent_cursor_state — should report the last position.
    d.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": { "name": "get_agent_cursor_state", "arguments": {} }
    }));
    let resp = d.recv();
    assert_eq!(resp["id"], 5);
    assert!(!resp["result"]["isError"].as_bool().unwrap_or(false));

    // Give the render loop a chance to tick (two frames ≈ 33 ms).
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Process must still be alive: probe with one more request (a crashed
    // process would EOF and panic in recv()).
    d.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "tools/call",
        "params": { "name": "get_agent_cursor_state", "arguments": {} }
    }));
    let resp = d.recv();
    assert_eq!(resp["id"], 99, "cua-driver crashed during overlay move_cursor test");
}

#[test]
#[cfg(target_os = "macos")]
fn set_agent_cursor_motion_bezier_knobs() {
    //! set_agent_cursor_motion with Bezier/timing knobs — verifies schema accepts them
    //! and returns a non-error response with the updated values in the response text.
    let Some(mut d) = RawDriver::spawn() else { return; };

    d.send(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    d.recv();

    // Set Bezier motion knobs including integer-encoded numbers (common from MCP clients).
    d.send(&serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"set_agent_cursor_motion","arguments":{
            "cursor_id": "agent1",
            "arc_size": 0.4,
            "spring": 0.85,
            "glide_duration_ms": 500,
            "dwell_after_click_ms": 200,
            "idle_hide_ms": 5000
        }}
    }));
    let resp = d.recv();
    assert!(resp["error"].is_null(), "Protocol error from set_agent_cursor_motion: {resp:?}");
    assert!(!resp["result"]["isError"].as_bool().unwrap_or(false),
        "set_agent_cursor_motion returned isError: {resp:?}");
    // Response text should mention the new glide duration.
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("500") || text.contains("motion"),
        "Expected motion summary in response, got: {text}");
}

/// Exercise the real MCP → AppKit → WindowServer path on a negative-origin
/// display. This is the behavior-level guard for the global Quartz coordinate
/// contract: when the key screen is external, reapplying its origin moves the
/// panel an entire monitor away. It also checks the no-input contract and that
/// an explicit session end removes the panel from WindowServer.
#[test]
#[cfg(target_os = "macos")]
#[ignore]
fn external_display_cursor_preserves_global_protocol_coordinates() {
    let Some(mut driver) = RawDriver::spawn() else { return; };

    driver.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    }));
    let initialized = driver.recv();
    assert_eq!(initialized["id"], 1);

    // Discover a live negative-origin display through the public protocol. A
    // single-display runner has no meaningful external-display assertion, so
    // leave it as an explicit environment skip rather than inventing pixels.
    driver.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "list_windows",
            "arguments": { "on_screen_only": true }
        }
    }));
    let listed = driver.recv();
    assert_eq!(listed["id"], 2);
    assert!(listed["error"].is_null());
    assert_ne!(listed["result"]["isError"].as_bool(), Some(true));
    let windows = listed["result"]["structuredContent"]["windows"]
        .as_array()
        .expect("list_windows protocol payload");
    let target = windows
        .iter()
        .filter(|window| window["pid"].as_u64() != Some(driver.pid() as u64))
        .filter(|window| window["is_on_screen"].as_bool() == Some(true))
        .filter_map(|window| {
            let bounds = window["bounds"].as_object()?;
            let x = bounds.get("x")?.as_f64()?;
            let y = bounds.get("y")?.as_f64()?;
            let width = bounds.get("width")?.as_f64()?;
            let height = bounds.get("height")?.as_f64()?;
            let window_id = window["window_id"].as_u64()?;
            (x < -50.0 && width > 100.0 && height > 100.0)
                .then_some((window_id, x, y, width, height))
        })
        .next();
    let Some((target_window_id, window_x, window_y, window_width, window_height)) = target else {
        eprintln!(
            "[macos-cursor] no visible negative-origin window; skipping external-display probe"
        );
        return;
    };
    let requested = (
        window_x + window_width / 2.0,
        window_y + window_height / 2.0,
    );

    driver.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": "get_cursor_position", "arguments": {} }
    }));
    let before_cursor = driver.recv();
    assert_eq!(before_cursor["id"], 3);
    assert!(before_cursor["error"].is_null());
    assert_ne!(before_cursor["result"]["isError"].as_bool(), Some(true));
    let before_x = before_cursor["result"]["structuredContent"]["x"]
        .as_f64()
        .expect("pre-action cursor x");
    let before_y = before_cursor["result"]["structuredContent"]["y"]
        .as_f64()
        .expect("pre-action cursor y");

    driver.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "move_cursor",
            "arguments": {
                "session": "external-display-probe",
                "x": requested.0,
                "y": requested.1
            }
        }
    }));
    let moved = driver.recv();
    assert_eq!(moved["id"], 4);
    assert!(moved["error"].is_null());
    assert_ne!(
        moved["result"]["isError"].as_bool(),
        Some(true),
        "move_cursor failed: {moved:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(150));

    driver.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "list_windows",
            "arguments": { "on_screen_only": true }
        }
    }));
    let after_move = driver.recv();
    assert_eq!(after_move["id"], 5);
    assert!(after_move["error"].is_null());
    assert_ne!(after_move["result"]["isError"].as_bool(), Some(true));
    let overlay = after_move["result"]["structuredContent"]["windows"]
        .as_array()
        .and_then(|entries| {
            entries.iter().find(|window| {
                window["pid"].as_u64() == Some(driver.pid() as u64)
                    && window["is_on_screen"].as_bool() == Some(true)
                    && window["bounds"]["width"].as_f64().unwrap_or(0.0) >= 100.0
                    && window["bounds"]["height"].as_f64().unwrap_or(0.0) >= 100.0
            })
        })
        .expect("the source-built cursor panel must be visible on the external display");
    let panel_x = overlay["bounds"]["x"].as_f64().expect("panel x");
    let panel_y = overlay["bounds"]["y"].as_f64().expect("panel y");
    let panel_width = overlay["bounds"]["width"].as_f64().expect("panel width");
    let panel_height = overlay["bounds"]["height"].as_f64().expect("panel height");
    let panel_center = (panel_x + panel_width / 2.0, panel_y + panel_height / 2.0);
    assert!(
        (panel_center.0 - requested.0).abs() <= 24.0
            && (panel_center.1 - requested.1).abs() <= 24.0,
        "cursor panel lost global coordinates: requested={requested:?}, panel={panel_center:?}, overlay={overlay:?}"
    );
    let target_after = after_move["result"]["structuredContent"]["windows"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|window| window["window_id"].as_u64() == Some(target_window_id))
        })
        .expect("the discovered external target must remain in the current-space listing");
    let overlay_z = overlay["z_index"].as_u64().expect("overlay z-index");
    let target_z = target_after["z_index"].as_u64().expect("target z-index");
    assert!(
        overlay_z > target_z,
        "cursor panel was not above the current-space target: overlay_z={overlay_z}, target_z={target_z}, overlay={overlay:?}, target={target_after:?}"
    );

    driver.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": { "name": "get_cursor_position", "arguments": {} }
    }));
    let after_cursor = driver.recv();
    assert_eq!(after_cursor["id"], 6);
    assert!(after_cursor["error"].is_null());
    assert_ne!(after_cursor["result"]["isError"].as_bool(), Some(true));
    let after_x = after_cursor["result"]["structuredContent"]["x"]
        .as_f64()
        .expect("post-action cursor x");
    let after_y = after_cursor["result"]["structuredContent"]["y"]
        .as_f64()
        .expect("post-action cursor y");
    assert!(
        (after_x - before_x).abs() <= 1.0 && (after_y - before_y).abs() <= 1.0,
        "move_cursor moved the user's real pointer: before=({before_x},{before_y}) after=({after_x},{after_y})"
    );

    driver.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "end_session",
            "arguments": { "session": "external-display-probe" }
        }
    }));
    let ended = driver.recv();
    assert_eq!(ended["id"], 7);
    assert!(ended["error"].is_null());
    assert_ne!(
        ended["result"]["isError"].as_bool(),
        Some(true),
        "end_session failed: {ended:?}"
    );

    // Closing the NSPanel is dispatched asynchronously to AppKit's main
    // queue. Poll the public WindowServer listing for a bounded interval
    // instead of asserting at an arbitrary frame boundary.
    let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut request_id = 8;
    loop {
        driver.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {
                "name": "list_windows",
                "arguments": { "on_screen_only": true }
            }
        }));
        let after_end = driver.recv();
        assert_eq!(after_end["id"], request_id);
        assert!(after_end["error"].is_null());
        assert_ne!(after_end["result"]["isError"].as_bool(), Some(true));
        let entries = after_end["result"]["structuredContent"]["windows"]
            .as_array()
            .expect("post-end list_windows protocol payload");
        let leaked_panel = entries.iter().any(|window| {
            window["pid"].as_u64() == Some(driver.pid() as u64)
                && window["is_on_screen"].as_bool() == Some(true)
                && window["bounds"]["width"].as_f64().unwrap_or(0.0) >= 100.0
                && window["bounds"]["height"].as_f64().unwrap_or(0.0) >= 100.0
        });
        if !leaked_panel {
            break;
        }
        assert!(
            std::time::Instant::now() < cleanup_deadline,
            "ended session left a cursor panel in WindowServer"
        );
        request_id += 1;
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
