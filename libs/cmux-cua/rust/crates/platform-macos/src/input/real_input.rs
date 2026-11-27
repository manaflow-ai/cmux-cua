//! The one gate between an agent and the person's real input devices.
//!
//! cmux Computer Use drives apps in the background: AX actions and per-pid
//! events (`SLEventPostToPid` / `CGEventPostToPid`). The branded agent cursor
//! is an overlay window only. Anything that instead takes the person's own
//! pointer or keyboard focus (warping the hardware cursor, posting to the
//! global HID tap, fronting an app) is "real input", and it is refused unless
//! the person turned it on by setting `CMUX_CUA_ALLOW_REAL_INPUT=1` in the
//! helper's environment. The refusal names the reason and the way to override.
//!
//! Every function that warps the cursor, posts to a global tap or activates an
//! app calls [`require`] (or [`allowed`]); `tests::real_input_calls_are_gated`
//! fails the build when one does not.

/// Env var the person sets to let the helper use their real pointer/focus.
pub const ENV: &str = "CMUX_CUA_ALLOW_REAL_INPUT";

fn truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// True only when the person explicitly allowed real-input delivery.
pub fn allowed() -> bool {
    truthy(std::env::var(ENV).ok().as_deref())
}

/// The refusal text for `what`.
pub fn refusal(what: &str) -> String {
    format!(
        "foreground_not_permitted: {what} would take the person's real mouse pointer or \
         keyboard focus, so cmux Computer Use refuses it. Use element_index/element_token \
         clicks or window-addressed x,y with delivery_mode \"background\" (the default). \
         To allow it anyway, the person sets {ENV}=1 for the helper."
    )
}

/// `Ok(())` when real input is allowed, else the refusal as an error.
pub fn require(what: &str) -> anyhow::Result<()> {
    if allowed() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(refusal(what)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn denied_by_default_and_only_explicit_values_allow() {
        assert!(!truthy(None));
        assert!(!truthy(Some("")));
        assert!(!truthy(Some("0")));
        assert!(!truthy(Some("foreground")));
        assert!(truthy(Some("1")));
        assert!(truthy(Some("True")));
        assert!(refusal("x").contains("foreground_not_permitted"));
        assert!(refusal("x").contains(ENV));
    }

    #[test]
    fn requiring_real_input_fails_without_the_env() {
        // The test process never sets the env; the gate must refuse.
        if std::env::var(ENV).is_err() {
            let e = require("a desktop-scope click").unwrap_err().to_string();
            assert!(e.starts_with("foreground_not_permitted"), "{e}");
        }
    }

    /// Tokens that take the person's pointer, a global event tap or app focus.
    const BANNED: &[&str] = &[
        "warp_mouse_cursor_position",
        "CGWarpMouseCursorPosition",
        "CGAssociateMouseAndMouseCursorPosition(true)",
        "CGEventTapLocation::HID",
        "CGEventTapLocation::Session",
        "CGEventTapLocation::AnnotatedSession",
        "kCGHIDEventTap",
        "kCGSessionEventTap",
        "activateWithOptions",
        "activateIgnoringOtherApps",
    ];

    /// (file suffix, fn name) pairs that may use a banned token without the
    /// gate: they restore focus the target stole, or show the helper's own
    /// permission panel. None of them takes the pointer.
    const ALLOWED_UNGATED: &[(&str, &str)] = &[
        ("apps/mod.rs", "activate_pid"),
        ("focus_steal.rs", "restore_focus"),
        ("focus_steal.rs", "reactivate"),
        ("permissions/panel.rs", "show_modal_unsafe"),
    ];

    fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// Name and text of the fn enclosing line `idx` (indentation-based).
    fn enclosing_fn(lines: &[&str], idx: usize) -> Option<(String, String)> {
        let mut start = idx;
        loop {
            let l = lines[start];
            let t = l.trim_start();
            if (t.starts_with("fn ") || t.contains(" fn ")) && t.contains('(') {
                break;
            }
            if start == 0 {
                return None;
            }
            start -= 1;
        }
        let indent = lines[start].len() - lines[start].trim_start().len();
        let mut end = start + 1;
        while end < lines.len() {
            let l = lines[end];
            if l.trim() == "}" && l.len() - l.trim_start().len() == indent {
                break;
            }
            end += 1;
        }
        let head = lines[start];
        let after = head.split("fn ").nth(1)?;
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let body = lines[start..=end.min(lines.len() - 1)].join("\n");
        Some((name, body))
    }

    /// Fails if any code path warps the cursor, posts to a global tap or
    /// activates an app without going through the real-input gate.
    #[test]
    fn real_input_calls_are_gated() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&root, &mut files);
        let mut bad = Vec::new();
        for f in files {
            let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            if rel == "input/real_input.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&f).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                let code = l.split("//").next().unwrap_or("");
                if !BANNED.iter().any(|b| code.contains(b)) {
                    continue;
                }
                match enclosing_fn(&lines, i) {
                    Some((name, body)) => {
                        let gated = body.contains("real_input::require(")
                            || body.contains("real_input::allowed()");
                        let allow = ALLOWED_UNGATED
                            .iter()
                            .any(|(s, n)| rel.ends_with(s) && *n == name);
                        if !gated && !allow {
                            bad.push(format!("{rel}:{}: fn {name}: `{}`", i + 1, l.trim()));
                        }
                    }
                    None => bad.push(format!("{rel}:{}: outside a fn: `{}`", i + 1, l.trim())),
                }
            }
        }
        assert!(
            bad.is_empty(),
            "these call sites take the person's real pointer/focus without \
             crate::input::real_input::require/allowed:\n{}",
            bad.join("\n")
        );
    }
}
