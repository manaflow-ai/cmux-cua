//! Stdio MCP proxy that forwards `tools/list` and `tools/call` through
//! the running `cua-driver-rs serve` daemon over its Unix socket.
//!
//! This is the runtime half of the TCC auto-relaunch path (issue #1525,
//! mirror of Swift PR #1479). When `cua-driver-rs mcp` is invoked from
//! an IDE terminal — Claude Code, Cursor, VS Code, Warp — macOS TCC
//! attributes the process to the calling terminal, not to
//! `CuaDriver.app`. The MCP client side sees a normal stdio server,
//! but every AX probe silently fails because the binary is running
//! against the wrong bundle id.
//!
//! The fix: detect that context (see `crate::bundle`), ensure a daemon
//! is running under `LaunchServices` (which gives it the right TCC
//! attribution), then proxy every MCP request through the daemon's
//! socket. The MCP client never sees the redirection — same JSON-RPC
//! envelope, same tool semantics.
//!
//! Why this lives in `cua-driver` and not `mcp-server`:
//!   `cua_driver_core::server::run` already speaks JSON-RPC over stdio
//!   against an in-process `ToolRegistry`. The proxy speaks the same
//!   protocol on the client side but the server side is the daemon's
//!   line-delimited JSON UDS protocol, owned by `crate::serve`.
//!   Putting the proxy here avoids `mcp-server → cua-driver` reverse
//!   coupling.

use std::sync::Arc;

use cua_driver_core::protocol::{
    codex_computer_use_initialize_result, initialize_result, Request, Response, ToolCall,
    ToolResult,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, error, warn};

use crate::serve::{
    is_daemon_listening, send_request, serialize_request, DaemonProfile, DaemonRequest,
    DaemonResponse,
    CODEX_COMPUTER_USE_TOOL_NAMES,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum ControlConnectionState {
    Connecting,
    Ready {
        approval_broker_token: Option<String>,
    },
    Rejected(String),
}

/// Run the MCP stdio proxy. Reads JSON-RPC lines from stdin, forwards
/// the body of each `tools/list` / `tools/call` to the daemon at
/// `socket_path`, and writes the daemon's response back as a proper
/// JSON-RPC envelope.
///
/// Mirrors `cua_driver_core::server::run`'s control flow exactly — same
/// EOF + parse-error + notification handling — only the per-method
/// branches change.
///
/// Fails fast if the daemon isn't reachable, so MCP clients see a
/// clear startup error instead of a "successful" handshake that
/// advertises zero tools and then errors on every call. Matches
/// Swift `makeProxy`'s `fetchProxyToolList` pre-check.
pub async fn run_proxy(
    socket_path: String,
    claude_code_compat: bool,
    expected_profile: DaemonProfile,
) -> anyhow::Result<()> {
    // Mint this MCP session's identity once at proxy startup. One proxy process
    // == one MCP session; the daemon outlives it. We stamp this id on every
    // forwarded request so the daemon can OWN and CLEAN UP this session's
    // state (recording, config overrides) and tear it down on disconnect via
    // a `session_end` signal. Dep-free `pid + start-nanos` is sufficient for
    // daemon-local uniqueness over this proxy's lifetime (no `uuid` crate dep
    // for one mint).
    let session_id = mint_session_id();
    debug!(session_id = %session_id, "proxy session minted");

    // Serve `initialize` and `tools/list` WITHOUT the daemon so the
    // permission-requesting daemon stays DORMANT until the agent actually
    // invokes a tool.
    //
    // The "control" connection referenced below is the reaper: it holds one
    // long-lived socket to the daemon and sends a single `session_begin`; when
    // the proxy exits (graceful stdin EOF) OR is SIGKILLed/crashes, the kernel
    // closes it and the daemon fires `session_end` for this `session_id`,
    // tearing down every piece of state this session owns (overlay cursor,
    // config overrides, recording).
    //
    let (control_ready_tx, control_ready_rx) =
        tokio::sync::watch::channel(ControlConnectionState::Connecting);
    // macOS tracks daemon/reaper ownership separately from the permission
    // onboarding milestone. A read-only permission probe may start the daemon
    // promptly without allowing a later driving action to skip the grant wait.
    #[cfg(target_os = "macos")]
    let mut daemon_state = DaemonStartState::default();
    #[cfg(not(target_os = "macos"))]
    {
        if !is_daemon_listening(&socket_path) {
            anyhow::bail!(
                "cua-driver-rs daemon not reachable on {socket_path}. Start it \
                 with `cua-driver serve` and retry."
            );
        }
        let socket = socket_path.clone();
        let sid = session_id.clone();
        let ready = control_ready_tx.clone();
        tokio::spawn(async move {
            run_control_connection(socket, sid, expected_profile, ready, None).await;
        });
    }

    // macOS: build the tool list from the in-process registry — a pure,
    // permission-free operation — and launch the daemon (plus the reaper) lazily
    // on the FIRST `tools/call` (see `ensure_daemon_started`). Nothing prompts
    // merely because an agent registered this MCP server at session start.
    //
    // Other platforms: there is no lazy `open -a` launch path, so the caller
    // guarantees a daemon is already up. Fetch the list from it and start the
    // reaper immediately (unchanged behaviour).
    #[cfg(target_os = "macos")]
    let bootstrap_tools_list = {
        let registry = crate::build_macos_registry_with_compat(
            claude_code_compat,
            expected_profile == DaemonProfile::CodexComputerUseCompat,
        );
        Arc::new(if expected_profile == DaemonProfile::CodexComputerUseCompat {
            registry.codex_computer_use_tools_list()
        } else {
            registry.tools_list()
        })
    };
    #[cfg(not(target_os = "macos"))]
    let cached_tools_list = {
        let _ = claude_code_compat;
        let _ = wait_for_control_connection(control_ready_rx.clone()).await?;
        Arc::new(fetch_tools_list_from_daemon(
            &socket_path,
            &session_id,
            expected_profile,
        )?)
    };

    #[cfg(target_os = "macos")]
    let (daemon_lifecycle_tx, mut daemon_lifecycle_rx) =
        tokio::sync::mpsc::unbounded_channel::<DaemonLifecycleEvent>();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut writer = tokio::io::BufWriter::new(stdout);
    let mut client_supports_elicitation = false;

    loop {
        // The persistent control connection is also the authoritative daemon
        // lifetime signal. A helper can exit and finish relaunching between
        // two MCP calls, leaving the same socket path reachable; a transient
        // `is_daemon_listening` probe cannot identify that replacement. Its
        // EOF event invalidates the generation/cache before the next request.
        // `next_line` is cancellation-safe, so selecting it against lifecycle
        // events cannot discard a partially received JSON-RPC line.
        #[cfg(target_os = "macos")]
        let line = tokio::select! {
            biased;
            event = daemon_lifecycle_rx.recv() => {
                if let Some(event) = event {
                    daemon_state.observe_control_connection_end(event.generation);
                }
                continue;
            }
            line = lines.next_line() => line?,
        };
        #[cfg(not(target_os = "macos"))]
        let line = lines.next_line().await?;
        let Some(line) = line else {
            break; // EOF — MCP client disconnected (stdin closed).
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        debug!(raw = trimmed, "→ proxy request");

        let response = match serde_json::from_str::<Request>(trimmed) {
            Err(e) => {
                error!("JSON parse error: {e}");
                Response::parse_error()
            }
            Ok(req) if req.is_notification() => {
                // Notifications get dropped, same as `server::run`.
                continue;
            }
            Ok(req) => {
                let id = req.id.clone().unwrap_or(serde_json::Value::Null);
                if req.method == "initialize" {
                    client_supports_elicitation = supports_elicitation(&req);
                }

                // macOS: bring the daemon (and the reaper control connection) up
                // only after a tools/call passes permission-free protocol,
                // roster, and schema admission. initialize/tools/list remain
                // local so registering the server cannot trigger TCC prompts.
                #[cfg(target_os = "macos")]
                let platform_response = if req.method == "tools/call" {
                    let tools_list = daemon_state.tools_list_or(&bootstrap_tools_list);
                    match admit_bootstrap_tool_call(&req, tools_list) {
                        BootstrapToolCallAdmission::InvalidParams(error) => {
                            Response::error(id, -32602, format!("Invalid params: {error}"))
                        }
                        BootstrapToolCallAdmission::Rejected(result) => {
                            response_from_tool_result(id, result)
                        }
                        BootstrapToolCallAdmission::Ready {
                            call,
                            wait_for_grants,
                        } => {
                            match ensure_daemon_started(
                                &socket_path,
                                &mut daemon_state,
                                &session_id,
                                claude_code_compat,
                                expected_profile,
                                &control_ready_tx,
                                wait_for_grants,
                                &daemon_lifecycle_tx,
                            )
                            .await
                            {
                                Err(error) => Response::error(
                                    id,
                                    -32603,
                                    format!("computer-use daemon failed to start: {error}"),
                                ),
                                Ok(()) if expected_profile
                                    == DaemonProfile::CodexComputerUseCompat =>
                                {
                                    forward_tool_call_with_approval(
                                        id,
                                        call.name,
                                        call.args,
                                        &socket_path,
                                        &session_id,
                                        &control_ready_rx,
                                        client_supports_elicitation,
                                        &mut lines,
                                        &mut writer,
                                    )
                                    .await
                                }
                                Ok(()) => {
                                    forward_tool_call(
                                        id,
                                        call.name,
                                        call.args,
                                        &socket_path,
                                        &session_id,
                                        &control_ready_rx,
                                    )
                                    .await
                                }
                            }
                        }
                    }
                } else {
                    let tools_list = daemon_state.tools_list_or(&bootstrap_tools_list);
                    handle_proxy_request(
                        req,
                        id,
                        &socket_path,
                        tools_list,
                        &session_id,
                        &control_ready_rx,
                        expected_profile,
                    )
                    .await
                };

                #[cfg(not(target_os = "macos"))]
                let platform_response =
                    if req.method == "tools/call"
                        && expected_profile == DaemonProfile::CodexComputerUseCompat
                    {
                        match req.tool_call() {
                            Err(error) => Response::error(
                                id,
                                -32602,
                                format!("Invalid params: {error}"),
                            ),
                            Ok(call) => {
                                forward_tool_call_with_approval(
                                    id,
                                    call.name,
                                    call.args,
                                    &socket_path,
                                    &session_id,
                                    &control_ready_rx,
                                    client_supports_elicitation,
                                    &mut lines,
                                    &mut writer,
                                )
                                .await
                            }
                        }
                    } else {
                        handle_proxy_request(
                            req,
                            id,
                            &socket_path,
                            &cached_tools_list,
                            &session_id,
                            &control_ready_rx,
                            expected_profile,
                        )
                        .await
                    };

                platform_response
            }
        };

        let serialized = serde_json::to_string(&response).unwrap_or_else(|e| {
            format!(
                r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize error: {e}"}}}}"#
            )
        });
        debug!(raw = %serialized, "← proxy response");

        writer.write_all(serialized.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    // Reached on a clean stdin EOF (the `n == 0` break above) — the normal
    // "MCP client disconnected" seam. Session teardown is NO LONGER done here:
    // it's fully subsumed by the persistent control connection spawned at
    // startup. On any proxy exit — graceful stdin EOF (this path), an I/O
    // error propagated via `?`, OR a SIGKILL/crash — the kernel closes the
    // control socket, the daemon's reader hits EOF, and it fires
    // `session_end(session_id)` once (idempotent). That single path reliably
    // covers the ungraceful-death case the old best-effort exit hook missed.
    Ok(())
}

/// Own the proxy's single long-lived control connection. Connects directly to
/// the daemon socket (its OWN async open — `send_request` is sync, blocking,
/// and one-shot, so it cannot be reused here), sends one `session_begin` line
/// carrying `session_id`, then parks in a read loop until the connection
/// closes. It never writes again. The daemon records `session_id` from
/// `session_begin` and fires `session_end` when this connection EOFs — which
/// the kernel triggers on proxy exit AND on kill -9.
///
/// When the daemon closes the connection during a permission-triggered re-exec,
/// reconnect with the same session identity and send `session_begin` again.
/// The old daemon reaps the old connection before it exits; the replacement
/// daemon then owns cleanup for the reconnected session. The task ends only
/// when the proxy runtime drops it, which also closes the current connection
/// and triggers the replacement daemon's EOF cleanup.
async fn run_control_connection(
    socket_path: String,
    session_id: String,
    expected_profile: DaemonProfile,
    readiness: tokio::sync::watch::Sender<ControlConnectionState>,
    mut lifecycle: Option<(
        tokio::sync::mpsc::UnboundedSender<DaemonLifecycleEvent>,
        u64,
    )>,
) {
    let begin = DaemonRequest {
        method: "session_begin".into(),
        name: None,
        args: (expected_profile == DaemonProfile::CodexComputerUseCompat)
            .then(|| serde_json::json!({"approval_broker": true})),
        session_id: Some(session_id.clone()),
    };
    let line = match serialize_request(&begin) {
        Ok(s) => s + "\n",
        Err(e) => {
            warn!("control connection: serialize session_begin failed: {e}");
            return;
        }
    };

    #[cfg(unix)]
    {
        loop {
            let connected = match run_unix_control_connection_once(
                &socket_path,
                &session_id,
                &line,
                expected_profile,
                &readiness,
            )
            .await
            {
                Ok(connected) => connected,
                Err(error) => {
                    let _ = readiness.send(ControlConnectionState::Rejected(error));
                    return;
                }
            };
            if connected {
                if let Some((sender, generation)) = lifecycle.as_mut() {
                    let _ = sender.send(DaemonLifecycleEvent {
                        generation: *generation,
                    });
                    *generation = generation.wrapping_add(1).max(1);
                }
            }
            debug!(
                session_id = %session_id,
                connected,
                "control connection unavailable; waiting to reconnect"
            );
            tokio::time::sleep(std::time::Duration::from_millis(
                if connected { 50 } else { 250 },
            ))
            .await;
        }
    }

    #[cfg(all(not(unix), target_os = "windows"))]
    {
        loop {
            let connected = match run_windows_control_connection_once(
                &socket_path,
                &session_id,
                &line,
                expected_profile,
                &readiness,
            )
            .await
            {
                Ok(connected) => connected,
                Err(error) => {
                    let _ = readiness.send(ControlConnectionState::Rejected(error));
                    return;
                }
            };
            if connected {
                if let Some((sender, generation)) = lifecycle.as_mut() {
                    let _ = sender.send(DaemonLifecycleEvent {
                        generation: *generation,
                    });
                    *generation = generation.wrapping_add(1).max(1);
                }
            }
            debug!(
                session_id = %session_id,
                connected,
                "control connection unavailable; waiting to reconnect"
            );
            tokio::time::sleep(std::time::Duration::from_millis(
                if connected { 50 } else { 250 },
            ))
            .await;
        }
    }

    #[cfg(all(not(unix), not(target_os = "windows")))]
    {
        let _ = (
            line,
            session_id,
            socket_path,
            expected_profile,
            readiness,
            lifecycle,
        );
    }
}

async fn wait_for_control_connection(
    mut readiness: tokio::sync::watch::Receiver<ControlConnectionState>,
) -> anyhow::Result<Option<String>> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        loop {
            match readiness.borrow().clone() {
                ControlConnectionState::Ready {
                    approval_broker_token,
                } => return Ok(approval_broker_token),
                ControlConnectionState::Rejected(error) => anyhow::bail!(error),
                ControlConnectionState::Connecting => {}
            }
            readiness.changed().await.map_err(|_| {
                anyhow::anyhow!("daemon control-connection task stopped unexpectedly")
            })?;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out waiting for the daemon session control connection"
        )
    })?
}

fn validate_control_ack(
    line: &str,
    expected_profile: DaemonProfile,
) -> anyhow::Result<Option<String>> {
    let response: DaemonResponse = serde_json::from_str(line)
        .map_err(|error| anyhow::anyhow!("decode session_begin response: {error}"))?;
    if !response.ok {
        anyhow::bail!(
            "daemon rejected session_begin: {}",
            response
                .error
                .unwrap_or_else(|| "unknown daemon error".to_owned())
        );
    }
    let reported_profile = response
        .result
        .as_ref()
        .and_then(|result| result.get("profile"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("session_begin ACK did not report a daemon profile"))?;
    if reported_profile != expected_profile.as_str() {
        anyhow::bail!(
            "daemon profile mismatch: MCP requested `{expected_profile}`, but the daemon \
             reports `{reported_profile}`"
        );
    }
    let approval_broker_token = response
        .result
        .as_ref()
        .and_then(|result| result.get("approval_broker_token"))
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    if expected_profile == DaemonProfile::CodexComputerUseCompat
        && approval_broker_token.is_none()
    {
        anyhow::bail!(
            "session_begin ACK did not include an authenticated app approval broker token"
        );
    }
    Ok(approval_broker_token)
}

#[cfg(unix)]
async fn run_unix_control_connection_once(
    socket_path: &str,
    session_id: &str,
    begin_line: &str,
    expected_profile: DaemonProfile,
    readiness: &tokio::sync::watch::Sender<ControlConnectionState>,
) -> Result<bool, String> {
    use tokio::net::UnixStream;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut stream = loop {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => break stream,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                debug!(session_id, "control connect failed while daemon restarts: {error}");
                return Ok(false);
            }
        }
    };
    if let Err(error) = stream.write_all(begin_line.as_bytes()).await {
        debug!(session_id, "control connection: write session_begin failed: {error}");
        return Ok(true);
    }
    let _ = stream.flush().await;
    let mut reader = BufReader::new(stream);
    let mut buffer = String::new();
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut buffer),
    )
    .await
    {
        Ok(Ok(bytes)) if bytes > 0 => {}
        Ok(Ok(_)) => {
            debug!(session_id, "control connection closed before session_begin ACK");
            return Ok(true);
        }
        Ok(Err(error)) => {
            debug!(session_id, "control connection ACK read failed: {error}");
            return Ok(true);
        }
        Err(_) => {
            debug!(session_id, "control connection timed out waiting for session_begin ACK");
            return Ok(true);
        }
    }
    let approval_broker_token = validate_control_ack(buffer.trim(), expected_profile)
        .map_err(|error| error.to_string())?;
    let _ = readiness.send(ControlConnectionState::Ready {
        approval_broker_token,
    });
    debug!(session_id, "control connection established (session_begin acknowledged)");

    loop {
        buffer.clear();
        match reader.read_line(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    let _ = readiness.send(ControlConnectionState::Connecting);
    Ok(true)
}

#[cfg(all(not(unix), target_os = "windows"))]
async fn run_windows_control_connection_once(
    socket_path: &str,
    session_id: &str,
    begin_line: &str,
    expected_profile: DaemonProfile,
    readiness: &tokio::sync::watch::Sender<ControlConnectionState>,
) -> Result<bool, String> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut client = loop {
        match ClientOptions::new().open(socket_path) {
            Ok(client) => break client,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                debug!(session_id, "control pipe open failed while daemon restarts: {error}");
                return Ok(false);
            }
        }
    };
    if let Err(error) = client.write_all(begin_line.as_bytes()).await {
        debug!(session_id, "control connection: write session_begin failed: {error}");
        return Ok(true);
    }
    let _ = client.flush().await;
    let mut reader = BufReader::new(client);
    let mut buffer = String::new();
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut buffer),
    )
    .await
    {
        Ok(Ok(bytes)) if bytes > 0 => {}
        Ok(Ok(_)) => {
            debug!(session_id, "control connection closed before session_begin ACK");
            return Ok(true);
        }
        Ok(Err(error)) => {
            debug!(session_id, "control connection ACK read failed: {error}");
            return Ok(true);
        }
        Err(_) => {
            debug!(session_id, "control connection timed out waiting for session_begin ACK");
            return Ok(true);
        }
    }
    let approval_broker_token = validate_control_ack(buffer.trim(), expected_profile)
        .map_err(|error| error.to_string())?;
    let _ = readiness.send(ControlConnectionState::Ready {
        approval_broker_token,
    });
    debug!(session_id, "control connection established (session_begin acknowledged)");

    loop {
        buffer.clear();
        match reader.read_line(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    let _ = readiness.send(ControlConnectionState::Connecting);
    Ok(true)
}

/// Mint a session id unique among the live proxies sharing one daemon, for the
/// lifetime of this proxy process. `pid + process-start nanos` is dep-free and
/// sufficient: two proxies can't share a pid concurrently, and the nanos guard
/// disambiguates pid reuse across the daemon's lifetime. We deliberately avoid
/// the `uuid` crate — a single v4 mint isn't worth a new dependency.
fn mint_session_id() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("mcp-{pid}-{nanos}")
}

fn validate_daemon_profile_and_roster(
    result: &serde_json::Value,
    expected_profile: DaemonProfile,
    socket_path: &str,
) -> anyhow::Result<()> {
    let reported_profile = result
        .get("profile")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "daemon on {socket_path} did not report a tool profile. Stop that daemon and \
                 restart it with the current cua-driver binary before retrying."
            )
        })?;
    if reported_profile != expected_profile.as_str() {
        anyhow::bail!(
            "daemon profile mismatch on {socket_path}: MCP requested `{expected_profile}`, \
             but the daemon reports `{reported_profile}`. Stop it and restart the matching \
             `cua-driver serve{}` profile.",
            if expected_profile == DaemonProfile::CodexComputerUseCompat {
                " --codex-computer-use-compat"
            } else {
                ""
            }
        );
    }

    if expected_profile == DaemonProfile::CodexComputerUseCompat {
        let tools = result
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("daemon list response missing `tools` array"))?;
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
            .collect();
        if names.as_slice() != CODEX_COMPUTER_USE_TOOL_NAMES {
            anyhow::bail!(
                "daemon on {socket_path} reports the Codex Computer Use profile, but its tool \
                 roster is invalid: expected {:?}, got {:?}. Restart it with the current \
                 cua-driver binary.",
                CODEX_COMPUTER_USE_TOOL_NAMES,
                names
            );
        }
    }
    Ok(())
}

/// One-shot daemon `list` over the UDS, reshaped into a MCP
/// `tools/list` result. The daemon now returns the full ToolDef
/// (`name`, `description`, `input_schema`, annotation hints) per
/// commit 3's `serve.rs` change.
///
/// macOS serves the in-process list only while its lazy proxy is dormant. Once
/// connected, it fetches this daemon-authored list once per observed daemon
/// generation and caches it for later `tools/list` requests.
fn fetch_tools_list_from_daemon(
    socket_path: &str,
    session_id: &str,
    expected_profile: DaemonProfile,
) -> anyhow::Result<serde_json::Value> {
    let req = DaemonRequest {
        method: "list".into(),
        name: None,
        args: None,
        session_id: Some(session_id.to_owned()),
    };
    let resp = send_request(socket_path, &req)?;
    if !resp.ok {
        anyhow::bail!(
            "daemon refused tool list on {socket_path}: {}",
            resp.error.unwrap_or_else(|| "(no error message)".into())
        );
    }
    let result = resp.result.ok_or_else(|| {
        anyhow::anyhow!("daemon list response missing `result` field")
    })?;
    validate_daemon_profile_and_roster(&result, expected_profile, socket_path)?;
    let tools_array = result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            anyhow::anyhow!("daemon list response missing `tools` array")
        })?;

    // Reshape the daemon's `{name, description, input_schema, read_only,
    // ..., capabilities}` envelope into MCP's `{name, description,
    // inputSchema, annotations: {...}, capabilities}` shape. Same
    // translation `ToolDef::to_list_entry` does for the in-process
    // path so MCP clients see identical tools/list output either way.
    //
    // `capabilities` is passed through verbatim when the daemon
    // provides it; older daemons that don't emit the field fall back
    // to a name-keyed lookup so the proxy still surfaces capability
    // metadata without an extra round-trip.
    let mcp_tools: Vec<serde_json::Value> = tools_array
        .iter()
        .map(|t| {
            let name = t.get("name").cloned().unwrap_or(serde_json::Value::Null);
            let description = t
                .get("description")
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new()));
            let input_schema = t.get("input_schema").cloned().unwrap_or_else(
                || serde_json::json!({"type": "object", "properties": {}}),
            );
            let read_only = t.get("read_only").and_then(|v| v.as_bool()).unwrap_or(false);
            let destructive =
                t.get("destructive").and_then(|v| v.as_bool()).unwrap_or(false);
            let idempotent =
                t.get("idempotent").and_then(|v| v.as_bool()).unwrap_or(false);
            let open_world =
                t.get("open_world").and_then(|v| v.as_bool()).unwrap_or(false);
            let capabilities = t.get("capabilities")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_else(|| {
                    // Fallback: derive from the centralised map by
                    // name. Keeps the proxy compatible with daemon
                    // builds that pre-date the capabilities field.
                    name.as_str()
                        .map(cua_driver_core::tool::default_capabilities_for)
                        .unwrap_or_default()
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect()
                });
            let mut entry = serde_json::json!({
                "name": name,
                "description": description,
                "inputSchema": input_schema,
                "annotations": {
                    "readOnlyHint": read_only,
                    "destructiveHint": destructive,
                    "idempotentHint": idempotent,
                    "openWorldHint": open_world,
                },
            });
            if expected_profile != DaemonProfile::CodexComputerUseCompat {
                entry["capabilities"] = serde_json::Value::Array(capabilities);
            }
            entry
        })
        .collect();

    // `capability_version` and `schema_version` are passed through
    // when the daemon emits them; older daemons fall back to the
    // proxy's compiled-in `CAPABILITY_VERSION` so MCP clients always
    // see the envelope keys.
    let capability_version = result
        .get("capability_version")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::String(
            cua_driver_core::tool::CAPABILITY_VERSION.to_owned()
        ));
    let schema_version = result
        .get("schema_version")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::String("1".to_owned()));

    if expected_profile == DaemonProfile::CodexComputerUseCompat {
        Ok(serde_json::json!({"tools": mcp_tools}))
    } else {
        Ok(serde_json::json!({
            "tools": mcp_tools,
            "capability_version": capability_version,
            "schema_version": schema_version,
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppApprovalChallenge {
    challenge: String,
    app_identifier: String,
    display_name: String,
    allow_persistent_approval: bool,
}

const APPROVAL_BROKER_TOKEN_ARG: &str = "_cua_approval_broker_token";

fn with_approval_broker_token(
    mut args: serde_json::Value,
    broker_token: &str,
) -> serde_json::Value {
    if !args.is_object() {
        args = serde_json::Value::Object(serde_json::Map::new());
    }
    args.as_object_mut().unwrap().insert(
        APPROVAL_BROKER_TOKEN_ARG.to_owned(),
        serde_json::Value::String(broker_token.to_owned()),
    );
    args
}

impl AppApprovalChallenge {
    fn from_daemon_response(response: &DaemonResponse) -> Option<Self> {
        let structured = response
            .result
            .as_ref()?
            .get("structuredContent")?;
        if structured.get("code")?.as_str()? != "app_approval_required" {
            return None;
        }
        let approval = structured.get("approval")?;
        Some(Self {
            challenge: approval.get("challenge")?.as_str()?.to_owned(),
            app_identifier: approval.get("app")?.as_str()?.to_owned(),
            display_name: approval.get("displayName")?.as_str()?.to_owned(),
            allow_persistent_approval: approval
                .get("allowPersistentApproval")
                .and_then(serde_json::Value::as_bool)
                .or_else(|| {
                    approval
                        .get("allow_persistent_approval")
                        .and_then(serde_json::Value::as_bool)
                })
                .unwrap_or(false),
        })
    }

    fn elicitation_request(&self, request_id: &str) -> serde_json::Value {
        let persist = if self.allow_persistent_approval {
            serde_json::json!(["session", "always"])
        } else {
            serde_json::json!(["session"])
        };
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "elicitation/create",
            "params": {
                "message": format!(
                    "Allow Computer Use to use \"{}\"?",
                    self.display_name
                ),
                "requestedSchema": {
                    "type": "object",
                    "properties": {},
                },
                "_meta": {
                    "codex_approval_kind": "mcp_tool_call",
                    "connector_id": "computer-use",
                    "connector_name": "Computer Use",
                    "persist": persist,
                    "riskLevel": "low",
                    "tool_params": {"app": self.app_identifier},
                    "tool_params_display": [{
                        "name": "app",
                        "display_name": "App",
                        "value": self.display_name,
                    }],
                },
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl ElicitationAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ElicitationDecision {
    action: ElicitationAction,
    persistence: Option<String>,
}

fn supports_elicitation(request: &Request) -> bool {
    request
        .params
        .as_ref()
        .and_then(|params| params.get("capabilities"))
        .and_then(|capabilities| capabilities.get("elicitation"))
        .is_some_and(serde_json::Value::is_object)
}

fn parse_elicitation_decision(
    response: &serde_json::Value,
    expected_id: &str,
) -> Result<ElicitationDecision, String> {
    if response.get("id").and_then(serde_json::Value::as_str) != Some(expected_id) {
        return Err("elicitation response id did not match the pending request".to_owned());
    }
    if let Some(error) = response.get("error") {
        return Err(format!("MCP client rejected app approval elicitation: {error}"));
    }
    let result = response
        .get("result")
        .ok_or_else(|| "elicitation response did not include a result".to_owned())?;
    let action = match result.get("action").and_then(serde_json::Value::as_str) {
        Some("accept") => ElicitationAction::Accept,
        Some("decline") => ElicitationAction::Decline,
        Some("cancel") => ElicitationAction::Cancel,
        Some(other) => return Err(format!("unsupported elicitation action '{other}'")),
        None => return Err("elicitation response did not include an action".to_owned()),
    };
    let persistence = result
        .get("_meta")
        .and_then(|meta| meta.get("persist"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(ElicitationDecision {
        action,
        persistence,
    })
}

async fn write_json_value<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let serialized = serde_json::to_vec(value)?;
    writer.write_all(&serialized).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn await_elicitation_decision<R, W>(
    lines: &mut tokio::io::Lines<R>,
    writer: &mut W,
    request_id: &str,
) -> Result<ElicitationDecision, String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|error| format!("read elicitation response: {error}"))?
            .ok_or_else(|| {
                "MCP client disconnected during app approval elicitation".to_owned()
            })?;
        let value: serde_json::Value = serde_json::from_str(line.trim())
            .map_err(|error| format!("decode elicitation response: {error}"))?;
        if value.get("id").and_then(serde_json::Value::as_str) == Some(request_id) {
            return parse_elicitation_decision(&value, request_id);
        }

        // MCP permits notifications while a server request is pending. Ignore
        // those, but fail an interleaved client request explicitly so the
        // caller does not wait forever for a response that cannot run yet.
        if value.get("method").is_some() && value.get("id").is_some() {
            let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let response = Response::error(
                id,
                -32000,
                "another MCP request cannot run while app approval is pending",
            );
            let response = serde_json::to_value(response)
                .map_err(|error| format!("encode pending-request response: {error}"))?;
            write_json_value(writer, &response)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
}

async fn send_daemon_request_async(
    socket_path: &str,
    request: DaemonRequest,
    context: &str,
) -> Result<DaemonResponse, String> {
    let socket = socket_path.to_owned();
    let blocking = tokio::task::spawn_blocking(move || send_request(&socket, &request)).await;
    match blocking {
        Err(error) => Err(format!("internal join error {context}: {error}")),
        Ok(Err(error)) => Err(format!("daemon transport error {context}: {error}")),
        Ok(Ok(response)) => Ok(response),
    }
}

async fn call_daemon_tool(
    socket_path: &str,
    session_id: &str,
    name: &str,
    args: serde_json::Value,
) -> Result<DaemonResponse, String> {
    send_daemon_request_async(
        socket_path,
        DaemonRequest {
            method: "call".to_owned(),
            name: Some(name.to_owned()),
            args: Some(args),
            session_id: Some(session_id.to_owned()),
        },
        &format!("forwarding `{name}`"),
    )
    .await
}

fn app_approval_error_response(
    id: serde_json::Value,
    code: &str,
    message: impl Into<String>,
) -> Response {
    let message = message.into();
    Response::ok(
        id,
        serde_json::json!({
            "content": [{"type": "text", "text": message}],
            "isError": true,
            "structuredContent": {"code": code, "message": message},
        }),
    )
}

async fn forward_tool_call_with_approval<R, W>(
    id: serde_json::Value,
    name: String,
    args: serde_json::Value,
    socket_path: &str,
    session_id: &str,
    control_ready: &tokio::sync::watch::Receiver<ControlConnectionState>,
    client_supports_elicitation: bool,
    lines: &mut tokio::io::Lines<R>,
    writer: &mut W,
) -> Response
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let broker_token = match wait_for_control_connection(control_ready.clone()).await {
        Ok(Some(token)) => token,
        Ok(None) => {
            return Response::error(
                id,
                -32603,
                "daemon did not provide an authenticated app approval broker token",
            )
        }
        Err(error) => {
            return Response::error(
                id,
                -32603,
                format!(
                    "daemon session control connection unavailable before forwarding `{name}`: {error}"
                ),
            )
        }
    };

    let authenticated_args = with_approval_broker_token(args, &broker_token);

    let first = match call_daemon_tool(
        socket_path,
        session_id,
        &name,
        authenticated_args.clone(),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => return Response::error(id, -32603, error),
    };
    let Some(challenge) = AppApprovalChallenge::from_daemon_response(&first) else {
        return daemon_response_to_mcp(id, &name, first);
    };

    if !client_supports_elicitation {
        return app_approval_error_response(
            id,
            "elicitation_not_supported",
            format!(
                "Computer Use requires app approval for '{}', but this MCP client did not negotiate the elicitation capability.",
                challenge.display_name
            ),
        );
    }

    let elicitation_id = format!("cua-app-approval-{}", uuid::Uuid::new_v4());
    if let Err(error) = write_json_value(writer, &challenge.elicitation_request(&elicitation_id)).await {
        return Response::error(
            id,
            -32603,
            format!("write app approval elicitation: {error}"),
        );
    }
    let decision = match await_elicitation_decision(lines, writer, &elicitation_id).await {
        Ok(decision) => decision,
        Err(error) => {
            return app_approval_error_response(id, "app_approval_unavailable", error)
        }
    };

    let resolution = match send_daemon_request_async(
        socket_path,
        DaemonRequest {
            method: "app_approval_resolve".to_owned(),
            name: None,
            args: Some(serde_json::json!({
                "challenge": challenge.challenge,
                "broker_token": broker_token,
                "action": decision.action.as_str(),
                "persist": decision.persistence.as_deref().unwrap_or("session"),
            })),
            session_id: Some(session_id.to_owned()),
        },
        "resolving app approval",
    )
    .await
    {
        Ok(response) => response,
        Err(error) => return Response::error(id, -32603, error),
    };
    if !resolution.ok {
        return app_approval_error_response(
            id,
            "app_approval_unavailable",
            resolution
                .error
                .unwrap_or_else(|| "Computer Use could not resolve app approval.".to_owned()),
        );
    }
    let resolution = resolution.result.unwrap_or_default();
    match resolution
        .get("resolution")
        .and_then(serde_json::Value::as_str)
    {
        Some("approved") => {
            let retry = match call_daemon_tool(
                socket_path,
                session_id,
                &name,
                authenticated_args,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => return Response::error(id, -32603, error),
            };
            if AppApprovalChallenge::from_daemon_response(&retry).is_some() {
                return app_approval_error_response(
                    id,
                    "app_approval_unavailable",
                    "Computer Use approval was accepted, but the daemon requested approval again.",
                );
            }
            daemon_response_to_mcp(id, &name, retry)
        }
        Some("declined") => app_approval_error_response(
            id,
            "app_approval_denied",
            resolution
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Computer Use approval was declined."),
        ),
        Some("canceled") => app_approval_error_response(
            id,
            "app_approval_canceled",
            resolution
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Computer Use approval was canceled."),
        ),
        _ => app_approval_error_response(
            id,
            "app_approval_unavailable",
            "Computer Use daemon returned an invalid app approval resolution.",
        ),
    }
}

/// Bring the daemon (and the reaper control connection) up exactly once, on the
/// first `tools/call`. Serving `initialize` / `tools/list` from the in-process
/// registry keeps the permission-requesting daemon dormant until the agent
/// actually invokes a tool, so its Accessibility / Screen Recording prompts
/// appear on real use rather than at agent-session start.
///
/// `launch_daemon_and_wait` runs `open -n -g -a <helper> --args serve` and
/// blocks until the daemon's socket is up (the daemon binds before running its
/// permission gate, so this returns promptly even while the gate prompts). It's
/// a blocking call, so it runs on the blocking pool.
#[cfg(target_os = "macos")]
async fn ensure_daemon_available_with<Probe, Launch>(
    previously_started: bool,
    externally_owned: bool,
    timeout: std::time::Duration,
    retry_interval: std::time::Duration,
    mut probe: Probe,
    launch: Launch,
) -> anyhow::Result<()>
where
    Probe: FnMut() -> bool,
    Launch: FnOnce() -> anyhow::Result<()>,
{
    if probe() {
        return Ok(());
    }

    if externally_owned {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                let state = if previously_started {
                    "after the previously-connected daemon exited"
                } else {
                    "during initial connection"
                };
                anyhow::bail!("cmux-owned Computer Use daemon stayed unavailable {state}");
            }
            tokio::time::sleep(retry_interval).await;
            if probe() {
                return Ok(());
            }
        }
    }

    launch()?;
    if probe() {
        Ok(())
    } else {
        anyhow::bail!("launched Computer Use daemon did not become reachable")
    }
}

/// Lazy proxy startup has two independent milestones: the daemon/reaper is
/// connected, and the onboarding grant wait has completed. Keeping them
/// separate ensures a prompt status call cannot accidentally waive onboarding
/// for the next driving action.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct DaemonStartState {
    reaper_started: bool,
    grant_wait_completed: bool,
    daemon_generation: u64,
    tools_list_generation: Option<u64>,
    authoritative_tools_list: Option<Arc<serde_json::Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DaemonLifecycleEvent {
    generation: u64,
}

#[cfg(target_os = "macos")]
impl DaemonStartState {
    fn needs_grant_wait(&self, request_requires_wait: bool, external_permission_flow: bool) -> bool {
        request_requires_wait && !external_permission_flow && !self.grant_wait_completed
    }

    fn complete_grant_wait(&mut self) {
        self.grant_wait_completed = true;
    }

    /// Record an observed socket outage. Until a replacement daemon is
    /// connected and queried, `tools/list` must fall back to the local,
    /// permission-free bootstrap registry rather than advertise the vanished
    /// daemon's contract.
    fn observe_daemon_loss(&mut self) {
        self.grant_wait_completed = false;
        self.tools_list_generation = None;
        self.authoritative_tools_list = None;
    }

    /// Apply the EOF signal from the generation-tagged control connection.
    /// Returns whether the event invalidated the current daemon. A delayed EOF
    /// from an older control task must not tear down a replacement generation.
    fn observe_control_connection_end(&mut self, generation: u64) -> bool {
        if !self.reaper_started || generation != self.daemon_generation {
            return false;
        }
        self.observe_daemon_loss();
        self.daemon_generation = self.daemon_generation.wrapping_add(1).max(1);
        true
    }

    /// Start a new observed daemon generation and return its stable token.
    /// The token lets the cache policy distinguish a replacement daemon from
    /// the one whose list it previously stored without a list round-trip on
    /// every tool call.
    fn begin_daemon_generation(&mut self) -> u64 {
        self.daemon_generation = self.daemon_generation.wrapping_add(1).max(1);
        self.reaper_started = true;
        self.daemon_generation
    }

    fn tools_list_refresh_generation(&self) -> Option<u64> {
        if self.reaper_started
            && self.tools_list_generation != Some(self.daemon_generation)
        {
            Some(self.daemon_generation)
        } else {
            None
        }
    }

    fn cache_authoritative_tools_list(
        &mut self,
        generation: u64,
        tools_list: Arc<serde_json::Value>,
    ) -> bool {
        if !self.reaper_started || generation != self.daemon_generation {
            return false;
        }
        self.authoritative_tools_list = Some(tools_list);
        self.tools_list_generation = Some(generation);
        true
    }

    fn tools_list_or<'a>(
        &'a self,
        bootstrap: &'a Arc<serde_json::Value>,
    ) -> &'a Arc<serde_json::Value> {
        match (
            self.tools_list_generation,
            self.authoritative_tools_list.as_ref(),
        ) {
            (Some(generation), Some(tools_list))
                if self.reaper_started && generation == self.daemon_generation =>
            {
                tools_list
            }
            _ => bootstrap,
        }
    }
}

/// `check_permissions` is itself the supported permission/status surface. It
/// must reach the daemon immediately, whether it is a read-only
/// `{ "prompt": false }` probe or the explicit prompting form, instead of
/// waiting up to 55 seconds for the condition it reports/requests. Every other
/// tool call retains the onboarding wait, including all driving actions.
#[cfg(target_os = "macos")]
fn tool_call_requires_grant_wait(req: &Request) -> bool {
    req.tool_call()
        .map(|call| call.name != "check_permissions")
        .unwrap_or(true)
}

/// Permission-free admission for macOS lazy proxy calls. Protocol shape, tool
/// identity, and the advertised input schema are checked before the helper is
/// started or health-recovered. The supplied list is the bootstrap registry
/// while dormant and the cached daemon-authored list after connection.
#[cfg(target_os = "macos")]
enum BootstrapToolCallAdmission {
    InvalidParams(String),
    Rejected(ToolResult),
    Ready {
        call: ToolCall,
        wait_for_grants: bool,
    },
}

#[cfg(target_os = "macos")]
fn admit_bootstrap_tool_call(
    req: &Request,
    tools_list: &serde_json::Value,
) -> BootstrapToolCallAdmission {
    let call = match req.tool_call() {
        Ok(call) => call,
        Err(error) => return BootstrapToolCallAdmission::InvalidParams(error.to_string()),
    };

    // Preserve the registry's sole deprecated alias even though aliases are
    // intentionally omitted from tools/list.
    let advertised_name = match call.name.as_str() {
        "type_text_chars" => "type_text",
        other => other,
    };
    let Some(entry) = tools_list
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .and_then(|tools| {
            tools.iter().find(|tool| {
                tool.get("name").and_then(serde_json::Value::as_str)
                    == Some(advertised_name)
            })
        })
    else {
        return BootstrapToolCallAdmission::Rejected(ToolResult::error(format!(
            "Unknown tool: {}",
            call.name
        )));
    };

    let def = cua_driver_core::tool::ToolDef {
        name: advertised_name.to_owned(),
        description: String::new(),
        input_schema: entry
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    };
    if let Err(result) = cua_driver_core::tool::validate_dispatch_args(&def, &call.args) {
        return BootstrapToolCallAdmission::Rejected(result);
    }

    let wait_for_grants = tool_call_requires_grant_wait(req);
    BootstrapToolCallAdmission::Ready {
        call,
        wait_for_grants,
    }
}

#[cfg(target_os = "macos")]
async fn ensure_daemon_started(
    socket_path: &str,
    state: &mut DaemonStartState,
    session_id: &str,
    claude_code_compat: bool,
    expected_profile: DaemonProfile,
    control_ready: &tokio::sync::watch::Sender<ControlConnectionState>,
    wait_for_grants: bool,
    lifecycle_tx: &tokio::sync::mpsc::UnboundedSender<DaemonLifecycleEvent>,
) -> anyhow::Result<()> {
    let listening = is_daemon_listening(socket_path);
    if !listening {
        // A replacement daemon needs a fresh control connection and, for the
        // next driving action, a fresh onboarding stability wait.
        let previously_started = state.reaper_started;
        state.observe_daemon_loss();
        // FORCE_PROXY callers supply their own daemon and have no bundle to
        // relaunch into — never auto-launch on their behalf.
        if crate::bundle::requires_external_daemon() {
            ensure_daemon_available_with(
                previously_started,
                true,
                std::time::Duration::from_secs(10),
                std::time::Duration::from_millis(100),
                || is_daemon_listening(socket_path),
                || anyhow::bail!("forced proxy cannot launch a standalone daemon"),
            )
            .await
            .map_err(|e| anyhow::anyhow!(
                "the tag-scoped cmux Computer Use runtime is not listening on {socket_path}: {e}"
            ))?;
        } else {
            let sp = socket_path.to_owned();
            tokio::task::spawn_blocking(move || {
                crate::cli::launch_daemon_and_wait(
                    &sp,
                    10,
                    claude_code_compat,
                    expected_profile == DaemonProfile::CodexComputerUseCompat,
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("daemon launch task failed: {e}"))??;
        }
    }
    // Daemon is up: start the reaper now (deferred from proxy startup).
    if !state.reaper_started {
        let generation = state.begin_daemon_generation();
        let socket = socket_path.to_owned();
        let sid = session_id.to_owned();
        let ready = control_ready.clone();
        let lifecycle_tx = lifecycle_tx.clone();
        tokio::spawn(async move {
            run_control_connection(
                socket,
                sid,
                expected_profile,
                ready,
                Some((lifecycle_tx, generation)),
            )
            .await;
        });
    }
    let _ = wait_for_control_connection(control_ready.subscribe()).await?;
    // The bootstrap list intentionally keeps initialize/tools/list local and
    // permission-free while the lazy daemon is dormant. Once a daemon is
    // connected, however, that daemon is the execution authority and its
    // schema must win. Fetch once for this observed generation, then reuse the
    // cache until a socket outage invalidates it. `send_request` is blocking
    // UDS I/O, so keep it off Tokio's worker threads.
    if let Some(generation) = state.tools_list_refresh_generation() {
        let socket = socket_path.to_owned();
        let sid = session_id.to_owned();
        let tools_list = tokio::task::spawn_blocking(move || {
            fetch_tools_list_from_daemon(&socket, &sid, expected_profile)
        })
        .await
        .map_err(|e| anyhow::anyhow!("daemon tool-list task failed: {e}"))??;
        if !state.cache_authoritative_tools_list(generation, Arc::new(tools_list)) {
            anyhow::bail!("daemon changed while its authoritative tool list was loading");
        }
    }
    // Onboarding: wait until BOTH TCC grants are in before the first tool
    // executes. The daemon's startup gate raises the Accessibility + Screen
    // Recording prompts and re-execs the daemon (~every 25s) to pick up each
    // grant. If the agent's first tool call runs during that window it races
    // the re-exec — dropped connections mid-click and a cursor that blinks out
    // (its overlay state is lost on re-exec until the next move). Holding the
    // first call until the grants settle makes onboarding go through all steps
    // first, then run on a stable daemon with a stable cursor. Bounded +
    // fail-safe: on timeout we proceed and let the tool call surface any real
    // TCC error, so a user who ignores the prompts is never hung forever.
    let external_permission_flow =
        crate::bundle::is_env_truthy("CUA_DRIVER_RS_EXTERNAL_PERMISSION_FLOW");
    if state.needs_grant_wait(wait_for_grants, external_permission_flow) {
        wait_for_daemon_grants(socket_path, session_id).await;
        state.complete_grant_wait();
    } else if wait_for_grants && external_permission_flow {
        // The embedding host owns permission onboarding, so driving calls are
        // intentionally considered past this proxy-local milestone.
        state.complete_grant_wait();
    }
    Ok(())
}

/// Poll the daemon's `check_permissions` (read-only, `prompt:false`) until both
/// Accessibility and Screen Recording read granted, or a bounded deadline
/// elapses. Kept under a typical MCP client tool-call timeout so a slow grant
/// degrades to "first call fails, agent retries on the now-granted daemon"
/// rather than a hung call. Every failure path (transport error during a gate
/// re-exec, unexpected shape) just retries until the deadline.
#[cfg(target_os = "macos")]
async fn wait_for_daemon_grants(socket_path: &str, session_id: &str) {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(55);
    loop {
        if Instant::now() >= deadline {
            debug!("wait_for_daemon_grants: deadline elapsed; proceeding");
            return;
        }
        let sp = socket_path.to_owned();
        let sid = session_id.to_owned();
        let probe = tokio::task::spawn_blocking(move || {
            let req = DaemonRequest {
                method: "call".into(),
                name: Some("check_permissions".into()),
                args: Some(serde_json::json!({ "prompt": false })),
                session_id: Some(sid),
            };
            send_request(&sp, &req)
        })
        .await;
        if let Ok(Ok(resp)) = probe {
            if let Some(result) = resp.result.as_ref() {
                if let Some((ax, sr)) = extract_grants(result) {
                    if ax && sr {
                        debug!("wait_for_daemon_grants: both grants active");
                        return;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

/// Recursively locate the `check_permissions` structured payload — an object
/// carrying both `accessibility` and `screen_recording` booleans — wherever the
/// daemon nests it, and return `(accessibility, screen_recording)`. Shape-
/// tolerant so the poll doesn't depend on the exact result envelope.
#[cfg(target_os = "macos")]
fn extract_grants(v: &serde_json::Value) -> Option<(bool, bool)> {
    match v {
        serde_json::Value::Object(obj) => {
            if let (Some(a), Some(s)) = (
                obj.get("accessibility").and_then(serde_json::Value::as_bool),
                obj.get("screen_recording").and_then(serde_json::Value::as_bool),
            ) {
                return Some((a, s));
            }
            obj.values().find_map(extract_grants)
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(extract_grants),
        _ => None,
    }
}

/// JSON-RPC method dispatcher for the proxy. Mirrors
/// `cua_driver_core::server::handle_request`:
///   - `initialize`     → static `initialize_result()` (same envelope
///                        the in-process path returns; the daemon's
///                        identity is hidden from the MCP client).
///   - `tools/list`     → return the current bootstrap/daemon cache.
///   - `tools/call`     → forward to the daemon and reshape the
///                        response into MCP's `CallTool.Result`.
///   - other            → method-not-found, same as in-process.
async fn handle_proxy_request(
    req: Request,
    id: serde_json::Value,
    socket_path: &str,
    cached_tools_list: &serde_json::Value,
    session_id: &str,
    control_ready: &tokio::sync::watch::Receiver<ControlConnectionState>,
    expected_profile: DaemonProfile,
) -> Response {
    match req.method.as_str() {
        "initialize" => Response::ok(
            id,
            if expected_profile == DaemonProfile::CodexComputerUseCompat {
                codex_computer_use_initialize_result()
            } else {
                initialize_result()
            },
        ),

        "tools/list" => Response::ok(id, cached_tools_list.clone()),

        "tools/call" => match req.tool_call() {
            Err(e) => Response::error(id, -32602, format!("Invalid params: {e}")),
            Ok(call) => {
                forward_tool_call(
                    id,
                    call.name,
                    call.args,
                    socket_path,
                    session_id,
                    control_ready,
                )
                .await
            }
        },

        other => {
            warn!(method = other, "unknown method");
            Response::method_not_found(id, other)
        }
    }
}

#[cfg(target_os = "macos")]
fn response_from_tool_result(id: serde_json::Value, result: ToolResult) -> Response {
    match serde_json::to_value(result) {
        Ok(value) => Response::ok(id, value),
        Err(error) => Response::error(id, -32603, format!("Serialize error: {error}")),
    }
}

/// Forward a single MCP `tools/call` to the daemon as a `call`
/// request, then translate the `DaemonResponse` back into an MCP
/// `CallTool.Result` envelope.
///
/// Error mapping:
///   - Tool ran and reported failure (`!resp.ok`, including unknown
///     tool / bad params) → JSON-RPC success with `result.isError =
///     true`. Mirrors the in-process `cua_driver_core::server` path so
///     MCP clients see identical envelopes either way.
///   - Transport failure (UDS unreachable, decode error, blocking
///     task panic) → JSON-RPC error (`-32603`), because the MCP
///     client really does need to distinguish "tool said no" from
///     "I couldn't reach the tool at all."
async fn forward_tool_call(
    id: serde_json::Value,
    name: String,
    args: serde_json::Value,
    socket_path: &str,
    session_id: &str,
    control_ready: &tokio::sync::watch::Receiver<ControlConnectionState>,
) -> Response {
    if let Err(error) = wait_for_control_connection(control_ready.clone()).await {
        return Response::error(
            id,
            -32603,
            format!(
                "daemon session control connection unavailable before forwarding `{name}`: {error}"
            ),
        );
    }
    let response = match call_daemon_tool(socket_path, session_id, &name, args).await {
        Ok(response) => response,
        Err(error) => return Response::error(id, -32603, error),
    };
    daemon_response_to_mcp(id, &name, response)
}

fn daemon_response_to_mcp(
    id: serde_json::Value,
    _name: &str,
    response: DaemonResponse,
) -> Response {
    if !response.ok {
        // MCP separates two failure modes:
        //   - JSON-RPC errors → `Response::error(...)`, used for
        //     transport / protocol failures (unknown method, bad
        //     params shape, server crash).
        //   - Tool-level errors → `Response::ok(...)` carrying a
        //     `CallTool.Result` with `isError: true` and the error
        //     message in `content[]`. The tool ran, returned a
        //     well-formed result that says "I failed."
        //
        // A non-`ok` daemon response means the tool call reached the
        // daemon and the daemon decided the tool returned an error
        // (or rejected the call). That's tool-level, not transport-
        // level, so the in-process `cua_driver_core::server` would surface
        // it as `Response::ok` with `isError: true`. Mirror that
        // shape here so MCP clients see identical envelopes either
        // way — CodeRabbit #2.
        if let Some(result) = response
            .result
            .filter(|result| result.get("isError").and_then(|value| value.as_bool()) == Some(true))
        {
            return Response::ok(id, result);
        }
        let msg = response
            .error
            .unwrap_or_else(|| "daemon reported failure".into());
        let exit_code = response.exit_code.unwrap_or(1);
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": msg }],
            "isError": true,
            "structuredContent": { "exit_code": exit_code }
        });
        return Response::ok(id, result);
    }

    let result = response.result.unwrap_or_else(|| {
        serde_json::json!({
            "content": [],
            "isError": false
        })
    });
    Response::ok(id, result)
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// Unit-test only the JSON shape of the proxy's tool-error envelope.
// The full proxy loop is exercised by the macOS integration test
// (the CUA_DRIVER_RS_MCP_FORCE_PROXY harness); these tests just lock
// in the per-branch reshape so a
// regression to `Response::error` for tool-level failures would fail
// fast in CI on every platform.

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_challenge_response(allow_persistent: bool) -> DaemonResponse {
        DaemonResponse::tool_error(
            serde_json::json!({
                "content": [{"type":"text","text":"approval required"}],
                "isError": true,
                "structuredContent": {
                    "code": "app_approval_required",
                    "message": "approval required",
                    "approval": {
                        "challenge": "challenge-1",
                        "app": "com.apple.calculator",
                        "displayName": "Calculator",
                        "allowPersistentApproval": allow_persistent,
                    }
                }
            }),
            "approval required",
            1,
        )
    }

    #[test]
    fn initialize_capability_negotiation_is_explicit() {
        let supported: Request = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"capabilities": {"elicitation": {}}},
        }))
        .unwrap();
        assert!(supports_elicitation(&supported));

        let unsupported: Request = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"capabilities": {}},
        }))
        .unwrap();
        assert!(!supports_elicitation(&unsupported));
    }

    #[test]
    fn compat_calls_overwrite_caller_broker_fields_with_the_daemon_token() {
        let args = with_approval_broker_token(
            serde_json::json!({
                "app": "Calculator",
                (APPROVAL_BROKER_TOKEN_ARG): "caller-forged",
            }),
            "daemon-minted",
        );
        assert_eq!(args["app"], "Calculator");
        assert_eq!(args[APPROVAL_BROKER_TOKEN_ARG], "daemon-minted");
    }

    #[test]
    fn approval_challenge_builds_codex_computer_use_elicitation_metadata() {
        let challenge = AppApprovalChallenge::from_daemon_response(
            &approval_challenge_response(true),
        )
        .unwrap();
        let request = challenge.elicitation_request("approval-request-1");
        assert_eq!(request["method"], "elicitation/create");
        assert_eq!(
            request["params"]["message"],
            "Allow Computer Use to use \"Calculator\"?"
        );
        assert_eq!(
            request["params"]["requestedSchema"],
            serde_json::json!({"type":"object","properties":{}})
        );
        let meta = &request["params"]["_meta"];
        assert_eq!(meta["codex_approval_kind"], "mcp_tool_call");
        assert_eq!(meta["connector_id"], "computer-use");
        assert_eq!(meta["connector_name"], "Computer Use");
        assert_eq!(meta["persist"], serde_json::json!(["session", "always"]));
        assert_eq!(meta["riskLevel"], "low");
        assert_eq!(meta["tool_params"]["app"], "com.apple.calculator");
        assert_eq!(meta["tool_params_display"][0]["value"], "Calculator");

        let session_only = AppApprovalChallenge::from_daemon_response(
            &approval_challenge_response(false),
        )
        .unwrap()
        .elicitation_request("approval-request-2");
        assert_eq!(
            session_only["params"]["_meta"]["persist"],
            serde_json::json!(["session"])
        );
    }

    #[test]
    fn elicitation_decisions_distinguish_accept_decline_cancel_and_persistence() {
        for (action, expected) in [
            ("accept", ElicitationAction::Accept),
            ("decline", ElicitationAction::Decline),
            ("cancel", ElicitationAction::Cancel),
        ] {
            let decision = parse_elicitation_decision(
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "approval-1",
                    "result": {"action": action},
                }),
                "approval-1",
            )
            .unwrap();
            assert_eq!(decision.action, expected);
            assert_eq!(decision.persistence, None);
        }

        let persistent = parse_elicitation_decision(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": "approval-1",
                "result": {"action": "accept", "_meta": {"persist": "always"}},
            }),
            "approval-1",
        )
        .unwrap();
        assert_eq!(persistent.action, ElicitationAction::Accept);
        assert_eq!(persistent.persistence.as_deref(), Some("always"));
    }

    #[tokio::test]
    async fn nested_elicitation_response_is_read_as_a_response_not_a_request() {
        let (proxy_stream, mut client_stream) = tokio::io::duplex(2048);
        let (proxy_read, mut proxy_write) = tokio::io::split(proxy_stream);
        let mut proxy_lines = BufReader::new(proxy_read).lines();
        tokio::spawn(async move {
            client_stream
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":\"approval-7\",\"result\":{\"action\":\"accept\",\"_meta\":{\"persist\":\"always\"}}}\n",
                )
                .await
                .unwrap();
        });
        let decision = await_elicitation_decision(
            &mut proxy_lines,
            &mut proxy_write,
            "approval-7",
        )
        .await
        .unwrap();
        assert_eq!(decision.action, ElicitationAction::Accept);
        assert_eq!(decision.persistence.as_deref(), Some("always"));
    }

    #[test]
    fn unsupported_client_error_is_a_tool_failure() {
        let response = app_approval_error_response(
            serde_json::json!(9),
            "elicitation_not_supported",
            "client did not negotiate elicitation",
        );
        let response = serde_json::to_value(response).unwrap();
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["structuredContent"]["code"],
            "elicitation_not_supported"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsupported_client_does_not_elicit_resolve_or_retry() {
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("approval.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&mut stream);
                reader.read_line(&mut line).await.unwrap();
            }
            let request: DaemonRequest = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request.method, "call");
            assert_eq!(request.name.as_deref(), Some("get_app_state"));
            assert_eq!(
                request.args.as_ref().unwrap()[APPROVAL_BROKER_TOKEN_ARG],
                "daemon-broker-token"
            );
            let response = approval_challenge_response(true);
            stream
                .write_all(
                    format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes(),
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();

            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    listener.accept(),
                )
                .await
                .is_err(),
                "unsupported client must not resolve or retry the challenge"
            );
        });

        let (_control_tx, control_rx) = tokio::sync::watch::channel(
            ControlConnectionState::Ready {
                approval_broker_token: Some("daemon-broker-token".to_owned()),
            },
        );
        let (proxy_stream, _client_stream) = tokio::io::duplex(1024);
        let (proxy_read, mut proxy_write) = tokio::io::split(proxy_stream);
        let mut proxy_lines = BufReader::new(proxy_read).lines();
        let response = forward_tool_call_with_approval(
            serde_json::json!(4),
            "get_app_state".to_owned(),
            serde_json::json!({"app":"Calculator"}),
            socket.to_str().unwrap(),
            "session-a",
            &control_rx,
            false,
            &mut proxy_lines,
            &mut proxy_write,
        )
        .await;
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(
            response["result"]["structuredContent"]["code"],
            "elicitation_not_supported"
        );
        server.await.unwrap();
    }
    #[cfg(target_os = "macos")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(target_os = "macos")]
    use std::time::Duration;

    /// Reconstruct the `!resp.ok` branch in isolation so we can assert
    /// on the serialized shape without spinning up a real daemon /
    /// tokio runtime. Keep this in sync with `forward_tool_call`.
    fn build_tool_error_response(
        id: serde_json::Value,
        resp: DaemonResponse,
    ) -> Response {
        let msg = resp.error.unwrap_or_else(|| "daemon reported failure".into());
        let exit_code = resp.exit_code.unwrap_or(1);
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": msg }],
            "isError": true,
            "structuredContent": { "exit_code": exit_code }
        });
        Response::ok(id, result)
    }

    #[test]
    fn daemon_tool_failure_wraps_as_jsonrpc_success_with_iserror_true() {
        let daemon_resp = DaemonResponse {
            ok: false,
            result: None,
            error: Some("missing required field `pid`".into()),
            exit_code: Some(64),
        };
        let resp = build_tool_error_response(serde_json::json!(7), daemon_resp);
        let value = serde_json::to_value(&resp).expect("serialize");

        // Top-level JSON-RPC envelope: success (`result`), not error.
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], serde_json::json!(7));
        assert!(value.get("error").is_none(),
            "tool-level failure must NOT surface as JSON-RPC error: got {value}");
        assert!(value.get("result").is_some(),
            "tool-level failure must carry a `result` payload: got {value}");

        // CallTool.Result inside `result`: isError + content text.
        let result = &value["result"];
        assert_eq!(result["isError"], serde_json::json!(true));
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "missing required field `pid`");
        assert_eq!(result["structuredContent"]["exit_code"], 64);
    }

    #[test]
    fn daemon_failure_with_no_error_message_uses_fallback_text() {
        let daemon_resp = DaemonResponse {
            ok: false,
            result: None,
            error: None,
            exit_code: None,
        };
        let resp = build_tool_error_response(serde_json::json!("abc"), daemon_resp);
        let value = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(value["result"]["isError"], serde_json::json!(true));
        assert_eq!(value["result"]["content"][0]["text"], "daemon reported failure");
        assert_eq!(value["result"]["structuredContent"]["exit_code"], 1);
    }

    #[test]
    fn daemon_tool_failure_preserves_original_structured_error() {
        let original = serde_json::json!({
            "content": [{"type":"text","text":"Call get_app_state first"}],
            "isError": true,
            "structuredContent": {
                "code": "no_active_app_session",
                "message": "Call get_app_state first"
            }
        });
        let daemon_resp = DaemonResponse::tool_error(original.clone(), "fallback", 1);
        let result = daemon_resp
            .result
            .filter(|result| {
                result.get("isError").and_then(|value| value.as_bool()) == Some(true)
            })
            .expect("structured tool error");
        assert_eq!(result, original);
    }

    fn daemon_list_result(profile: &str, names: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "profile": profile,
            "tools": names.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>()
        })
    }

    #[test]
    fn daemon_profile_mismatch_fails_closed() {
        let native = daemon_list_result("native", &["click"]);
        let error = validate_daemon_profile_and_roster(
            &native,
            DaemonProfile::CodexComputerUseCompat,
            "/tmp/explicit.sock",
        )
        .expect_err("native daemon must not satisfy compat proxy");
        assert!(error.to_string().contains("profile mismatch"));
        assert!(error.to_string().contains("/tmp/explicit.sock"));

        let compat = daemon_list_result(
            "codex-computer-use-compat",
            &CODEX_COMPUTER_USE_TOOL_NAMES,
        );
        let error = validate_daemon_profile_and_roster(
            &compat,
            DaemonProfile::Native,
            "/tmp/explicit.sock",
        )
        .expect_err("compat daemon must not satisfy native proxy");
        assert!(error.to_string().contains("profile mismatch"));
    }

    #[test]
    fn compat_profile_requires_exact_ordered_ten_tool_roster() {
        let exact = daemon_list_result(
            "codex-computer-use-compat",
            &CODEX_COMPUTER_USE_TOOL_NAMES,
        );
        validate_daemon_profile_and_roster(
            &exact,
            DaemonProfile::CodexComputerUseCompat,
            "/tmp/compat.sock",
        )
        .expect("exact compat roster");

        let mut reordered = CODEX_COMPUTER_USE_TOOL_NAMES;
        reordered.swap(0, 1);
        let wrong = daemon_list_result("codex-computer-use-compat", &reordered);
        let error = validate_daemon_profile_and_roster(
            &wrong,
            DaemonProfile::CodexComputerUseCompat,
            "/tmp/compat.sock",
        )
        .expect_err("reordered roster must fail closed");
        assert!(error.to_string().contains("roster is invalid"));
    }

    #[test]
    fn reconnect_ack_must_preserve_the_requested_profile() {
        let native = DaemonResponse::ok(serde_json::json!({
            "session_begin": true,
            "profile": DaemonProfile::Native,
        }));
        let native = serde_json::to_string(&native).unwrap();
        validate_control_ack(&native, DaemonProfile::Native).expect("matching profile");
        let error = validate_control_ack(
            &native,
            DaemonProfile::CodexComputerUseCompat,
        )
        .expect_err("a replacement daemon with the wrong profile must fail closed");
        assert!(error.to_string().contains("profile mismatch"));

        let missing = DaemonResponse::ok(serde_json::json!({"session_begin": true}));
        let missing = serde_json::to_string(&missing).unwrap();
        assert!(validate_control_ack(&missing, DaemonProfile::Native)
            .unwrap_err()
            .to_string()
            .contains("did not report"));

        let compat_without_broker = DaemonResponse::ok(serde_json::json!({
            "session_begin": true,
            "profile": DaemonProfile::CodexComputerUseCompat,
        }));
        assert!(validate_control_ack(
            &serde_json::to_string(&compat_without_broker).unwrap(),
            DaemonProfile::CodexComputerUseCompat,
        )
        .unwrap_err()
        .to_string()
        .contains("approval broker token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn control_connection_reconnects_after_daemon_rebind() {
        use tokio::net::{UnixListener, UnixStream};

        async fn accept_session_begin(
            listener: &UnixListener,
            expected_session: &str,
        ) -> UnixStream {
            let (mut stream, _) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                listener.accept(),
            )
            .await
            .expect("control connection timeout")
            .expect("accept control connection");
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&mut stream);
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await
                .expect("session_begin timeout")
                .expect("read session_begin");
            }
            let request: DaemonRequest =
                serde_json::from_str(line.trim()).expect("decode session_begin");
            assert_eq!(request.method, "session_begin");
            assert_eq!(request.session_id.as_deref(), Some(expected_session));
            let ack = DaemonResponse::ok(serde_json::json!({
                "session_begin": true,
                "profile": DaemonProfile::Native,
            }));
            stream
                .write_all(
                    format!("{}\n", serde_json::to_string(&ack).unwrap()).as_bytes(),
                )
                .await
                .expect("write session_begin ACK");
            stream.flush().await.expect("flush session_begin ACK");
            stream
        }

        let root = tempfile::tempdir().expect("socket tempdir");
        let socket = root.path().join("daemon.sock");
        let first_listener = UnixListener::bind(&socket).expect("bind first daemon");
        let session_id = "proxy-reexec-test-session".to_owned();
        let (ready_tx, mut ready_rx) =
            tokio::sync::watch::channel(ControlConnectionState::Connecting);
        let task = tokio::spawn(run_control_connection(
            socket.to_string_lossy().into_owned(),
            session_id.clone(),
            DaemonProfile::Native,
            ready_tx,
            None,
        ));

        let first_stream = accept_session_begin(&first_listener, &session_id).await;
        wait_for_control_connection(ready_rx.clone())
            .await
            .expect("first control connection ready");
        drop(first_listener);
        drop(first_stream);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while matches!(
                &*ready_rx.borrow(),
                ControlConnectionState::Ready { .. }
            ) {
                ready_rx.changed().await.expect("readiness sender alive");
            }
        })
        .await
        .expect("first control connection closes");
        std::fs::remove_file(&socket).expect("remove first daemon socket");

        let second_listener = UnixListener::bind(&socket).expect("bind replacement daemon");
        let second_stream = accept_session_begin(&second_listener, &session_id).await;
        wait_for_control_connection(ready_rx.clone())
            .await
            .expect("replacement control connection ready");

        task.abort();
        let _ = task.await;
        drop(second_stream);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bootstrap_admission_rejects_malformed_unknown_and_invalid_calls() {
        fn request(params: serde_json::Value) -> Request {
            serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": params,
            }))
            .expect("request envelope")
        }

        let tools_list = crate::build_macos_registry_with_compat(false, false).tools_list();

        let malformed = request(serde_json::json!({ "arguments": {} }));
        assert!(matches!(
            admit_bootstrap_tool_call(&malformed, &tools_list),
            BootstrapToolCallAdmission::InvalidParams(_)
        ));

        let unknown = request(serde_json::json!({
            "name": "definitely_not_a_tool",
            "arguments": {},
        }));
        match admit_bootstrap_tool_call(&unknown, &tools_list) {
            BootstrapToolCallAdmission::Rejected(result) => {
                let value = serde_json::to_value(result).expect("serialize rejection");
                assert_eq!(value["isError"], true);
                assert_eq!(
                    value["content"][0]["text"],
                    "Unknown tool: definitely_not_a_tool"
                );
            }
            _ => panic!("unknown tools must be rejected before daemon startup"),
        }

        let invalid = request(serde_json::json!({
            "name": "set_agent_cursor_enabled",
            "arguments": {},
        }));
        assert!(matches!(
            admit_bootstrap_tool_call(&invalid, &tools_list),
            BootstrapToolCallAdmission::Rejected(_)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bootstrap_admission_accepts_only_real_calls_and_preserves_wait_policy() {
        fn request(name: &str, arguments: serde_json::Value) -> Request {
            serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            }))
            .expect("request envelope")
        }

        let tools_list = crate::build_macos_registry_with_compat(false, false).tools_list();

        match admit_bootstrap_tool_call(
            &request("check_permissions", serde_json::json!({ "prompt": false })),
            &tools_list,
        ) {
            BootstrapToolCallAdmission::Ready { wait_for_grants, .. } => {
                assert!(!wait_for_grants, "permission status must remain prompt");
            }
            _ => panic!("check_permissions must be admitted"),
        }

        match admit_bootstrap_tool_call(
            &request("set_agent_cursor_enabled", serde_json::json!({ "enabled": true })),
            &tools_list,
        ) {
            BootstrapToolCallAdmission::Ready { wait_for_grants, .. } => {
                assert!(wait_for_grants, "ordinary tools retain the onboarding wait");
            }
            _ => panic!("a schema-valid tool call must be admitted"),
        }

        assert!(matches!(
            admit_bootstrap_tool_call(
                &request(
                    "type_text_chars",
                    serde_json::json!({ "pid": 1, "text": "a" }),
                ),
                &tools_list,
            ),
            BootstrapToolCallAdmission::Ready { .. }
        ));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn external_proxy_waits_for_cmux_daemon_without_launching_standalone() {
        let probes = AtomicUsize::new(0);
        let launches = AtomicUsize::new(0);

        let result = ensure_daemon_available_with(
            true,
            true,
            Duration::from_millis(100),
            Duration::from_millis(1),
            || probes.fetch_add(1, Ordering::SeqCst) >= 2,
            || {
                launches.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok(), "cmux recovered within the bounded wait");
        assert!(probes.load(Ordering::SeqCst) >= 3);
        assert_eq!(
            launches.load(Ordering::SeqCst),
            0,
            "an externally-owned proxy must never launch CuaDriver.app"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn previously_started_proxy_rechecks_daemon_health() {
        let probes = AtomicUsize::new(0);

        let result = ensure_daemon_available_with(
            true,
            true,
            Duration::from_millis(100),
            Duration::from_millis(1),
            || probes.fetch_add(1, Ordering::SeqCst) >= 1,
            || Ok(()),
        )
        .await;

        assert!(result.is_ok());
        assert!(
            probes.load(Ordering::SeqCst) >= 2,
            "started state must not bypass a fresh socket health check"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn grant_wait_policy_bypasses_only_permission_status_calls() {
        fn request(name: &str, arguments: serde_json::Value) -> Request {
            serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }))
            .expect("valid request")
        }

        assert!(!tool_call_requires_grant_wait(&request(
            "check_permissions",
            serde_json::json!({ "prompt": false }),
        )));
        assert!(!tool_call_requires_grant_wait(&request(
            "check_permissions",
            serde_json::json!({ "prompt": true }),
        )));

        for driving_tool in ["click", "move_cursor", "type_text", "scroll"] {
            assert!(
                tool_call_requires_grant_wait(&request(driving_tool, serde_json::json!({}))),
                "{driving_tool} must retain the onboarding grant wait"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn permission_status_skip_does_not_waive_later_driving_wait() {
        let mut state = DaemonStartState {
            reaper_started: true,
            grant_wait_completed: false,
            ..DaemonStartState::default()
        };

        assert!(
            !state.needs_grant_wait(false, false),
            "the first permission status call must be forwarded promptly"
        );
        assert!(
            !state.grant_wait_completed,
            "skipping the status call must leave the grant milestone pending"
        );

        assert!(
            state.needs_grant_wait(true, false),
            "the next driving call must still perform the grant wait"
        );
        state.complete_grant_wait();
        assert!(state.grant_wait_completed);
        assert!(
            !state.needs_grant_wait(true, false),
            "a completed wait should not repeat while the daemon remains healthy"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tool_list_cache_tracks_observed_daemon_generations() {
        let bootstrap = Arc::new(serde_json::json!({ "source": "bootstrap" }));
        let daemon_v1 = Arc::new(serde_json::json!({ "source": "daemon-v1" }));
        let daemon_v2 = Arc::new(serde_json::json!({ "source": "daemon-v2" }));
        let mut state = DaemonStartState::default();

        assert_eq!(state.tools_list_or(&bootstrap)["source"], "bootstrap");
        assert_eq!(state.tools_list_refresh_generation(), None);

        let first_generation = state.begin_daemon_generation();
        assert_eq!(
            state.tools_list_refresh_generation(),
            Some(first_generation),
            "a newly connected daemon needs exactly one authoritative list fetch"
        );
        assert!(state.cache_authoritative_tools_list(first_generation, daemon_v1));
        assert_eq!(state.tools_list_or(&bootstrap)["source"], "daemon-v1");
        assert_eq!(
            state.tools_list_refresh_generation(),
            None,
            "healthy calls must reuse the generation cache instead of round-tripping"
        );

        assert!(state.observe_control_connection_end(first_generation));
        assert_eq!(
            state.tools_list_or(&bootstrap)["source"],
            "bootstrap",
            "an observed outage must stop advertising the vanished daemon's schema"
        );
        assert_eq!(
            state.tools_list_refresh_generation(),
            Some(state.daemon_generation)
        );

        let second_generation = state.daemon_generation;
        assert_ne!(second_generation, first_generation);
        assert_eq!(
            state.tools_list_refresh_generation(),
            Some(second_generation),
            "a replacement daemon must refresh the authoritative contract"
        );
        assert!(state.cache_authoritative_tools_list(second_generation, daemon_v2));
        assert_eq!(state.tools_list_or(&bootstrap)["source"], "daemon-v2");
        assert_eq!(state.tools_list_refresh_generation(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stale_daemon_generation_cannot_overwrite_replacement_tool_list() {
        let bootstrap = Arc::new(serde_json::json!({ "source": "bootstrap" }));
        let stale = Arc::new(serde_json::json!({ "source": "stale-daemon" }));
        let current = Arc::new(serde_json::json!({ "source": "current-daemon" }));
        let mut state = DaemonStartState::default();

        let stale_generation = state.begin_daemon_generation();
        assert!(state.observe_control_connection_end(stale_generation));
        let current_generation = state.daemon_generation;

        assert!(!state.cache_authoritative_tools_list(stale_generation, stale));
        assert_eq!(state.tools_list_or(&bootstrap)["source"], "bootstrap");
        assert!(state.cache_authoritative_tools_list(current_generation, current));
        assert_eq!(state.tools_list_or(&bootstrap)["source"], "current-daemon");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn control_eof_detects_replacement_completed_between_requests() {
        let bootstrap = Arc::new(serde_json::json!({ "source": "bootstrap" }));
        let daemon_v1 = Arc::new(serde_json::json!({ "source": "daemon-v1" }));
        let daemon_v2 = Arc::new(serde_json::json!({ "source": "daemon-v2" }));
        let mut state = DaemonStartState::default();

        let first_generation = state.begin_daemon_generation();
        assert!(state.cache_authoritative_tools_list(first_generation, daemon_v1));
        state.complete_grant_wait();
        assert!(state.reaper_started);
        assert!(state.grant_wait_completed);

        // The helper exits and its replacement binds the same path before the
        // next MCP request. Socket reachability alone is true both before and
        // after, but EOF from the old control connection identifies the loss.
        assert!(state.observe_control_connection_end(first_generation));
        assert!(
            state.reaper_started,
            "the reconnecting control supervisor remains the session owner"
        );
        assert!(!state.grant_wait_completed);
        assert_eq!(state.tools_list_generation, None);
        assert!(state.authoritative_tools_list.is_none());
        assert_eq!(state.tools_list_or(&bootstrap)["source"], "bootstrap");

        let replacement_generation = state.daemon_generation;
        assert_ne!(replacement_generation, first_generation);
        assert_eq!(
            state.tools_list_refresh_generation(),
            Some(replacement_generation)
        );
        assert!(state.cache_authoritative_tools_list(replacement_generation, daemon_v2));
        assert_eq!(state.tools_list_or(&bootstrap)["source"], "daemon-v2");

        assert!(
            !state.observe_control_connection_end(first_generation),
            "a delayed EOF from the old control task must not invalidate the replacement"
        );
        assert_eq!(state.tools_list_or(&bootstrap)["source"], "daemon-v2");
    }
}
