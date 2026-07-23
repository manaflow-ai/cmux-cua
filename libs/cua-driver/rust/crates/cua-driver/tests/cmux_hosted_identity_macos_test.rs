#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn raw_cmux_binary_refuses_to_own_a_daemon() {
    let temporary = tempfile::tempdir().expect("create temporary directory");
    let source = env!("CARGO_BIN_EXE_cua-driver");
    let binary = temporary.path().join("cmux-cua-driver");
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
        .env_remove("CUA_DRIVER_DISCLAIM")
        .env_remove("CUA_DRIVER_RS_RESPONSIBILITY_DISCLAIMED")
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
