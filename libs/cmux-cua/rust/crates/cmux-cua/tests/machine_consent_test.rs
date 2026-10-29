//! Machine-join consent through the real `cmux-cua mcp` stdio server.
//!
//! One server process stays up across the whole test, the way an MCP server
//! an agent connected before the join keeps running. Joining, granting and
//! revoking happen on disk between calls, and each call sees the change.
//!
//! The tools chosen are harmless when they do run (screen geometry, and
//! focusing a pid that does not exist); the assertions only look at whether
//! the call was refused for consent, not at what the tool returned, so the
//! test needs no display.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn spawn(home: &Path, acpmux_home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cmux-cua"))
            .args(["mcp", "--no-daemon-relaunch", "--no-overlay"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("ACPMUX_HOME", acpmux_home)
            .env("CMUX_CUA_TELEMETRY_ENABLED", "false")
            .env("CMUX_CUA_UPDATE_CHECK", "false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmux-cua mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut server = Server {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        server.request("initialize", json!({}));
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        let response: Value = serde_json::from_str(line.trim()).expect("parse response");
        assert_eq!(response["id"], id, "{response}");
        response
    }

    /// The consent surface a call was refused for, or `None` if it ran.
    fn refused_for(&mut self, tool: &str, arguments: Value) -> Option<String> {
        let response = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &response["result"];
        (result["structuredContent"]["code"] == "consent_required").then(|| {
            assert_eq!(result["isError"], true, "{response}");
            result["structuredContent"]["surface"]
                .as_str()
                .unwrap()
                .to_owned()
        })
    }

    fn screen(&mut self) -> Option<String> {
        self.refused_for("get_screen_size", json!({}))
    }

    fn control(&mut self) -> Option<String> {
        // A pid no process has: harmless if it runs.
        self.refused_for("bring_to_front", json!({"pid": 2_147_483_000_i64}))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_private(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn consent(acpmux_home: &Path, screen: &str, control: &str) {
    write_private(
        &acpmux_home.join("consent.json"),
        &format!(r#"{{"screen":"{screen}","control":"{control}"}}"#),
    );
}

#[test]
fn a_running_server_follows_join_grant_and_revoke_on_every_call() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let acpmux_home = root.path().join("acpmux");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&acpmux_home).unwrap();
    std::fs::set_permissions(&acpmux_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::spawn(&home, &acpmux_home);

    // Not joined: unchanged, nothing gated, nothing written.
    assert_eq!(server.screen(), None);
    assert_eq!(server.control(), None);
    assert!(!acpmux_home.join("consent-refusal.json").exists());

    // The machine joins while the server is running: no consent yet, so
    // both surfaces are refused from the next call on.
    write_private(
        &acpmux_home.join("machine-credential.json"),
        r#"{"credential":"cmm_test"}"#,
    );
    assert_eq!(server.screen().as_deref(), Some("screen"));
    assert_eq!(server.control().as_deref(), Some("control"));
    let refusal: Value =
        serde_json::from_slice(&std::fs::read(acpmux_home.join("consent-refusal.json")).unwrap())
            .unwrap();
    assert_eq!(refusal["code"], "consent_required");
    assert_eq!(refusal["surface"], "control");

    // Screen only: reads run, actions are refused.
    consent(&acpmux_home, "granted", "none");
    assert_eq!(server.screen(), None);
    assert_eq!(server.control().as_deref(), Some("control"));

    // Both granted, then control revoked between two calls.
    consent(&acpmux_home, "granted", "granted");
    assert_eq!(server.control(), None);
    consent(&acpmux_home, "granted", "none");
    assert_eq!(server.control().as_deref(), Some("control"));

    // A malformed record grants nothing.
    write_private(&acpmux_home.join("consent.json"), "{");
    assert_eq!(server.screen().as_deref(), Some("screen"));

    // Metadata tools never need consent.
    let response = server.request("tools/call", json!({"name": "get_config", "arguments": {}}));
    assert_ne!(
        response["result"]["structuredContent"]["code"],
        "consent_required"
    );
}
