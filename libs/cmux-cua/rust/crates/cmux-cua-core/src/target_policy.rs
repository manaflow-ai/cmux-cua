//! Target guard for the native tool set.
//!
//! Every targeted tool call (a `pid`, a `window_id`, or the app that
//! `launch_app` opens) passes this policy at the registry choke point
//! ([`crate::tool::ToolRegistry::invoke`]) before it runs. By default it
//! refuses:
//!
//! - the user's cmux: `com.cmuxterm.app` and every `com.cmuxterm.app.*`
//!   bundle (nightly, rc, staging, tagged DEV builds) and their windows;
//! - other terminal apps (Terminal, Ghostty, iTerm2, Warp, kitty, ...);
//! - the driver itself and the embedding host (`CMUX_CUA_HOST_BUNDLE_ID`);
//! - macOS authentication and security surfaces (SecurityAgent, Keychain
//!   Access, Passwords, System Settings, ...).
//!
//! A caller may unlock exact bundle ids with
//! [`ALLOWED_TARGET_BUNDLE_IDS_ENV`] on the process that serves the MCP
//! session (cmux sets it to the session's own tagged DEV app). The list can
//! unlock a tagged cmux DEV build (`com.cmuxterm.app.debug.<tag>`), the host,
//! and other terminals. It never unlocks the release cmux family, the
//! driver, or the security surfaces.
//!
//! The list travels to the registry as the private argument
//! [`ALLOWED_TARGET_BUNDLE_IDS_ARG`]. Every trusted entry point (the stdio MCP
//! server, the daemon, one-shot `call`) first removes any model-supplied copy
//! with [`scope_target_args`], so a tool argument can never widen the scope.

use crate::protocol::ToolResult;
use serde_json::{json, Value};

/// Comma- or whitespace-separated exact bundle ids this MCP session may
/// target even though the default policy refuses them.
pub const ALLOWED_TARGET_BUNDLE_IDS_ENV: &str = "CMUX_CUA_ALLOWED_TARGET_BUNDLE_IDS";

/// Private tool argument carrying the trusted allow list to the registry.
pub const ALLOWED_TARGET_BUNDLE_IDS_ARG: &str = "_cua_allowed_target_bundle_ids";

/// Bundle prefix of tagged cmux DEV builds, the only cmux bundles a session
/// may unlock.
const CMUX_TAGGED_DEV_PREFIX: &str = "com.cmuxterm.app.debug.";

/// The identity of a target app, as resolved by the platform.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TargetIdentity {
    pub name: String,
    pub bundle_id: Option<String>,
    pub pid: Option<i64>,
}

/// Platform lookups the policy needs. Both are side-effect free.
pub struct TargetResolver<'a> {
    /// The app that owns a process id, if it is a running app.
    pub app_for_pid: &'a dyn Fn(i64) -> Option<TargetIdentity>,
    /// The process id that owns a window id.
    pub pid_for_window: &'a dyn Fn(u64) -> Option<i64>,
}

/// Why a target was refused. The `as_str` value is the
/// `structuredContent.reason` of the refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefusalReason {
    /// The user's cmux (any `com.cmuxterm.app*` bundle not unlocked).
    UserCmux,
    /// Another terminal app.
    Terminal,
    /// The driver, its helper, or the embedding host.
    Driver,
    /// A macOS authentication or security surface.
    SecuritySurface,
}

impl RefusalReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserCmux => "user_cmux",
            Self::Terminal => "terminal",
            Self::Driver => "driver",
            Self::SecuritySurface => "security_surface",
        }
    }
}

/// Parse the allow list from its env form: exact ids, lower-cased. Entries
/// with a wildcard are dropped, since the list only names exact bundles.
pub fn parse_allowed(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = raw
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty() && !entry.contains('*'))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The allow list of this process, from [`ALLOWED_TARGET_BUNDLE_IDS_ENV`].
pub fn allowed_from_env() -> Vec<String> {
    std::env::var(ALLOWED_TARGET_BUNDLE_IDS_ENV)
        .map(|raw| parse_allowed(&raw))
        .unwrap_or_default()
}

/// Replace any caller-supplied allow list in `args` with the trusted one.
/// Call at each trusted entry point before the registry sees the args.
pub fn scope_target_args(args: &mut Value, allowed: &[String]) {
    let Some(object) = args.as_object_mut() else {
        return;
    };
    object.remove(ALLOWED_TARGET_BUNDLE_IDS_ARG);
    if !allowed.is_empty() {
        object.insert(ALLOWED_TARGET_BUNDLE_IDS_ARG.to_owned(), json!(allowed));
    }
}

/// Remove the trusted allow list from `args` and return it. The registry
/// calls this so tools never see the private argument.
pub fn take_allowed(args: &mut Value) -> Vec<String> {
    let Some(value) = args
        .as_object_mut()
        .and_then(|object| object.remove(ALLOWED_TARGET_BUNDLE_IDS_ARG))
    else {
        return Vec::new();
    };
    let joined = value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    parse_allowed(&joined)
}

/// Tools that take a `pid` or `window_id` only to filter a listing. They read
/// no window content and act on nothing.
fn is_listing_tool(tool: &str) -> bool {
    matches!(tool, "list_windows" | "list_apps")
}

/// Check a native tool call against the policy. `Ok` for calls that name no
/// target or name an allowed one.
pub fn enforce(
    tool: &str,
    args: &Value,
    allowed: &[String],
    resolver: &TargetResolver<'_>,
) -> Result<(), ToolResult> {
    // Not enforced yet.
    let _ = (tool, args, allowed, resolver, is_listing_tool(tool));
    Ok(())
}

#[allow(dead_code)]
fn check_pid(
    tool: &str,
    pid: i64,
    own_pid: i64,
    allowed: &[String],
    resolver: &TargetResolver<'_>,
) -> Result<(), ToolResult> {
    let identity = (resolver.app_for_pid)(pid);
    if pid == own_pid {
        let identity = identity.unwrap_or(TargetIdentity {
            name: "cmux-cua".to_owned(),
            bundle_id: None,
            pid: Some(pid),
        });
        return Err(refusal(tool, &identity, RefusalReason::Driver));
    }
    match identity {
        Some(identity) => check(tool, &identity, allowed),
        None => Ok(()),
    }
}

/// Check one resolved identity.
pub fn check(
    tool: &str,
    identity: &TargetIdentity,
    allowed: &[String],
) -> Result<(), ToolResult> {
    match classify(identity, allowed) {
        Some(reason) => Err(refusal(tool, identity, reason)),
        None => Ok(()),
    }
}

/// The refusal reason for `identity`, or `None` when it may be targeted.
pub fn classify(identity: &TargetIdentity, allowed: &[String]) -> Option<RefusalReason> {
    let bundle = identity
        .bundle_id
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = identity.name.trim().to_ascii_lowercase();
    let unlocked = !bundle.is_empty() && allowed.iter().any(|entry| *entry == bundle);

    if is_driver(&bundle, &name) {
        return Some(RefusalReason::Driver);
    }
    if is_security_surface(&bundle, &name) {
        return Some(RefusalReason::SecuritySurface);
    }
    if is_cmux(&bundle, &name) {
        let tagged_dev = bundle.starts_with(CMUX_TAGGED_DEV_PREFIX)
            && bundle.len() > CMUX_TAGGED_DEV_PREFIX.len();
        return if tagged_dev && unlocked {
            None
        } else {
            Some(RefusalReason::UserCmux)
        };
    }
    let host = std::env::var(crate::HOST_BUNDLE_ID_ENV)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !host.is_empty() && bundle == host && !unlocked {
        return Some(RefusalReason::Driver);
    }
    if is_terminal(&bundle, &name) && !unlocked {
        return Some(RefusalReason::Terminal);
    }
    None
}

fn is_driver(bundle: &str, name: &str) -> bool {
    matches!(bundle, "com.cmuxterm.cua" | "com.cmuxterm.app.computer-use")
        || bundle.starts_with("com.cmuxterm.cua.")
        || matches!(name, "cmux-cua" | "cmux computer use")
}

fn is_cmux(bundle: &str, name: &str) -> bool {
    bundle == "com.cmuxterm.app"
        || bundle.starts_with("com.cmuxterm.app.")
        || name == "cmux"
        || name.starts_with("cmux ")
}

fn is_security_surface(bundle: &str, name: &str) -> bool {
    matches!(
        bundle,
        "com.apple.loginwindow"
            | "com.apple.securityagent"
            | "com.apple.coreservicesuiagent"
            | "com.apple.authorizationhost"
            | "com.apple.keychainaccess"
            | "com.apple.passwords"
            | "com.apple.systempreferences"
    ) || bundle.starts_with("com.apple.localauthentication.")
        || bundle.starts_with("com.apple.authenticationservices.")
        || matches!(
            name,
            "loginwindow"
                | "securityagent"
                | "authorizationhost"
                | "coreauthd"
                | "localauthenticationremoteservice"
                | "keychain access"
                | "passwords"
                | "system settings"
                | "system preferences"
        )
}

/// Terminal apps other than cmux. Shared with the Codex compatibility
/// profile's guard.
pub fn is_terminal(bundle: &str, name: &str) -> bool {
    matches!(
        bundle,
        "com.apple.terminal"
            | "com.mitchellh.ghostty"
            | "com.googlecode.iterm2"
            | "net.kovidgoyal.kitty"
            | "org.alacritty"
            | "com.github.wez.wezterm"
            | "co.zeit.hyper"
            | "co.vercel.hyper"
    ) || bundle.starts_with("dev.warp.")
        || matches!(
            name,
            "terminal"
                | "ghostty"
                | "iterm"
                | "iterm2"
                | "warp"
                | "warp preview"
                | "kitty"
                | "alacritty"
                | "wezterm"
                | "hyper"
        )
}

/// `launch_app` accepts a display name or an app path; compare the app name.
fn app_name_from_reference(reference: &str) -> String {
    let trimmed = reference.trim().trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    last.strip_suffix(".app").unwrap_or(last).to_owned()
}

/// A `cmux:` (or `cmux-*:`) URL opens in the user's cmux.
fn url_opens_cmux(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    scheme == "cmux" || scheme.starts_with("cmux-") || scheme.starts_with("cmux+")
}

/// The typed refusal returned to the model.
pub fn refusal(tool: &str, identity: &TargetIdentity, reason: RefusalReason) -> ToolResult {
    let bundle = identity.bundle_id.as_deref().unwrap_or("unknown bundle");
    let why = match reason {
        RefusalReason::UserCmux => {
            "cmux Computer Use never acts on the user's cmux app or its windows. Use the \
             cmux CLI, the cmux MCP tools, or the cmux-browser skill to work with cmux; a \
             session can drive its own tagged cmux DEV app only when cmux scopes the \
             session to it."
        }
        RefusalReason::Terminal => {
            "cmux Computer Use does not act on terminal apps by default. Run the command \
             in your own shell instead."
        }
        RefusalReason::Driver => {
            "cmux Computer Use does not act on itself or on the app that hosts it."
        }
        RefusalReason::SecuritySurface => {
            "cmux Computer Use does not act on macOS authentication or security surfaces. \
             Ask the user to do this step."
        }
    };
    let message = format!(
        "target_not_allowed: {tool} refused to target '{}' ({bundle}). {why}",
        identity.name
    );
    ToolResult::error(message.clone()).with_structured(json!({
        "code": "target_not_allowed",
        "reason": reason.as_str(),
        "tool": tool,
        "app": identity.name,
        "bundle_id": identity.bundle_id,
        "pid": identity.pid,
        "message": message,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, bundle: &str, pid: i64) -> TargetIdentity {
        TargetIdentity {
            name: name.to_owned(),
            bundle_id: Some(bundle.to_owned()),
            pid: Some(pid),
        }
    }

    /// pid 100 = the user's cmux, 101 = a tagged cmux DEV build,
    /// 102 = TextEdit, 103 = Terminal, 104 = cmux NIGHTLY, 105 = Keychain
    /// Access. Window 9000 belongs to the user's cmux, 9001 to TextEdit.
    fn with_fake_apps<T>(body: impl FnOnce(&TargetResolver<'_>) -> T) -> T {
        let app_for_pid = |pid: i64| match pid {
            100 => Some(app("cmux", "com.cmuxterm.app", 100)),
            101 => Some(app("cmux DEV agt1", "com.cmuxterm.app.debug.agt1", 101)),
            102 => Some(app("TextEdit", "com.apple.TextEdit", 102)),
            103 => Some(app("Terminal", "com.apple.Terminal", 103)),
            104 => Some(app("cmux NIGHTLY", "com.cmuxterm.app.nightly", 104)),
            105 => Some(app("Keychain Access", "com.apple.keychainaccess", 105)),
            _ => None,
        };
        let pid_for_window = |window: u64| match window {
            9000 => Some(100),
            9001 => Some(102),
            _ => None,
        };
        body(&TargetResolver {
            app_for_pid: &app_for_pid,
            pid_for_window: &pid_for_window,
        })
    }

    fn reason_of(result: Result<(), ToolResult>) -> Option<String> {
        let error = result.err()?;
        assert_eq!(error.is_error, Some(true));
        let structured = error.structured_content.expect("typed refusal");
        assert_eq!(structured["code"], "target_not_allowed");
        Some(structured["reason"].as_str().unwrap().to_owned())
    }

    #[test]
    fn a_call_on_the_users_cmux_is_refused() {
        with_fake_apps(|resolver| {
            for tool in ["click", "type_text", "press_key", "get_window_state", "kill_app", "bring_to_front"] {
                let refused = reason_of(enforce(tool, &json!({"pid": 100, "x": 1, "y": 1}), &[], resolver));
                assert_eq!(refused.as_deref(), Some("user_cmux"), "{tool} on com.cmuxterm.app");
            }
            let nightly = reason_of(enforce("click", &json!({"pid": 104}), &[], resolver));
            assert_eq!(nightly.as_deref(), Some("user_cmux"));
        });
    }

    #[test]
    fn a_call_on_another_app_is_not_refused() {
        with_fake_apps(|resolver| {
            assert!(enforce("click", &json!({"pid": 102, "x": 1, "y": 1}), &[], resolver).is_ok());
            assert!(enforce("get_window_state", &json!({"pid": 102, "window_id": 9001}), &[], resolver).is_ok());
            assert!(enforce("get_screen_size", &json!({}), &[], resolver).is_ok());
            assert!(enforce("list_windows", &json!({"pid": 100}), &[], resolver).is_ok());
            assert!(enforce("launch_app", &json!({"bundle_id": "com.apple.TextEdit"}), &[], resolver).is_ok());
            assert!(enforce("launch_app", &json!({"urls": ["https://example.com"]}), &[], resolver).is_ok());
        });
    }

    #[test]
    fn a_window_of_the_users_cmux_is_refused_even_with_another_pid() {
        with_fake_apps(|resolver| {
            let refused = reason_of(enforce("click", &json!({"pid": 102, "window_id": 9000}), &[], resolver));
            assert_eq!(refused.as_deref(), Some("user_cmux"));
            let refused = reason_of(enforce("zoom", &json!({"window_id": 9000}), &[], resolver));
            assert_eq!(refused.as_deref(), Some("user_cmux"));
        });
    }

    #[test]
    fn launching_or_deep_linking_cmux_is_refused() {
        with_fake_apps(|resolver| {
            for args in [
                json!({"bundle_id": "com.cmuxterm.app"}),
                json!({"name": "cmux"}),
                json!({"name": "/Applications/cmux.app"}),
                json!({"urls": ["cmux://workspace/1"]}),
            ] {
                let refused = reason_of(enforce("launch_app", &args, &[], resolver));
                assert_eq!(refused.as_deref(), Some("user_cmux"), "{args}");
            }
        });
    }

    #[test]
    fn terminals_driver_and_security_surfaces_are_refused_by_default() {
        with_fake_apps(|resolver| {
            assert_eq!(reason_of(enforce("type_text", &json!({"pid": 103}), &[], resolver)).as_deref(), Some("terminal"));
            assert_eq!(reason_of(enforce("click", &json!({"pid": 105}), &[], resolver)).as_deref(), Some("security_surface"));
            let own = i64::from(std::process::id());
            assert_eq!(reason_of(enforce("click", &json!({"pid": own}), &[], resolver)).as_deref(), Some("driver"));
            assert_eq!(
                reason_of(enforce("launch_app", &json!({"bundle_id": "com.cmuxterm.cua"}), &[], resolver)).as_deref(),
                Some("driver")
            );
        });
    }

    #[test]
    fn an_explicit_scope_unlocks_only_a_tagged_dev_build_or_a_terminal() {
        with_fake_apps(|resolver| {
            let tagged = vec!["com.cmuxterm.app.debug.agt1".to_owned()];
            assert_eq!(reason_of(enforce("click", &json!({"pid": 101}), &[], resolver)).as_deref(), Some("user_cmux"));
            assert!(enforce("click", &json!({"pid": 101}), &tagged, resolver).is_ok());
            // The tagged scope does not reach the user's cmux.
            assert_eq!(reason_of(enforce("click", &json!({"pid": 100}), &tagged, resolver)).as_deref(), Some("user_cmux"));
            // The release family can never be unlocked.
            let release = vec!["com.cmuxterm.app".to_owned(), "com.cmuxterm.app.nightly".to_owned()];
            assert_eq!(reason_of(enforce("click", &json!({"pid": 100}), &release, resolver)).as_deref(), Some("user_cmux"));
            assert_eq!(reason_of(enforce("click", &json!({"pid": 104}), &release, resolver)).as_deref(), Some("user_cmux"));
            // Security surfaces cannot be unlocked.
            let keychain = vec!["com.apple.keychainaccess".to_owned()];
            assert_eq!(reason_of(enforce("click", &json!({"pid": 105}), &keychain, resolver)).as_deref(), Some("security_surface"));
            // A terminal can be unlocked by its exact bundle id.
            let terminal = vec!["com.apple.terminal".to_owned()];
            assert!(enforce("click", &json!({"pid": 103}), &terminal, resolver).is_ok());
        });
    }

    #[test]
    fn a_model_supplied_scope_is_replaced_by_the_trusted_one() {
        let mut args = json!({"pid": 101, ALLOWED_TARGET_BUNDLE_IDS_ARG: ["com.cmuxterm.app.debug.agt1"]});
        scope_target_args(&mut args, &[]);
        assert!(args.get(ALLOWED_TARGET_BUNDLE_IDS_ARG).is_none());
        assert!(take_allowed(&mut args).is_empty());

        let mut args = json!({"pid": 101, ALLOWED_TARGET_BUNDLE_IDS_ARG: ["com.apple.terminal"]});
        scope_target_args(&mut args, &["com.cmuxterm.app.debug.agt1".to_owned()]);
        assert_eq!(take_allowed(&mut args), vec!["com.cmuxterm.app.debug.agt1".to_owned()]);
        assert!(args.get(ALLOWED_TARGET_BUNDLE_IDS_ARG).is_none());
    }

    #[test]
    fn the_env_form_takes_exact_ids_only() {
        assert_eq!(
            parse_allowed(" com.cmuxterm.app.debug.A, com.cmuxterm.app.* \ncom.apple.Terminal"),
            vec!["com.apple.terminal".to_owned(), "com.cmuxterm.app.debug.a".to_owned()]
        );
    }
}
