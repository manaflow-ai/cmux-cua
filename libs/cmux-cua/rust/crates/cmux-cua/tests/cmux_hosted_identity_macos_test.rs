#![cfg(target_os = "macos")]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn raw_cmux_binary_refuses_to_own_a_daemon() {
    let temporary = tempfile::tempdir().expect("create temporary directory");
    let source = env!("CARGO_BIN_EXE_cmux-cua");
    let binary = temporary.path().join("cmux-cua");
    std::fs::copy(source, &binary).expect("copy driver under cmux branded name");
    let mut permissions = std::fs::metadata(&binary)
        .expect("read copied driver metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&binary, permissions).expect("make copied driver executable");

    let socket = temporary.path().join("daemon.sock");
    let mut child = Command::new(&binary)
        .args([
            "--no-overlay",
            "serve",
            "--no-permissions-gate",
            "--socket",
            socket.to_str().expect("UTF-8 socket path"),
        ])
        .env_remove("CMUX_CUA_RESPONSIBILITY_DISCLAIMED")
        .env_remove("CMUX_CUA_RESPONSIBILITY_DISCLAIMED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch copied driver");

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll copied driver") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    if status.is_none() {
        child.kill().expect("stop unsafe raw daemon after failed regression");
    }
    let output = child.wait_with_output().expect("collect copied driver output");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        status.is_some(),
        "raw cmux binary copied from {source} stayed alive as a daemon; stderr: {stderr}"
    );
    assert!(!output.status.success(), "raw cmux binary unexpectedly served");
    assert!(
        stderr.contains("cmux Computer Use"),
        "expected branded recovery guidance, got: {stderr}"
    );
    assert!(
        stderr.contains("Settings"),
        "expected Settings recovery guidance, got: {stderr}"
    );
}

#[test]
fn cmux_proxy_advertises_one_round_trip_action_groups() {
    let temporary = tempfile::tempdir().expect("create temporary directory");
    let socket = temporary.path().join("dormant-daemon.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cmux-cua"))
        .args([
            "mcp",
            "--socket",
            socket.to_str().expect("UTF-8 socket path"),
        ])
        .env("CMUX_CUA_MCP_FORCE_PROXY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch dormant MCP proxy");

    let mut stdin = child.stdin.take().expect("proxy stdin");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "cmux-regression", "version": "1" }
            }
        })
    )
    .expect("write initialize");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        })
    )
    .expect("write tools/list");
    drop(stdin);

    let output = child.wait_with_output().expect("collect proxy output");
    assert!(
        output.status.success(),
        "dormant proxy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tools = String::from_utf8(output.stdout)
        .expect("UTF-8 proxy output")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|response| response.get("id") == Some(&serde_json::json!(2)))
        .and_then(|response| response["result"]["tools"].as_array().cloned())
        .expect("tools/list response");

    assert!(
        tools.iter().any(|tool| tool["name"] == "perform_actions"),
        "cmux proxy must expose a bounded action-group tool so stable controls \
         do not require one model/MCP round trip per click"
    );
}
