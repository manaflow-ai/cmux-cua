//! AX tree walker: produces the treeMarkdown string and element cache.
//!
//! Format (matching libs/cmux-cua exactly):
//!   `INDENT- [N] AXRole "Title" [value="..." actions=[...]]`
//!   `INDENT- AXStaticText = "value"`  (non-indexed)
//!
//! Rules (from cmux-cua reference):
//! - An element is "actionable" (gets an index) when it has ≥1 action name.
//! - Non-actionable leaf nodes with a value are rendered as `AXRole = "value"`.
//! - AXStaticText with no title/value is omitted.
//! - Tree is walked depth-first; element_index is assigned in DFS order.

use super::bindings::*;
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};
use std::collections::HashSet;

/// Default maximum depth for AX tree walks. Deep menus and complex web views
/// can nest deeply; 25 covers realistic app chrome without exploding on
/// pathological trees (mirrors Swift reference implementation).
///
/// Callers can override per-call via `walk_tree`'s `max_depth` parameter to
/// trade fidelity for context-window budget on AX-heavy apps (Electron,
/// Obsidian, large web apps — issue #22865).
pub const DEFAULT_MAX_DEPTH: usize = 25;

/// Default maximum total nodes visited during a single AX walk. Chromium-family
/// apps (Arc, VS Code, Chrome) can expose thousands of nodes; capping at 2 000
/// keeps the walk bounded while still covering realistic app chrome.
/// When the cap is hit the walk stops early and the partial tree is returned
/// with a warning line appended (mirrors Swift reference implementation).
///
/// Callers can override per-call via `walk_tree`'s `max_elements` parameter
/// (issue #22865).
pub const DEFAULT_MAX_ELEMENTS: usize = 2_000;

/// Default cooperative wall-time budget for one AX snapshot. A single AX IPC
/// call can still run until the system messaging timeout, but the walker stops
/// traversing as soon as it regains control after this deadline.
pub const DEFAULT_MAX_AX_TIME_MS: u64 = 2_000;

const MAX_AX_MESSAGE_TIMEOUT_SECONDS: f32 = 0.1;
const MIN_AX_MESSAGE_TIMEOUT_SECONDS: f32 = 0.001;

fn messaging_timeout_seconds(remaining: std::time::Duration) -> f32 {
    remaining.as_secs_f32().clamp(
        MIN_AX_MESSAGE_TIMEOUT_SECONDS,
        MAX_AX_MESSAGE_TIMEOUT_SECONDS,
    )
}

unsafe fn apply_messaging_timeout(element: AXUIElementRef, deadline: Option<std::time::Instant>) {
    if let Some(limit) = deadline {
        let remaining = limit.saturating_duration_since(std::time::Instant::now());
        let _ = AXUIElementSetMessagingTimeout(element, messaging_timeout_seconds(remaining));
    }
}

/// Maximum number of Unicode scalar values emitted for one AXValue. Large
/// document/text-area values otherwise duplicate entire documents into both
/// the structured array and Markdown tree.
pub const MAX_AX_VALUE_CHARS: usize = 512;

#[derive(Default)]
struct TopLevelWindowTracker {
    window_ids: HashSet<u32>,
    element_ptrs: HashSet<usize>,
}

impl TopLevelWindowTracker {
    /// Returns `true` only for a top-level window not already represented by
    /// the same retained AX reference or stable CGWindowID.
    fn insert(&mut self, element_ptr: usize, window_id: Option<u32>) -> bool {
        if !self.element_ptrs.insert(element_ptr) {
            return false;
        }
        match window_id {
            Some(window_id) => self.window_ids.insert(window_id),
            None => true,
        }
    }

    fn insert_with_existing(
        &mut self,
        element: AXUIElementRef,
        existing: &[AXUIElementRef],
        window_id: Option<u32>,
    ) -> bool {
        if existing
            .iter()
            .any(|other| unsafe { CFEqual(*other as CFTypeRef, element as CFTypeRef) != 0 })
        {
            return false;
        }
        self.insert(element as usize, window_id)
    }
}

pub(crate) fn truncate_ax_value(value: String) -> String {
    if value.chars().count() <= MAX_AX_VALUE_CHARS {
        return value;
    }
    let mut truncated: String = value.chars().take(MAX_AX_VALUE_CHARS).collect();
    truncated.push('…');
    truncated
}

/// Human-facing label shared by the Markdown and structured output. Preserve
/// the established title-first order used by structured consumers.
pub(crate) fn preferred_label<'a>(
    title: Option<&'a str>,
    description: Option<&'a str>,
    value: Option<&'a str>,
    identifier: Option<&'a str>,
) -> Option<&'a str> {
    title.or(description).or(value).or(identifier)
}

/// Enable the lazy web-content AX tree for an Electron/Chromium application.
/// This is intentionally a no-op for every other bundle and is shared by
/// `launch_app` and the first AX snapshot so both attach paths have identical
/// behavior. Returns true only when an AX attribute write was accepted.
pub(crate) fn enable_chromium_accessibility_for_pid(pid: i32) -> bool {
    if !crate::apps::has_chromium_framework(pid) {
        return false;
    }

    let enabled = unsafe {
        let app_elem = AXUIElementCreateApplication(pid);
        if app_elem.is_null() {
            false
        } else {
            super::enablement::ensure_chromium_ax_enabled(pid, app_elem);
            CFRelease(app_elem as CFTypeRef);
            true
        }
    };
    enabled
}

/// A single node in the AX tree.
#[derive(Debug, Clone)]
pub struct AXNode {
    /// 0-based addressable index. Native walks index actionable nodes; Codex
    /// compatibility walks also index meaningful text and containers.
    pub element_index: Option<usize>,
    pub role: String,
    /// AXTitle — shown as `"title"` in the tree line.
    pub title: Option<String>,
    /// AXValue — shown as `= "value"` in the tree line.
    pub value: Option<String>,
    /// AXDescription — shown as `(description)` in the tree line.
    /// Kept separate from `title` so `_find_calc_button("2")` can find
    /// Calculator buttons where AXTitle="" but AXDescription="2".
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub help: Option<String>,
    pub actions: Vec<String>,
    /// The raw AXUIElementRef pointer value, for caching.
    pub element_ptr: usize,
    /// Depth in the rendered markdown tree (matches the indent level used in
    /// `tree_markdown`). Layout containers AXScrollArea/AXGroup collapse so
    /// children share the parent's depth.
    pub depth: usize,
    /// `element_index` of the nearest actionable ancestor, if any. Walks the
    /// rendered tree (so it skips collapsed layout containers).
    pub parent_element_index: Option<usize>,
    /// Screen-coordinate bounding rect `[x, y, width, height]` captured at
    /// walk time. `None` when AX didn't report a usable position+size.
    pub frame: Option<[f64; 4]>,
}

pub struct TreeWalkResult {
    pub tree_markdown: String,
    pub nodes: Vec<AXNode>,
    /// True when the walk was cut short by the MAX_ELEMENTS cap.
    pub truncated: bool,
    /// Why the walk was truncated, when it was partial. This is kept as a
    /// string so tool responses can expose stable machine-readable metadata.
    pub truncation_reason: Option<String>,
    /// Number of AX nodes inspected before returning.
    pub nodes_visited: usize,
    /// Elapsed wall time spent in the cooperative walk, in milliseconds.
    pub elapsed_ms: u64,
    /// Retains every emitted AX element until the result is dropped. Cache
    /// publishers must take their own retain before dropping this result.
    pub(crate) owned_elements: RetainedNodeGuard,
    /// Retained traversal roots, including collapsed AXGroup wrappers. These
    /// are observer targets even when compact serialization omits them.
    pub(crate) watch_elements: RetainedNodeGuard,
}

/// Ownership guard for AX node references emitted by a walk.
#[derive(Debug)]
pub struct RetainedNodeGuard(Vec<usize>);

impl TreeWalkResult {
    /// Take a separate retain for a cache/observer that outlives this result.
    pub fn retain_nodes(&self) -> RetainedNodeGuard {
        RetainedNodeGuard::retain_nodes(&self.nodes)
    }

    pub(crate) fn retain_watch_elements(&self) -> RetainedNodeGuard {
        self.watch_elements.clone_retained()
    }
}

impl RetainedNodeGuard {
    pub(crate) fn empty() -> Self {
        Self(Vec::new())
    }

    fn from_walk(nodes: &[AXNode]) -> Self {
        Self(
            nodes
                .iter()
                .map(|node| node.element_ptr)
                .filter(|ptr| *ptr != 0)
                .collect(),
        )
    }

    /// Take ownership of references already retained by the traversal.
    fn from_owned_ptrs(ptrs: &[usize]) -> Self {
        Self(ptrs.iter().copied().filter(|ptr| *ptr != 0).collect())
    }

    /// Duplicate ownership for a cache or observer that outlives the walk.
    pub fn retain_nodes(nodes: &[AXNode]) -> Self {
        let ptrs = nodes
            .iter()
            .map(|node| node.element_ptr)
            .filter(|ptr| *ptr != 0)
            .collect::<Vec<_>>();
        for ptr in &ptrs {
            unsafe { CFRetain(*ptr as AXUIElementRef as CFTypeRef) };
        }
        Self(ptrs)
    }

    pub(crate) fn append(&mut self, mut other: RetainedNodeGuard) {
        for ptr in other.0.drain(..) {
            if !self.0.contains(&ptr) {
                self.0.push(ptr);
            } else if ptr != 0 {
                unsafe { CFRelease(ptr as AXUIElementRef as CFTypeRef) };
            }
        }
    }

    pub(crate) fn clone_retained(&self) -> Self {
        for ptr in &self.0 {
            if *ptr != 0 {
                unsafe { CFRetain(*ptr as AXUIElementRef as CFTypeRef) };
            }
        }
        Self(self.0.clone())
    }

    pub(crate) fn pointers(&self) -> &[usize] {
        &self.0
    }
}

impl Drop for RetainedNodeGuard {
    fn drop(&mut self) {
        for ptr in &self.0 {
            unsafe { CFRelease(*ptr as AXUIElementRef as CFTypeRef) };
        }
    }
}

/// Walk the AX tree of `pid`, optionally filtered to a specific window.
///
/// `window_id` — when Some, only the AXWindow matching that CGWindowID is
/// walked (plus non-window children like the menu bar). When None, all
/// top-level children are walked.
///
/// Key background-app fix: at the application root we union `AXChildren`
/// and `AXWindows`. macOS only puts windows in `AXChildren` when the app
/// is frontmost; `AXWindows` returns the window list regardless of focus
/// state. Without this union, Safari / any backgrounded app returns an
/// empty tree.
///
/// # Safety
/// Calls macOS AX API. Must be called on a thread that has a CF run loop.
pub fn walk_tree(pid: i32, window_id: Option<u32>, query: Option<&str>) -> TreeWalkResult {
    walk_tree_bounded(
        pid,
        window_id,
        query,
        DEFAULT_MAX_ELEMENTS,
        DEFAULT_MAX_DEPTH,
    )
}

/// Walk the AX tree with caller-supplied caps. See [`walk_tree`] for the
/// common case (defaults apply). `max_elements`/`max_depth` clamp the
/// rendered tree breadth-wise (DFS truncated when the element counter hits
/// the cap) and depth-wise (nodes whose markdown indent would exceed the cap
/// are omitted). Markdown and the `nodes` vec are truncated identically.
///
/// Issue #22865: caps protect against Electron / Obsidian / large web apps
/// that produce 10k+ element trees and blow context windows.
pub fn walk_tree_bounded(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_elements: usize,
    max_depth: usize,
) -> TreeWalkResult {
    walk_tree_bounded_with_timeout(pid, window_id, query, max_elements, max_depth, None)
}

/// Walk an AX tree with count, depth, and optional cooperative time caps.
pub fn walk_tree_bounded_with_timeout(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_elements: usize,
    max_depth: usize,
    max_ax_time_ms: Option<u64>,
) -> TreeWalkResult {
    walk_tree_bounded_with_mode(
        pid,
        window_id,
        query,
        max_elements,
        max_depth,
        max_ax_time_ms,
        WalkMode::Native,
    )
}

/// Codex Computer Use compatibility requires a complete addressable map rather
/// than the native action-only map. This variant preserves meaningful layout
/// containers and assigns indices to display text so scroll, selection, and
/// secondary actions can target the same rows the v829 surface exposes.
/// Native callers continue through [`walk_tree_bounded`] unchanged.
pub fn walk_tree_bounded_full_map(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_elements: usize,
    max_depth: usize,
) -> TreeWalkResult {
    walk_tree_bounded_full_map_with_timeout(pid, window_id, query, max_elements, max_depth, None)
}

/// Codex compatibility walk with the same cooperative time budget as the
/// native action-only walk.
pub fn walk_tree_bounded_full_map_with_timeout(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_elements: usize,
    max_depth: usize,
    max_ax_time_ms: Option<u64>,
) -> TreeWalkResult {
    walk_tree_bounded_with_mode(
        pid,
        window_id,
        query,
        max_elements,
        max_depth,
        max_ax_time_ms,
        WalkMode::CodexFull,
    )
}

/// Walk one already-retained AX element without resolving an application root.
/// Cache refreshers use this for observer-marked descendants so a value or
/// structural notification need not rebuild the entire window snapshot. The
/// caller retains `root` for the duration; actionable descendants receive the
/// same cache retain used by a full walk.
pub unsafe fn walk_subtree(
    root: AXUIElementRef,
    max_elements: usize,
    max_depth: usize,
) -> TreeWalkResult {
    walk_subtree_with_options(
        root,
        max_elements,
        max_depth,
        true,
        None,
        Some(DEFAULT_MAX_AX_TIME_MS),
    )
}

/// Walk a retained subtree with the same map mode and viewport pruning policy
/// as its owning window snapshot.
pub unsafe fn walk_subtree_with_options(
    root: AXUIElementRef,
    max_elements: usize,
    max_depth: usize,
    full_map: bool,
    visible_bounds: Option<[f64; 4]>,
    max_ax_time_ms: Option<u64>,
) -> TreeWalkResult {
    let started_at = std::time::Instant::now();
    let mut nodes = Vec::new();
    let mut lines = Vec::new();
    let mut counter = 0;
    let mut visited = 0;
    let mut truncated = false;
    let mut reason = None;
    let mut watch_ptrs = Vec::new();
    let mut seen = Vec::new();
    let deadline = max_ax_time_ms
        .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms.max(1)));
    walk_element(
        root,
        0,
        None,
        &mut nodes,
        &mut lines,
        &mut counter,
        &mut visited,
        &mut truncated,
        &mut reason,
        max_elements,
        max_depth,
        deadline,
        if full_map {
            WalkMode::CodexFull
        } else {
            WalkMode::Native
        },
        visible_bounds,
        &mut watch_ptrs,
        &mut seen,
    );
    let owned_elements = RetainedNodeGuard::from_walk(&nodes);
    TreeWalkResult {
        tree_markdown: render_lines(&lines),
        nodes,
        truncated,
        truncation_reason: reason,
        nodes_visited: visited,
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        owned_elements,
        watch_elements: RetainedNodeGuard::from_owned_ptrs(&watch_ptrs),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WalkMode {
    Native,
    CodexFull,
}

fn walk_tree_bounded_with_mode(
    pid: i32,
    window_id: Option<u32>,
    query: Option<&str>,
    max_elements: usize,
    max_depth: usize,
    max_ax_time_ms: Option<u64>,
    mode: WalkMode,
) -> TreeWalkResult {
    let started_at = std::time::Instant::now();
    let mut deadline = None;
    let mut nodes: Vec<AXNode> = Vec::new();
    let mut lines: Vec<(usize, String)> = Vec::new(); // (depth, line)
    let mut index_counter = 0usize;
    // Shared visited-node counter passed into walk_element to enforce the cap.
    let mut visited_count = 0usize;
    // Set to true only when walk_element actually stops early due to the cap —
    // avoids a false-positive when the tree naturally ends on exactly the cap.
    let mut truncated = false;
    let mut truncation_reason: Option<String> = None;
    let mut watch_ptrs = Vec::new();
    let mut seen = Vec::new();

    unsafe {
        let app_elem = AXUIElementCreateApplication(pid);
        if app_elem.is_null() {
            return TreeWalkResult {
                tree_markdown: String::new(),
                nodes,
                truncated: false,
                truncation_reason: None,
                nodes_visited: 0,
                elapsed_ms: started_at.elapsed().as_millis() as u64,
                owned_elements: RetainedNodeGuard(Vec::new()),
                watch_elements: RetainedNodeGuard(Vec::new()),
            };
        }

        // Chromium/Electron apps (Arc, VS Code, Electron shells) ship their
        // web-content AX tree OFF and only build it once an assistive client
        // asks for it. Without this, the first walk of such an app returns an
        // empty/title-bar-only tree (#1616). Flip the enablement attribute,
        // then — only when the flip actually took and only the first time we
        // see this pid — let the asynchronously-built tree settle before we
        // read it. Native Cocoa apps reject the attribute, so they pay no
        // settle cost. This relies on the MAX_ELEMENTS node cap to keep the
        // now-materialized (potentially large) tree bounded.
        // Only Electron/Chromium bundles lazily materialize their web AX tree.
        // Native applications reject these attributes and should not pay a
        // settle delay or receive Chromium-specific AX state.
        if crate::apps::has_chromium_framework(pid) {
            super::enablement::ensure_chromium_ax_enabled(pid, app_elem);
        }

        // The cooperative walk budget starts after Chromium materialization;
        // attach may legitimately wait for the web area to appear.
        deadline = max_ax_time_ms.map(|ms| {
            std::time::Instant::now() + std::time::Duration::from_millis(ms.clamp(1, 60_000))
        });

        apply_messaging_timeout(app_elem, deadline);

        // Union AXChildren + AXWindows — the only way to see background windows.
        // AXChildren omits windows when the app isn't frontmost (AppKit limitation).
        // AXWindows returns the window list regardless of activation state.
        let from_children = copy_children(app_elem);
        let from_windows = copy_ax_windows(app_elem);

        let mut top_level = from_children;
        let mut top_level_windows = TopLevelWindowTracker::default();
        for element in &top_level {
            top_level_windows.insert(*element as usize, ax_get_window_id(*element));
        }
        for w in from_windows {
            // `AXChildren` and `AXWindows` can return distinct AX wrapper
            // objects for the same window. Deduplicate only by retained
            // reference or stable CGWindowID; visual attributes are not
            // identity and may legitimately match across separate controls.
            if top_level_windows.insert_with_existing(w, &top_level, ax_get_window_id(w)) {
                top_level.push(w);
            } else {
                // Already present — release the extra retain from copy_ax_windows.
                CFRelease(w as CFTypeRef);
            }
        }

        // Electron can expose the focused renderer proxy only through the
        // application's focused-element attribute. Add it as a traversal
        // root for desktop-wide walks, deduplicating by AX identity.
        let focused_root = copy_element_attr(app_elem, "AXFocusedUIElement").and_then(|element| {
            if window_id.is_none() || element_owning_window_id(element) == window_id {
                Some(element)
            } else {
                CFRelease(element as CFTypeRef);
                None
            }
        });

        // Window-scoped snapshots are screenshot-scoped in both modes:
        // walking a menu bar or another app-level child adds irrelevant AX
        // calls, destabilizes numeric indices, and can consume the response
        // budget before the target window's controls. A desktop-wide walk
        // (window_id == None) still includes all app-root children.
        let mut walk_these: Vec<AXUIElementRef> = if let Some(wid) = window_id {
            top_level
                .iter()
                .copied()
                .filter(|&child| {
                    apply_messaging_timeout(child, deadline);
                    let role = copy_string_attr(child, "AXRole").unwrap_or_default();
                    should_walk_top_level(&role, ax_get_window_id(child), Some(wid), mode)
                })
                .collect()
        } else {
            top_level.iter().copied().collect()
        };
        let visible_bounds = window_id.and_then(|wid| {
            walk_these
                .iter()
                .find(|element| ax_get_window_id(**element) == Some(wid))
                .and_then(|element| element_screen_rect(*element))
        });
        if let Some(focused) = focused_root {
            if !walk_these
                .iter()
                .any(|other| CFEqual(*other as CFTypeRef, focused as CFTypeRef) != 0)
            {
                walk_these.insert(0, focused);
            } else {
                CFRelease(focused as CFTypeRef);
            }
        }
        prioritize_children(&mut walk_these, deadline);

        // Walk each top-level child at depth 0.
        for child in walk_these {
            walk_element(
                child,
                0,
                None,
                &mut nodes,
                &mut lines,
                &mut index_counter,
                &mut visited_count,
                &mut truncated,
                &mut truncation_reason,
                max_elements,
                max_depth,
                deadline,
                mode,
                visible_bounds,
                &mut watch_ptrs,
                &mut seen,
            );
            if !top_level
                .iter()
                .any(|other| CFEqual(*other as CFTypeRef, child as CFTypeRef) != 0)
            {
                CFRelease(child as CFTypeRef);
            }
        }

        // Release all top-level elements (copy_children / copy_ax_windows both retain).
        for child in top_level {
            CFRelease(child as CFTypeRef);
        }

        CFRelease(app_elem as CFTypeRef);
    }

    let truncated_flag = truncated;
    let raw_markdown = render_lines(&lines);
    let mut tree_markdown = if let Some(q) = query {
        filter_tree(&raw_markdown, q)
    } else {
        raw_markdown
    };

    if truncated_flag {
        let detail = match truncation_reason.as_deref() {
            Some("max_ax_time_ms") => format!(
                "the cooperative time budget ({})",
                max_ax_time_ms
                    .map(|ms| format!("{ms} ms"))
                    .unwrap_or_else(|| "expired".into())
            ),
            Some("max_depth") => format!("the maximum depth ({max_depth})"),
            _ if mode == WalkMode::CodexFull => format!(
                "{max_elements} returned nodes or {} scanned nodes",
                scan_limit_for_mode(max_elements, mode)
            ),
            _ => format!("{max_elements} nodes"),
        };
        tree_markdown.push_str(&format!(
            "\n⚠️  AX tree truncated at {detail} \
             (app has a very large accessibility tree — Arc, Electron, or similar). \
             Element indices above are still valid. Use pixel clicks for elements \
             not visible in this partial tree."
        ));
    }

    let owned_elements = RetainedNodeGuard::from_walk(&nodes);
    TreeWalkResult {
        tree_markdown,
        nodes,
        truncated: truncated_flag,
        truncation_reason,
        nodes_visited: visited_count,
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        owned_elements,
        watch_elements: RetainedNodeGuard::from_owned_ptrs(&watch_ptrs),
    }
}

fn should_collapse_layout_container(role: &str, mode: WalkMode) -> bool {
    role == "AXGroup" || (mode == WalkMode::Native && role == "AXScrollArea")
}

unsafe fn element_owning_window_id(element: AXUIElementRef) -> Option<u32> {
    if let Some(window) = copy_element_attr(element, "AXWindow") {
        let id = ax_get_window_id(window);
        CFRelease(window as CFTypeRef);
        if id.is_some() {
            return id;
        }
    }
    if let Some(id) = ax_get_window_id(element) {
        return Some(id);
    }

    // Some Chromium renderer proxies do not expose AXWindow and are not
    // accepted by the private window SPI directly. Ascend the standard AX
    // parent relationship until the enclosing AXWindow, with a hard bound for
    // malformed/cyclic trees.
    let mut current = element;
    let mut owns_current = false;
    for _ in 0..64 {
        if copy_string_attr(current, "AXRole").as_deref() == Some("AXWindow") {
            let id = ax_get_window_id(current);
            if owns_current {
                CFRelease(current as CFTypeRef);
            }
            return id;
        }
        let parent = copy_element_attr(current, "AXParent");
        if owns_current {
            CFRelease(current as CFTypeRef);
        }
        let Some(parent) = parent else {
            return None;
        };
        current = parent;
        owns_current = true;
    }
    if owns_current {
        CFRelease(current as CFTypeRef);
    }
    None
}

fn frames_intersect(frame: [f64; 4], viewport: [f64; 4]) -> bool {
    let [x, y, width, height] = frame;
    let [vx, vy, vwidth, vheight] = viewport;
    width > 0.0
        && height > 0.0
        && x < vx + vwidth
        && x + width > vx
        && y < vy + vheight
        && y + height > vy
}

fn should_walk_top_level(
    role: &str,
    child_window_id: Option<u32>,
    target_window_id: Option<u32>,
    mode: WalkMode,
) -> bool {
    let Some(target_window_id) = target_window_id else {
        return true;
    };
    if role == "AXWindow" {
        return child_window_id == Some(target_window_id);
    }
    false
}

fn child_attribute_for_role(role: &str, _mode: WalkMode) -> &'static str {
    match role {
        "AXOutline" | "AXTable" => return "AXVisibleRows",
        "AXCollection" => return "AXVisibleChildren",
        _ => {}
    }
    "AXChildren"
}

unsafe fn copy_children_for_walk(
    element: AXUIElementRef,
    role: &str,
    mode: WalkMode,
    deadline: Option<std::time::Instant>,
) -> Vec<AXUIElementRef> {
    let mut children = copy_element_array_attr(element, child_attribute_for_role(role, mode));
    // Large virtualized collections already use AXVisibleRows/AXVisibleChildren
    // in CodexFull mode. Avoid a descriptor IPC for every one of thousands of
    // siblings before the walk's own count/deadline guards can run.
    if children.len() <= 128 {
        prioritize_children(&mut children, deadline);
    }
    children
}

/// Visit focused and visible descendants first. Chromium frequently exposes a
/// large collection of offscreen wrappers before the active renderer subtree;
/// stable priority makes bounded walks useful without changing element identity
/// or requiring a second traversal.
unsafe fn prioritize_children(
    children: &mut [AXUIElementRef],
    deadline: Option<std::time::Instant>,
) {
    for element in children.iter().copied() {
        if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
            return;
        }
        apply_messaging_timeout(element, deadline);
    }
    // Cache each key once. `sort_by_key` would repeat the AX batch read for
    // every comparison (O(n log n) IPC); cached keys keep this pass O(n).
    children.sort_by_cached_key(|element| std::cmp::Reverse(child_priority(*element)));
}

unsafe fn child_priority(element: AXUIElementRef) -> u8 {
    let descriptor = copy_descriptor_strings(element);
    let mut score = u8::from(descriptor.focused == Some(true)) * 4;
    let labelled = descriptor
        .title
        .as_deref()
        .or(descriptor.description.as_deref())
        .or(descriptor.value.as_deref())
        .is_some_and(|label| !label.trim().is_empty());
    if labelled {
        score += 1;
    }
    if descriptor
        .position
        .zip(descriptor.size)
        .is_some_and(|(_, (width, height))| width > 5.0 && height > 5.0)
    {
        score += 2;
    }
    score
}

fn scan_limit_for_mode(max_elements: usize, mode: WalkMode) -> usize {
    if mode == WalkMode::CodexFull {
        // A small allowance still lets layout wrappers lead to useful
        // descendants without turning an 800-element response into 6,400 AX
        // round trips. Visible-row traversal handles large virtualized lists.
        max_elements.saturating_mul(2)
    } else {
        max_elements
    }
}

fn should_index_node(role: &str, is_actionable: bool, has_content: bool, mode: WalkMode) -> bool {
    if is_actionable {
        return true;
    }
    if mode != WalkMode::CodexFull {
        return false;
    }

    match role {
        // Text can be consumed by select_text, including static labels whose
        // selectable ancestor owns AXSelectedTextRange.
        "AXStaticText" | "AXHeading" | "AXTextField" | "AXTextArea" | "AXSearchField" => {
            has_content
        }
        // These containers are meaningful wheel targets even when they expose
        // no AX action of their own. Empty layout-only groups stay in the
        // markdown hierarchy without consuming an addressable index.
        "AXScrollArea" | "AXWebArea" | "AXList" | "AXOutline" | "AXTable" | "AXCollection" => true,
        _ => false,
    }
}

/// Chromium often advertises generic navigation actions on anonymous layout
/// wrappers. Those actions do not identify a control an agent can safely
/// press. Keep explicit controls and labelled menu/scroll targets addressable,
/// while letting the wrapper collapse to its useful descendants.
fn has_meaningful_actions(role: &str, actions: &[String], has_content: bool) -> bool {
    actions.iter().any(|action| {
        let generic = matches!(action.as_str(), "AXShowMenu" | "AXScrollToVisible");
        !generic
            || has_content
            || matches!(
                role,
                "AXMenuItem"
                    | "AXButton"
                    | "AXLink"
                    | "AXScrollArea"
                    | "AXList"
                    | "AXOutline"
                    | "AXTable"
                    | "AXCollection"
            )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_element(
    element: AXUIElementRef,
    depth: usize,
    parent_index: Option<usize>,
    nodes: &mut Vec<AXNode>,
    lines: &mut Vec<(usize, String)>,
    counter: &mut usize,
    visited_count: &mut usize,
    truncated: &mut bool,
    truncation_reason: &mut Option<String>,
    max_elements: usize,
    max_depth: usize,
    deadline: Option<std::time::Instant>,
    mode: WalkMode,
    visible_bounds: Option<[f64; 4]>,
    watch_ptrs: &mut Vec<usize>,
    seen: &mut Vec<AXUIElementRef>,
) {
    if depth_limit_reached(depth, max_depth, truncated) {
        *truncation_reason = Some("max_depth".to_owned());
        return;
    }
    if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
        *truncated = true;
        *truncation_reason = Some("max_ax_time_ms".to_owned());
        return;
    }
    apply_messaging_timeout(element, deadline);
    // Codex compatibility separates the 800-node response budget from a
    // larger bounded scan budget. This lets empty Electron layout wrappers be
    // traversed without crowding useful controls out of the addressable map,
    // while retaining a hard stop for pathological trees.
    let scan_limit = scan_limit_for_mode(max_elements, mode);
    if *visited_count >= scan_limit {
        *truncated = true;
        *truncation_reason = Some("max_elements".to_owned());
        return;
    }
    if seen
        .iter()
        .any(|other| CFEqual(*other as CFTypeRef, element as CFTypeRef) != 0)
    {
        return;
    }
    seen.push(element);
    *visited_count += 1;
    CFRetain(element as CFTypeRef);
    watch_ptrs.push(element as usize);

    let descriptor = copy_descriptor_strings(element);
    let role = descriptor.role.unwrap_or_else(|| "AXUnknown".into());

    // Keep AXTitle and AXDescription SEPARATE so that the tree format matches
    // the Swift reference: title → "title", description → (description).
    // This is critical for Calculator where AXTitle="" but AXDescription="2"
    // (digit buttons). Merging them would produce "2" (quoted) instead of (2)
    // (parens), breaking _find_calc_button which searches for "(2)".
    let descriptor_frame = descriptor
        .position
        .zip(descriptor.size)
        .map(|((x, y), (w, h))| [x, y, w, h]);
    let title = descriptor.title;
    let value = descriptor.value;
    // AXPlaceholderValue as fallback for empty text fields.
    let value = value
        .filter(|v| !v.trim().is_empty())
        .or_else(|| descriptor.placeholder);
    let description = descriptor.description;
    let identifier = descriptor.identifier;
    let help = descriptor.help.filter(|h| !h.trim().is_empty());
    let actions = copy_action_names(element);

    let visible_title = title.as_deref().unwrap_or("").trim().to_owned();
    let visible_description = description.as_deref().unwrap_or("").trim().to_owned();
    let visible_value = value.as_deref().unwrap_or("").trim().to_owned();
    let visible_value = truncate_ax_value(visible_value);

    let has_content =
        !visible_title.is_empty() || !visible_description.is_empty() || !visible_value.is_empty();
    let is_actionable = has_meaningful_actions(&role, &actions, has_content);
    let is_indexed = should_index_node(&role, is_actionable, has_content, mode);

    // Collapse only pure layout wrappers. A labelled or actionable AXGroup
    // is a real target and must remain in the compact map.
    if should_collapse_layout_container(&role, mode) && !has_content && !is_actionable {
        let children = copy_children_for_walk(element, &role, mode, deadline);
        for child in children {
            walk_element(
                child,
                depth,
                parent_index,
                nodes,
                lines,
                counter,
                visited_count,
                truncated,
                truncation_reason,
                max_elements,
                max_depth,
                deadline,
                mode,
                visible_bounds,
                watch_ptrs,
                seen,
            );
            CFRelease(child as CFTypeRef);
        }
        return;
    }

    let frame = descriptor_frame.or_else(|| element_screen_rect(element));
    let unusable_frame = frame.is_some_and(|[_, _, width, height]| width <= 0.0 || height <= 0.0);
    let offscreen = visible_bounds.is_some_and(|viewport| {
        frame.is_some_and(|[_, _, width, height]| {
            width > 0.0
                && height > 0.0
                && frame.is_some_and(|candidate| !frames_intersect(candidate, viewport))
        })
    });

    if descriptor.hidden == Some(true) || offscreen {
        // A positive-area subtree outside the requested window cannot produce
        // a valid target. Zero-size wrappers are handled below because
        // Chromium may hide visible descendants behind them.
        return;
    }

    if unusable_frame && !is_indexed {
        let children = copy_children_for_walk(element, &role, mode, deadline);
        for child in children {
            walk_element(
                child,
                depth + 1,
                parent_index,
                nodes,
                lines,
                counter,
                truncated,
                truncation_reason,
                max_elements,
                max_depth,
                deadline,
                mode,
                visible_bounds,
                watch_ptrs,
                seen,
            );
            CFRelease(child as CFTypeRef);
        }
        return;
    }

    if !is_indexed && !has_content && role != "AXWindow" && role != "AXSheet" {
        let children = copy_children_for_walk(element, &role, mode, deadline);
        for child in children {
            walk_element(
                child,
                depth + 1,
                parent_index,
                nodes,
                lines,
                counter,
                visited_count,
                truncated,
                truncation_reason,
                max_elements,
                max_depth,
                deadline,
                mode,
                visible_bounds,
                watch_ptrs,
                seen,
            );
            CFRelease(child as CFTypeRef);
        }
        return;
    }

    if nodes.len() >= max_elements {
        *truncated = true;
        *truncation_reason = Some("max_elements".to_owned());
        return;
    }

    let element_ptr = element as usize;
    let element_index = is_indexed.then(|| {
        let index = *counter;
        *counter += 1;
        index
    });
    let node = AXNode {
        element_index,
        role: role.clone(),
        title: if visible_title.is_empty() {
            None
        } else {
            Some(visible_title.clone())
        },
        value: if visible_value.is_empty() {
            None
        } else {
            Some(visible_value.clone())
        },
        description: if visible_description.is_empty() {
            None
        } else {
            Some(visible_description.clone())
        },
        identifier: identifier.clone(),
        help: help.clone(),
        actions: actions.clone(),
        element_ptr,
        depth,
        parent_element_index: parent_index,
        frame,
    };
    let descendants_parent_index = node.element_index.or(parent_index);
    let line = format_node_line(&node);
    // Keep every emitted node alive through publication. This includes
    // non-indexed labelled/layout nodes that the observer cache watches.
    CFRetain(element as CFTypeRef);
    lines.push((depth, line));
    nodes.push(node);

    let children = copy_children_for_walk(element, &role, mode, deadline);
    for child in children {
        walk_element(
            child,
            depth + 1,
            descendants_parent_index,
            nodes,
            lines,
            counter,
            visited_count,
            truncated,
            truncation_reason,
            max_elements,
            max_depth,
            deadline,
            mode,
            visible_bounds,
            watch_ptrs,
            seen,
        );
        CFRelease(child as CFTypeRef);
    }
}

fn depth_limit_reached(depth: usize, max_depth: usize, truncated: &mut bool) -> bool {
    if depth <= max_depth {
        return false;
    }
    *truncated = true;
    true
}

fn format_node_line(node: &AXNode) -> String {
    let mut parts = String::new();

    // Common prefix (with or without index).
    if let Some(idx) = node.element_index {
        parts.push_str(&format!("- [{}] {}", idx, node.role));
    } else {
        parts.push_str(&format!("- {}", node.role));
    }

    // AXTitle → "title"
    if let Some(t) = &node.title {
        parts.push_str(&format!(" \"{}\"", t));
    }
    // AXValue → = "value"
    if let Some(v) = &node.value {
        parts.push_str(&format!(" = \"{}\"", v));
    }
    // AXDescription → (description) — critical for Calculator digit buttons
    // where AXTitle="" but AXDescription="2".
    if let Some(d) = &node.description {
        parts.push_str(&format!(" ({})", d));
    }

    // Bracketed metadata block (identifier, help, actions).
    if node.element_index.is_some() {
        let mut attrs: Vec<String> = Vec::new();
        if let Some(id) = &node.identifier {
            attrs.push(format!("id={}", id));
        }
        if let Some(h) = &node.help {
            attrs.push(format!("help=\"{}\"", h));
        }
        if !node.actions.is_empty() {
            let action_str = node
                .actions
                .iter()
                .map(|a| a.strip_prefix("AX").unwrap_or(a).to_lowercase())
                .collect::<Vec<_>>()
                .join(",");
            attrs.push(format!("actions=[{}]", action_str));
        }
        if !attrs.is_empty() {
            parts.push_str(" [");
            parts.push_str(&attrs.join(" "));
            parts.push(']');
        }
    }

    parts
}

fn render_lines(lines: &[(usize, String)]) -> String {
    let mut out = String::new();
    for (depth, line) in lines {
        for _ in 0..*depth {
            out.push_str("  ");
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Render cached nodes back to the stable Markdown representation used by
/// get_window_state clean hits.
pub fn render_nodes_markdown(nodes: &[AXNode]) -> String {
    let lines = nodes
        .iter()
        .map(|node| (node.depth, format_node_line(node)))
        .collect::<Vec<_>>();
    render_lines(&lines)
}

/// Filter the tree markdown to lines matching `query` plus their ancestor chain.
fn filter_tree(markdown: &str, query: &str) -> String {
    let needle = query.to_lowercase();
    let lines: Vec<&str> = markdown.lines().collect();

    let mut current_ancestor: Vec<&str> = Vec::new();
    let mut last_emitted_at: Vec<Option<&str>> = Vec::new();
    let mut output: Vec<&str> = Vec::new();

    for line in &lines {
        let depth = leading_indent_depth(line);

        while current_ancestor.len() <= depth {
            current_ancestor.push("");
            last_emitted_at.push(None);
        }
        for deeper in (depth + 1)..current_ancestor.len() {
            last_emitted_at[deeper] = None;
        }
        current_ancestor[depth] = line;

        if line.to_lowercase().contains(&needle) {
            for ancestor_depth in 0..depth {
                let ancestor = current_ancestor[ancestor_depth];
                if ancestor.is_empty() {
                    continue;
                }
                if last_emitted_at[ancestor_depth] == Some(ancestor) {
                    continue;
                }
                last_emitted_at[ancestor_depth] = Some(ancestor);
                output.push(ancestor);
            }
            last_emitted_at[depth] = Some(line);
            output.push(line);
        }
    }

    if output.is_empty() {
        return String::new();
    }
    let mut result = output.join("\n");
    result.push('\n');
    result
}

pub fn filter_tree_markdown(markdown: &str, query: &str) -> String {
    filter_tree(markdown, query)
}

fn leading_indent_depth(line: &str) -> usize {
    let mut count = 0;
    for ch in line.chars() {
        if ch == ' ' {
            count += 1;
        } else {
            break;
        }
    }
    count / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mode_remains_action_only_and_collapses_layout() {
        assert!(should_index_node("AXButton", true, true, WalkMode::Native));
        assert!(!should_index_node(
            "AXStaticText",
            false,
            true,
            WalkMode::Native
        ));
        assert!(!should_index_node(
            "AXWindow",
            false,
            false,
            WalkMode::Native
        ));
        assert!(should_collapse_layout_container(
            "AXGroup",
            WalkMode::Native
        ));
        assert!(should_collapse_layout_container(
            "AXScrollArea",
            WalkMode::Native
        ));
    }

    #[test]
    fn codex_full_mode_indexes_only_nodes_consumable_by_compat_actions() {
        assert!(should_index_node(
            "AXStaticText",
            false,
            true,
            WalkMode::CodexFull
        ));
        assert!(should_index_node(
            "AXScrollArea",
            false,
            false,
            WalkMode::CodexFull
        ));
        assert!(should_index_node(
            "AXTextField",
            false,
            true,
            WalkMode::CodexFull
        ));
        assert!(!should_index_node(
            "AXWindow",
            false,
            false,
            WalkMode::CodexFull
        ));
        assert!(!should_index_node(
            "AXGroup",
            false,
            false,
            WalkMode::CodexFull
        ));
        assert!(!should_index_node(
            "AXUnknown",
            false,
            false,
            WalkMode::CodexFull
        ));
        assert!(should_collapse_layout_container(
            "AXGroup",
            WalkMode::CodexFull
        ));
        assert!(!should_collapse_layout_container(
            "AXScrollArea",
            WalkMode::CodexFull
        ));
    }

    #[test]
    fn depth_limit_marks_partial_trees_as_truncated() {
        let mut truncated = false;
        assert!(!depth_limit_reached(20, 20, &mut truncated));
        assert!(!truncated);
        assert!(depth_limit_reached(21, 20, &mut truncated));
        assert!(truncated);
    }

    #[test]
    fn timed_walk_bounds_each_ax_message_timeout() {
        assert_eq!(
            messaging_timeout_seconds(std::time::Duration::ZERO),
            MIN_AX_MESSAGE_TIMEOUT_SECONDS
        );
        assert_eq!(
            messaging_timeout_seconds(std::time::Duration::from_secs(1)),
            MAX_AX_MESSAGE_TIMEOUT_SECONDS
        );
        assert_eq!(
            messaging_timeout_seconds(std::time::Duration::from_millis(50)),
            0.05
        );
    }

    #[test]
    fn top_level_window_tracker_deduplicates_distinct_wrappers_for_one_window() {
        let mut tracker = TopLevelWindowTracker::default();
        assert!(tracker.insert(0x1111, Some(7)));
        assert!(
            !tracker.insert(0x2222, Some(7)),
            "one CGWindowID returned through two AX wrappers must be walked once"
        );
        assert!(tracker.insert(0x3333, None));
        assert!(
            !tracker.insert(0x3333, None),
            "the same retained AX reference must be walked once"
        );
    }

    #[test]
    fn top_level_window_tracker_preserves_distinct_window_identity() {
        let mut tracker = TopLevelWindowTracker::default();
        assert!(tracker.insert(0x1111, Some(7)));
        assert!(tracker.insert(0x2222, Some(8)));
        // Role, label, and frame are deliberately absent from this identity
        // seam: distinct overlapping controls with identical display
        // attributes must remain separately addressable inside each window.
    }

    #[test]
    fn codex_window_capture_excludes_menu_bar_and_other_top_level_children() {
        assert!(should_walk_top_level(
            "AXWindow",
            Some(7),
            Some(7),
            WalkMode::CodexFull,
        ));
        assert!(!should_walk_top_level(
            "AXWindow",
            Some(8),
            Some(7),
            WalkMode::CodexFull,
        ));
        assert!(!should_walk_top_level(
            "AXMenuBar",
            None,
            Some(7),
            WalkMode::CodexFull,
        ));
        assert!(!should_walk_top_level(
            "AXMenuBar",
            None,
            Some(7),
            WalkMode::Native
        ));
    }

    #[test]
    fn codex_large_collections_walk_only_visible_rows() {
        assert_eq!(
            child_attribute_for_role("AXOutline", WalkMode::CodexFull),
            "AXVisibleRows"
        );
        assert_eq!(
            child_attribute_for_role("AXTable", WalkMode::CodexFull),
            "AXVisibleRows"
        );
        assert_eq!(
            child_attribute_for_role("AXCollection", WalkMode::CodexFull),
            "AXVisibleChildren"
        );
        assert_eq!(
            child_attribute_for_role("AXOutline", WalkMode::Native),
            "AXVisibleRows",
            "large native collections should prefer visible rows"
        );
    }

    #[test]
    fn codex_scan_budget_stays_close_to_the_response_budget() {
        assert_eq!(scan_limit_for_mode(800, WalkMode::CodexFull), 1_600);
        assert_eq!(scan_limit_for_mode(800, WalkMode::Native), 800);
    }

    #[test]
    fn truncate_ax_value_keeps_unicode_boundaries() {
        let value = "é".repeat(MAX_AX_VALUE_CHARS + 1);
        let truncated = truncate_ax_value(value);
        assert_eq!(truncated.chars().count(), MAX_AX_VALUE_CHARS + 1);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn anonymous_generic_actions_do_not_create_click_targets() {
        assert!(!has_meaningful_actions(
            "AXGroup",
            &["AXShowMenu".into(), "AXScrollToVisible".into()],
            false
        ));
        assert!(has_meaningful_actions(
            "AXMenuItem",
            &["AXShowMenu".into()],
            false
        ));
        assert!(has_meaningful_actions(
            "AXGroup",
            &["AXPress".into()],
            false
        ));
    }

    #[test]
    fn offscreen_intersection_requires_positive_overlap() {
        let viewport = [10.0, 10.0, 100.0, 100.0];
        assert!(frames_intersect([20.0, 20.0, 10.0, 10.0], viewport));
        assert!(!frames_intersect([0.0, 0.0, 5.0, 5.0], viewport));
        assert!(!frames_intersect([20.0, 20.0, 0.0, 10.0], viewport));
    }
}
