//! Per-(pid, window_id) element cache.
//!
//! After `get_window_state`, each actionable element's AXUIElementRef pointer
//! is cached by element_index. Subsequent `click`, `type_text`, etc. look up
//! the element_index to get the raw pointer and perform AX actions on it.
//!
//! Cache is scoped per (pid, window_id) — a new `get_window_state` call
//! for the same (pid, window_id) replaces the entire entry.
//!
//! Memory contract:
//!   tree::walk_element retains each actionable element before storing its ptr.
//!   CachedSnapshot::drop releases those retains so we have no AX leaks.
//!
//! The locked-HashMap plumbing lives in `cmux_cua_core::element_cache` — see
//! `docs/dedup-audit.md` item #3. This module owns the macOS-specific
//! `CacheKey`, `CachedSnapshot`, and the `Drop` impl that fires `CFRelease`
//! when an entry is replaced or removed.

use super::bindings::AXUIElementRef;
use super::identity::IdentityRegistry;
use super::observer::{DirtyEvent, DirtyKind, ObserverHub, ObserverKey};
use super::tree::{AXNode, RetainedNodeGuard};
use cmux_cua_core::element_cache::ElementCacheCore;
use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// An AXUIElementRef borrowed out of the cache with an extra `CFRetain`, so it
/// stays alive for the duration of an AX action even if a concurrent
/// `get_window_state` (→ [`ElementCache::update`]) replaces and drops the
/// snapshot it came from. Without this, the snapshot's `Drop` could `CFRelease`
/// the element to zero while an in-flight click was still dereferencing the raw
/// pointer — a use-after-free that trips `AXUIElementCopyActionNames` →
/// `CFGetTypeID` (`EXC_BREAKPOINT`) and crashes the daemon. The retain is taken
/// under the cache lock (see [`ElementCache::get_element_retained`]); the
/// matching `CFRelease` fires on drop.
pub struct RetainedElement(usize);

impl RetainedElement {
    /// The raw pointer, valid for as long as this guard is held.
    pub fn as_ptr(&self) -> usize {
        self.0
    }
}

// The raw AXUIElementRef is already shuttled across threads as a `usize` into
// `spawn_blocking`; wrapping it in a retain guard doesn't change that, and CF
// reference counting is thread-safe, so the guard is safe to Send.
unsafe impl Send for RetainedElement {}

impl Drop for RetainedElement {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { CFRelease(self.0 as AXUIElementRef as CFTypeRef) };
        }
    }
}

/// Key for the element cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub pid: i32,
    pub window_id: u32,
}

/// Process/window scope for the persistent tree cache. The generation is
/// intentionally separate from the pid because macOS may recycle a pid after
/// an app exits while the daemon remains alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowCacheKey {
    pub pid: i32,
    pub window_id: u32,
    pub process_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactNode {
    pub id: String,
    pub role: String,
    pub label: Option<String>,
    pub value: Option<String>,
    pub actions: Vec<String>,
    /// Window-local `[x, y, width, height]` after applying the origin passed
    /// to `snapshot_json`; the internal node retains its screen frame.
    pub frame: Option<[f64; 4]>,
    pub element_index: Option<usize>,
    pub parent_id: Option<String>,
}

impl CompactNode {
    pub fn to_json(&self, origin: [f64; 2]) -> Value {
        let mut out = serde_json::json!({
            "id": self.id,
            "role": self.role,
            "actions": self.actions,
        });
        if let Some(label) = &self.label {
            out["label"] = Value::String(label.clone());
        }
        if let Some(value) = &self.value {
            out["value"] = Value::String(value.clone());
        }
        if let Some(frame) = self.frame {
            out["frame"] = serde_json::json!({
                "x": frame[0] - origin[0],
                "y": frame[1] - origin[1],
                "w": frame[2],
                "h": frame[3],
            });
        }
        if let Some(index) = self.element_index {
            out["element_index"] = serde_json::json!(index);
        }
        if let Some(parent_id) = &self.parent_id {
            out["parent_id"] = Value::String(parent_id.clone());
        }
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotDiff {
    pub from_revision: Option<u64>,
    pub to_revision: u64,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirtyFlags {
    pub value: bool,
    pub children: bool,
    pub focus: bool,
    pub layout: bool,
    pub destroyed: bool,
}

impl DirtyFlags {
    fn mark(&mut self, kind: DirtyKind) {
        match kind {
            DirtyKind::Value => self.value = true,
            DirtyKind::Children => self.children = true,
            DirtyKind::Focus => self.focus = true,
            DirtyKind::Layout => self.layout = true,
            DirtyKind::Destroyed => self.destroyed = true,
            DirtyKind::Unknown => {
                self.children = true;
                self.layout = true;
            }
        }
    }

    pub fn any(self) -> bool {
        self.value || self.children || self.focus || self.layout || self.destroyed
    }
}

#[derive(Debug, Clone)]
pub struct CachedWindowState {
    pub key: WindowCacheKey,
    pub revision: u64,
    pub nodes: Vec<AXNode>,
    pub compact_nodes: Vec<CompactNode>,
    pub diff: SnapshotDiff,
    pub dirty: DirtyFlags,
    pub observer_supported: bool,
    pub cache_hit: bool,
    /// Descriptor reads performed to produce this state. A clean cache hit is
    /// explicitly zero, which is the latency regression guard for Electron.
    pub ax_reads: u64,
    /// Keeps every raw AX handle referenced by `nodes` alive after the cache
    /// lock is released.  Consumers may retain this state while dispatching
    /// an action; replacing the cache entry cannot free those handles out
    /// from under them.
    pub(crate) owned_elements: Arc<RetainedNodeGuard>,
}

struct WindowEntry {
    key: WindowCacheKey,
    revision: u64,
    nodes: Vec<AXNode>,
    compact_nodes: Vec<CompactNode>,
    diff: SnapshotDiff,
    dirty: DirtyFlags,
    observer_supported: bool,
    identity_ids: HashMap<usize, String>,
    identity_registry: IdentityRegistry,
    next_identity: u64,
    pending_events: Vec<DirtyEvent>,
    owned_elements: Option<Arc<RetainedNodeGuard>>,
    max_elements: usize,
    max_depth: usize,
    full_map: bool,
}

impl Default for WindowEntry {
    fn default() -> Self {
        Self {
            key: WindowCacheKey {
                pid: 0,
                window_id: 0,
                process_generation: 0,
            },
            revision: 0,
            nodes: Vec::new(),
            compact_nodes: Vec::new(),
            diff: SnapshotDiff::default(),
            dirty: DirtyFlags::default(),
            observer_supported: false,
            identity_ids: HashMap::new(),
            identity_registry: IdentityRegistry::with_scope(0, 0, 0, 4096),
            next_identity: 0,
            pending_events: Vec::new(),
            owned_elements: None,
            max_elements: usize::MAX,
            max_depth: usize::MAX,
            full_map: false,
        }
    }
}

/// Cached snapshot for one (pid, window_id) pair.
pub struct CachedSnapshot {
    /// element_index → raw AXUIElementRef pointer (retained, as usize for Send).
    pub elements: Vec<usize>,
}

impl Drop for CachedSnapshot {
    fn drop(&mut self) {
        // Release the extra CFRetain that walk_element added for each cached ptr.
        for ptr in &self.elements {
            if *ptr != 0 {
                unsafe { CFRelease(*ptr as AXUIElementRef as CFTypeRef) };
            }
        }
    }
}

/// Global element cache.
pub struct ElementCache {
    core: ElementCacheCore<CacheKey, CachedSnapshot>,
    windows: Mutex<HashMap<WindowCacheKey, WindowEntry>>,
    process_generations: Mutex<HashMap<i32, u64>>,
    observer: Arc<ObserverHub>,
}

impl ElementCache {
    pub fn new() -> Self {
        Self {
            core: ElementCacheCore::new(),
            windows: Mutex::new(HashMap::new()),
            process_generations: Mutex::new(HashMap::new()),
            observer: ObserverHub::new(),
        }
    }

    /// Return a cache key only when the kernel can identify this live process.
    /// A pid without a start stamp is not a safe identity: reusing the last
    /// monotonic fallback would let a dead/recycled process consume a clean
    /// snapshot from an unrelated process. Callers must use their bounded
    /// full-walk path when this returns `None`.
    pub fn window_key(&self, pid: i32, window_id: u32) -> Option<WindowCacheKey> {
        let mut generations = self
            .process_generations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(observed) = super::enablement::process_generation(pid) else {
            // Never use a cached generation when proc_pidinfo cannot prove
            // that this pid is still the same process. Drop both snapshots
            // and observer registrations; the next successful observation
            // will build a fresh generation and registration.
            generations.remove(&pid);
            self.remove_window_entries(pid);
            return None;
        };
        let prior = generations.insert(pid, observed);
        if prior.is_some_and(|prior| prior != observed) {
            // A recycled pid must never reuse a clean old snapshot.
            self.remove_window_entries(pid);
        }
        Some(WindowCacheKey {
            pid,
            window_id,
            process_generation: observed,
        })
    }

    fn remove_window_entries(&self, pid: i32) {
        let removed_keys = if let Ok(mut windows) = self.windows.lock() {
            let keys = windows
                .keys()
                .filter(|key| key.pid == pid)
                .copied()
                .collect::<Vec<_>>();
            windows.retain(|key, _| key.pid != pid);
            keys
        } else {
            Vec::new()
        };
        for key in removed_keys {
            self.observer.remove(ObserverKey {
                pid: key.pid,
                window_id: key.window_id,
                process_generation: key.process_generation,
            });
        }
    }

    pub fn invalidate_process(&self, pid: i32) {
        if let Ok(mut generations) = self.process_generations.lock() {
            generations.remove(&pid);
        }
        self.remove_window_entries(pid);
    }

    pub fn observer(&self) -> Arc<ObserverHub> {
        self.observer.clone()
    }

    /// Drain notification records and mark only the affected window dirty.
    /// The callback has already retained each element, so this method may
    /// safely compare/remove handles before the event retain is dropped.
    pub fn poll_dirty(&self, pid: i32, window_id: u32) -> DirtyFlags {
        let Some(key) = self.window_key(pid, window_id) else {
            return DirtyFlags::default();
        };
        let observer_key = ObserverKey {
            pid: key.pid,
            window_id: key.window_id,
            process_generation: key.process_generation,
        };
        let events = self.observer.drain(observer_key);
        if events.is_empty() {
            return DirtyFlags::default();
        }
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let entry = windows.entry(key).or_insert_with(|| WindowEntry {
            key,
            ..WindowEntry::default()
        });
        if entry.key != key {
            entry.identity_registry =
                IdentityRegistry::with_scope(key.pid, key.window_id, key.process_generation, 4096);
        }
        for event in events {
            entry.dirty.mark(event.kind);
            entry.pending_events.push(event);
        }
        entry.dirty
    }

    /// Take queued events for a window so the caller can refresh only the
    /// affected descriptor or subtree. Each event owns an AX retain until the
    /// returned vector is dropped.
    pub fn take_dirty_events(&self, pid: i32, window_id: u32) -> Vec<DirtyEvent> {
        self.poll_dirty(pid, window_id);
        let Some(key) = self.window_key(pid, window_id) else {
            return Vec::new();
        };
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        windows
            .get_mut(&key)
            .map(|entry| std::mem::take(&mut entry.pending_events))
            .unwrap_or_default()
    }

    pub fn is_clean(&self, pid: i32, window_id: u32) -> bool {
        let Some(key) = self.window_key(pid, window_id) else {
            return false;
        };
        self.poll_dirty(pid, window_id);
        self.windows
            .lock()
            .ok()
            .and_then(|windows| windows.get(&key).map(|entry| !entry.dirty.any()))
            .unwrap_or(false)
    }

    /// Return a clean metadata snapshot without touching AX. A clean hit has
    /// `ax_reads == 0`, which is the warm-path latency/IPC invariant.
    pub fn cached_window(&self, pid: i32, window_id: u32) -> Option<CachedWindowState> {
        self.cached_window_for_policy(pid, window_id, usize::MAX, usize::MAX, false)
    }

    pub fn cached_window_for_policy(
        &self,
        pid: i32,
        window_id: u32,
        max_elements: usize,
        max_depth: usize,
        full_map: bool,
    ) -> Option<CachedWindowState> {
        let key = self.window_key(pid, window_id)?;
        self.poll_dirty(pid, window_id);
        let windows = self.windows.lock().ok()?;
        let entry = windows.get(&key)?;
        if entry.max_elements != max_elements
            || entry.max_depth != max_depth
            || entry.full_map != full_map
        {
            return None;
        }
        if entry.dirty.any() {
            return None;
        }
        let owned_elements = entry
            .owned_elements
            .clone()
            .unwrap_or_else(|| Arc::new(RetainedNodeGuard::empty()));
        let state = CachedWindowState {
            key,
            revision: entry.revision,
            nodes: entry.nodes.clone(),
            compact_nodes: entry.compact_nodes.clone(),
            // A clean read is a new consumer cursor over the same revision,
            // not another replay of the prior mutation diff.
            diff: SnapshotDiff {
                from_revision: Some(entry.revision),
                to_revision: entry.revision,
                ..SnapshotDiff::default()
            },
            dirty: entry.dirty,
            observer_supported: entry.observer_supported,
            cache_hit: true,
            ax_reads: 0,
            owned_elements,
        };
        Some(state)
    }

    /// Publish a newly collected tree and (re)arm observers for its retained
    /// handles. The registration happens after publication; any notification
    /// racing the handoff lands in the queue and makes the next read dirty.
    pub fn update_window(
        &self,
        pid: i32,
        window_id: u32,
        nodes: &[AXNode],
        ax_reads: u64,
    ) -> CachedWindowState {
        let owned_elements = RetainedNodeGuard::retain_nodes(nodes);
        self.update_window_owned_with_policy(
            pid,
            window_id,
            nodes,
            ax_reads,
            owned_elements,
            usize::MAX,
            usize::MAX,
            false,
        )
    }

    /// Publish a tree with an ownership guard duplicated from the walk result.
    /// The caller may drop its `TreeWalkResult` immediately after this call;
    /// the cache retains its own guard until replacement/removal.
    pub fn update_window_owned(
        &self,
        pid: i32,
        window_id: u32,
        nodes: &[AXNode],
        ax_reads: u64,
        owned_elements: RetainedNodeGuard,
    ) -> CachedWindowState {
        self.update_window_owned_with_policy(
            pid,
            window_id,
            nodes,
            ax_reads,
            owned_elements,
            usize::MAX,
            usize::MAX,
            false,
        )
    }

    /// Publish a tree and remember the traversal policy that produced it.
    /// A later caller using a different cap/map mode must take the cold path;
    /// serving a smaller cached map as if it were complete would silently
    /// hide controls from Electron callers.
    pub fn update_window_owned_with_policy(
        &self,
        pid: i32,
        window_id: u32,
        nodes: &[AXNode],
        ax_reads: u64,
        owned_elements: RetainedNodeGuard,
        max_elements: usize,
        max_depth: usize,
        full_map: bool,
    ) -> CachedWindowState {
        let Some(key) = self.window_key(pid, window_id) else {
            return transient_window_state(pid, window_id, nodes, ax_reads, owned_elements);
        };
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let entry = windows.entry(key).or_insert_with(|| WindowEntry {
            key,
            ..WindowEntry::default()
        });
        if entry.key != key {
            entry.identity_registry =
                IdentityRegistry::with_scope(key.pid, key.window_id, key.process_generation, 4096);
        }
        let previous = entry.compact_nodes.clone();
        entry.revision = entry.revision.saturating_add(1);
        entry.key = key;
        entry.nodes = nodes.to_vec();
        entry.compact_nodes = compact_nodes(nodes, entry);
        entry.diff = diff_nodes(entry.revision, &previous, &entry.compact_nodes);
        entry.dirty = DirtyFlags::default();
        let owned_elements = Arc::new(owned_elements);
        entry.owned_elements = Some(owned_elements.clone());
        let observer_elements = owned_elements
            .pointers()
            .iter()
            .copied()
            .filter(|ptr| *ptr != 0)
            .collect::<Vec<_>>();
        drop(windows);

        let observer_supported = self.observer.register(
            ObserverKey {
                pid: key.pid,
                window_id: key.window_id,
                process_generation: key.process_generation,
            },
            observer_elements,
        );
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let entry = windows.get_mut(&key).expect("window entry published above");
        entry.observer_supported = observer_supported;
        entry.max_elements = max_elements;
        entry.max_depth = max_depth;
        entry.full_map = full_map;
        if !observer_supported {
            // AXObserver is optional on older/denied targets. Never return a
            // clean forever snapshot when no invalidation source exists.
            entry.dirty.children = true;
        }
        CachedWindowState {
            key,
            revision: entry.revision,
            nodes: entry.nodes.clone(),
            compact_nodes: entry.compact_nodes.clone(),
            diff: entry.diff.clone(),
            dirty: entry.dirty,
            observer_supported,
            cache_hit: false,
            ax_reads,
            owned_elements,
        }
    }

    /// Variant used by tree walkers that retain collapsed layout wrappers for
    /// observer registration. The wrappers stay out of serialized nodes but
    /// remain owned by the same cache guard and are watched for AXChildren and
    /// layout notifications.
    pub fn update_window_owned_with_policy_and_watch(
        &self,
        pid: i32,
        window_id: u32,
        nodes: &[AXNode],
        ax_reads: u64,
        mut owned_elements: RetainedNodeGuard,
        watch_elements: RetainedNodeGuard,
        max_elements: usize,
        max_depth: usize,
        full_map: bool,
    ) -> CachedWindowState {
        owned_elements.append(watch_elements);
        self.update_window_owned_with_policy(
            pid,
            window_id,
            nodes,
            ax_reads,
            owned_elements,
            max_elements,
            max_depth,
            full_map,
        )
    }

    pub fn query_compact(
        &self,
        pid: i32,
        window_id: u32,
        origin: [f64; 2],
        role: Option<&str>,
        label: Option<&str>,
        region: Option<[f64; 4]>,
    ) -> Vec<Value> {
        let Some(snapshot) = self.cached_window(pid, window_id) else {
            return Vec::new();
        };
        let role = role.map(str::to_ascii_lowercase);
        let label = label.map(str::to_ascii_lowercase);
        snapshot
            .compact_nodes
            .iter()
            .filter(|node| {
                role.as_deref()
                    .is_none_or(|wanted| node.role.eq_ignore_ascii_case(wanted))
            })
            .filter(|node| {
                label.as_deref().is_none_or(|wanted| {
                    node.label
                        .as_deref()
                        .is_some_and(|value| value.to_ascii_lowercase().contains(wanted))
                })
            })
            .filter(|node| region.is_none_or(|wanted| frame_intersects(node.frame, wanted, origin)))
            .map(|node| node.to_json(origin))
            .collect()
    }

    /// Apply non-structural AX notifications to the retained snapshot. Value,
    /// focus, and geometry notifications need only one descriptor read for the
    /// affected handle; child creation/removal and observer overflow return
    /// `None`, asking the caller to perform a bounded full walk. The returned
    /// state owns an `Arc` clone of the entry's AX-handle guard, so it remains
    /// safe to use after this method releases the cache lock.
    pub fn refresh_window(
        &self,
        pid: i32,
        window_id: u32,
        max_elements: usize,
        max_depth: usize,
        full_map: bool,
        max_ax_time_ms: Option<u64>,
    ) -> Option<CachedWindowState> {
        let events = self.take_dirty_events(pid, window_id);
        if events.is_empty() {
            return self.cached_window_for_policy(
                pid,
                window_id,
                max_elements,
                max_depth,
                full_map,
            );
        }
        if events.iter().any(|event| {
            matches!(event.kind, DirtyKind::Destroyed | DirtyKind::Unknown) || event.element == 0
        }) {
            return None;
        }

        // Clone the handles and metadata under the lock, then do AX IPC after
        // unlocking. The Arc owner prevents a concurrent replacement from
        // releasing the pointers while the refresh is in flight.
        let key = self.window_key(pid, window_id)?;
        let (mut nodes, owner, previous, revision, observer_supported, policy_ok) = {
            let windows = self.windows.lock().ok()?;
            let entry = windows.get(&key)?;
            (
                entry.nodes.clone(),
                entry.owned_elements.clone()?,
                entry.compact_nodes.clone(),
                entry.revision,
                entry.observer_supported,
                entry.max_elements == max_elements
                    && entry.max_depth == max_depth
                    && entry.full_map == full_map,
            )
        };
        if !policy_ok || !observer_supported {
            return None;
        }

        let mut ax_reads = 0_u64;
        let mut refresh_guards = Vec::new();
        let mut refresh_watch_guards = Vec::new();
        let visible_bounds = nodes
            .iter()
            .find(|node| node.role == "AXWindow")
            .and_then(|node| node.frame);
        for event in events {
            let Some(index) = nodes
                .iter()
                .position(|node| same_ax_element(node.element_ptr, event.element))
            else {
                return None;
            };
            let subtree = matches!(event.kind, DirtyKind::Children | DirtyKind::Layout);
            let refreshed = unsafe {
                super::tree::walk_subtree_with_options(
                    event.element as AXUIElementRef,
                    if subtree { max_elements } else { 1 },
                    if subtree { max_depth } else { 0 },
                    full_map,
                    visible_bounds,
                    max_ax_time_ms,
                )
            };
            ax_reads = ax_reads.saturating_add(refreshed.nodes_visited as u64);
            if subtree && refreshed.truncated {
                return None;
            }
            refresh_guards.push(refreshed.retain_nodes());
            refresh_watch_guards.push(refreshed.retain_watch_elements());
            if subtree {
                let root_depth = nodes[index].depth;
                let end = subtree_end(&nodes, index, root_depth);
                let old_count = nodes[index..end]
                    .iter()
                    .filter(|node| node.element_index.is_some())
                    .count();
                let replacement = rebase_subtree(refreshed.nodes, root_depth);
                let new_count = replacement
                    .iter()
                    .filter(|node| node.element_index.is_some())
                    .count();
                nodes.splice(index..end, replacement);
                reindex_nodes(&mut nodes, index, old_count, new_count);
            } else {
                let Some(mut replacement) = refreshed.nodes.into_iter().next() else {
                    return None;
                };
                replacement.element_index = nodes[index].element_index;
                replacement.parent_element_index = nodes[index].parent_element_index;
                replacement.depth = nodes[index].depth;
                nodes[index] = replacement;
            }
        }

        let mut windows = self.windows.lock().ok()?;
        let entry = windows.get_mut(&key)?;
        // A full-walk publisher may have won the race while AX IPC was in
        // flight. Do not overwrite its newer revision with stale metadata.
        if entry.revision != revision || !Arc::ptr_eq(entry.owned_elements.as_ref()?, &owner) {
            return None;
        }
        entry.revision = entry.revision.saturating_add(1);
        entry.nodes = nodes;
        let refreshed_nodes = entry.nodes.clone();
        entry.compact_nodes = compact_nodes(&refreshed_nodes, entry);
        entry.diff = diff_nodes(entry.revision, &previous, &entry.compact_nodes);
        entry.dirty = DirtyFlags::default();
        // Rebuild ownership from the final vector. This drops obsolete
        // handles instead of accumulating every historical subtree pointer.
        let mut refreshed_owner = owner.as_ref().clone_retained();
        for guard in refresh_guards {
            refreshed_owner.append(guard);
        }
        for guard in refresh_watch_guards {
            refreshed_owner.append(guard);
        }
        let refreshed_owner = Arc::new(refreshed_owner);
        entry.owned_elements = Some(refreshed_owner.clone());
        let mut state = CachedWindowState {
            key,
            revision: entry.revision,
            nodes: entry.nodes.clone(),
            compact_nodes: entry.compact_nodes.clone(),
            diff: entry.diff.clone(),
            dirty: entry.dirty,
            observer_supported: entry.observer_supported,
            cache_hit: false,
            ax_reads,
            owned_elements: refreshed_owner,
        };
        let observer_elements = state
            .owned_elements
            .pointers()
            .iter()
            .copied()
            .filter(|ptr| *ptr != 0)
            .collect::<Vec<_>>();
        drop(windows);
        let observer_supported = self.observer.register(
            ObserverKey {
                pid: key.pid,
                window_id: key.window_id,
                process_generation: key.process_generation,
            },
            observer_elements,
        );
        state.observer_supported = observer_supported;
        if let Ok(mut windows) = self.windows.lock() {
            if let Some(entry) = windows.get_mut(&key) {
                entry.observer_supported = observer_supported;
                if !observer_supported {
                    entry.dirty.children = true;
                }
            }
        }
        if !observer_supported {
            state.dirty.children = true;
        }
        Some(state)
    }

    /// Replace the snapshot for (pid, window_id) with the nodes from a fresh walk.
    pub fn update(&self, pid: i32, window_id: u32, nodes: &[AXNode]) {
        let elements: Vec<usize> = nodes
            .iter()
            .filter(|n| n.element_index.is_some())
            .map(|n| n.element_ptr)
            .inspect(|ptr| {
                if *ptr != 0 {
                    // Action dispatch owns an independent retain from the
                    // walk result and persistent metadata guard.
                    unsafe { CFRetain(*ptr as AXUIElementRef as CFTypeRef) };
                }
            })
            .collect();
        self.core
            .insert(CacheKey { pid, window_id }, CachedSnapshot { elements });
    }

    /// Look up + `CFRetain` the element for `element_index` in (pid, window_id),
    /// returning a guard that releases on drop. The retain happens **under the
    /// cache lock**, so a concurrent [`update`](Self::update) (which replaces
    /// the snapshot and drops its retains) cannot free the element between the
    /// lookup and the retain. Hold the returned guard for the entire AX action —
    /// this is what makes element actions safe when two sessions drive the same
    /// `(pid, window_id)`. Returns `None` if the index isn't cached.
    pub fn get_element_retained(
        &self,
        pid: i32,
        window_id: u32,
        element_index: usize,
    ) -> Option<RetainedElement> {
        self.core
            .with_snapshot(&CacheKey { pid, window_id }, |s| {
                let ptr = s.elements.get(element_index).copied()?;
                if ptr != 0 {
                    // Safety: still inside `with_snapshot`'s lock, so the
                    // snapshot (and thus this CFTypeRef) is alive right now.
                    unsafe { CFRetain(ptr as AXUIElementRef as CFTypeRef) };
                }
                Some(RetainedElement(ptr))
            })
            .flatten()
    }

    /// Number of indexed elements for (pid, window_id), or 0 if not cached.
    pub fn element_count(&self, pid: i32, window_id: u32) -> usize {
        self.core
            .with_snapshot(&CacheKey { pid, window_id }, |s| s.elements.len())
            .unwrap_or(0)
    }
}

fn compact_nodes(nodes: &[AXNode], entry: &mut WindowEntry) -> Vec<CompactNode> {
    let mut ids_by_index = HashMap::new();
    let mut compact = Vec::new();
    for node in nodes {
        let label = super::tree::preferred_label(
            node.title.as_deref(),
            node.description.as_deref(),
            node.value.as_deref(),
            node.identifier.as_deref(),
        )
        .map(str::to_owned);
        // Compact output is intentionally actionable-or-labeled. Keep the
        // legacy node vector untouched so integer indices remain valid.
        if node.element_index.is_none() && label.is_none() {
            continue;
        }
        let id = stable_id(node, entry);
        if let Some(index) = node.element_index {
            ids_by_index.insert(index, id.clone());
        }
        compact.push(CompactNode {
            id,
            role: node.role.clone(),
            label,
            value: node.value.clone(),
            actions: node.actions.clone(),
            frame: node.frame,
            element_index: node.element_index,
            parent_id: node
                .parent_element_index
                .and_then(|index| ids_by_index.get(&index).cloned()),
        });
    }
    compact
}

fn stable_id(node: &AXNode, entry: &mut WindowEntry) -> String {
    if node.element_index.is_some() && node.element_ptr != 0 {
        // The walker retains actionable nodes. IdentityRegistry adds one
        // bounded retain of its own and compares AX handles with CFEqual, so
        // wrapper addresses and mutable labels do not churn the serialized ID.
        return unsafe {
            entry.identity_registry.stable_id(
                node.element_ptr as AXUIElementRef,
                node.identifier.as_deref(),
                &node.role,
                None,
            )
        };
    }
    if node.element_ptr != 0 {
        if let Some(id) = entry.identity_ids.get(&node.element_ptr) {
            return id.clone();
        }
    }
    // AXIdentifier is the only public semantic identity macOS provides. Use
    // it when present; geometry, title, and value deliberately do not enter
    // the fallback so those fields may change without changing the id.
    let id = if let Some(identifier) = node.identifier.as_deref().filter(|s| !s.is_empty()) {
        format!("ax-ident:{}", stable_hash(identifier))
    } else {
        entry.next_identity = entry.next_identity.saturating_add(1);
        format!("ax-node:{}", entry.next_identity)
    };
    if node.element_ptr != 0 {
        entry.identity_ids.insert(node.element_ptr, id.clone());
    }
    id
}

fn stable_hash(value: &str) -> String {
    // FNV-1a is deterministic across daemon restarts and avoids the random
    // per-process seed used by Rust's DefaultHasher.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn diff_nodes(revision: u64, previous: &[CompactNode], current: &[CompactNode]) -> SnapshotDiff {
    let old = previous
        .iter()
        .map(|node| (&node.id, node))
        .collect::<HashMap<_, _>>();
    let new = current
        .iter()
        .map(|node| (&node.id, node))
        .collect::<HashMap<_, _>>();
    let mut added = Vec::new();
    let mut updated = Vec::new();
    for node in current {
        match old.get(&node.id) {
            None => added.push(node.id.clone()),
            Some(previous) if *previous != node => updated.push(node.id.clone()),
            _ => {}
        }
    }
    let removed = previous
        .iter()
        .filter(|node| !new.contains_key(&node.id))
        .map(|node| node.id.clone())
        .collect();
    SnapshotDiff {
        from_revision: (!previous.is_empty()).then_some(revision.saturating_sub(1)),
        to_revision: revision,
        added,
        removed,
        updated,
    }
}

fn frame_intersects(frame: Option<[f64; 4]>, region: [f64; 4], origin: [f64; 2]) -> bool {
    let Some(frame) = frame else { return false };
    let x = frame[0] - origin[0];
    let y = frame[1] - origin[1];
    x < region[0] + region[2]
        && x + frame[2] > region[0]
        && y < region[1] + region[3]
        && y + frame[3] > region[1]
}

fn same_ax_element(left: usize, right: usize) -> bool {
    if left == 0 || right == 0 {
        return left == right;
    }
    unsafe {
        core_foundation::base::CFEqual(
            left as AXUIElementRef as CFTypeRef,
            right as AXUIElementRef as CFTypeRef,
        ) != 0
    }
}

fn subtree_end(nodes: &[AXNode], start: usize, root_depth: usize) -> usize {
    nodes
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, node)| node.depth <= root_depth)
        .map(|(index, _)| index)
        .unwrap_or(nodes.len())
}

fn rebase_subtree(mut nodes: Vec<AXNode>, root_depth: usize) -> Vec<AXNode> {
    for node in &mut nodes {
        node.depth = root_depth.saturating_add(node.depth);
        node.parent_element_index = None;
    }
    nodes
}

fn reindex_nodes(nodes: &mut [AXNode], _start: usize, _old_count: usize, _new_count: usize) {
    let mut ancestors = Vec::new();
    let mut next = 0usize;
    for node in nodes.iter_mut() {
        while ancestors
            .last()
            .is_some_and(|(depth, _): &(usize, usize)| *depth >= node.depth)
        {
            ancestors.pop();
        }
        node.parent_element_index = ancestors.last().map(|(_, index)| *index);
        if node.element_index.is_some() {
            node.element_index = Some(next);
            ancestors.push((node.depth, next));
            next = next.saturating_add(1);
        }
    }
}

/// Produce a one-shot state when proc_pidinfo cannot establish a live process
/// generation.  It is deliberately never inserted into the persistent map and
/// is marked dirty/observer-unsupported, forcing the next call to walk again.
fn transient_window_state(
    pid: i32,
    window_id: u32,
    nodes: &[AXNode],
    ax_reads: u64,
    owned_elements: RetainedNodeGuard,
) -> CachedWindowState {
    let key = WindowCacheKey {
        pid,
        window_id,
        process_generation: 0,
    };
    let mut entry = WindowEntry {
        key,
        revision: 1,
        observer_supported: false,
        dirty: DirtyFlags {
            children: true,
            ..DirtyFlags::default()
        },
        ..WindowEntry::default()
    };
    entry.nodes = nodes.to_vec();
    entry.compact_nodes = compact_nodes(nodes, &mut entry);
    entry.diff = diff_nodes(entry.revision, &[], &entry.compact_nodes);
    let owner = Arc::new(owned_elements);
    CachedWindowState {
        key,
        revision: entry.revision,
        nodes: entry.nodes,
        compact_nodes: entry.compact_nodes,
        diff: entry.diff,
        dirty: entry.dirty,
        observer_supported: false,
        cache_hit: false,
        ax_reads,
        owned_elements: owner,
    }
}

impl Default for ElementCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::{CFGetRetainCount, CFRetain, TCFType};
    use core_foundation::string::CFString;

    // An AXNode carrying a raw CFTypeRef pointer as if it were an element.
    // A long, dynamic string is heap-allocated (not a tagged-pointer CFString),
    // so CFGetRetainCount is reliable.
    fn node_with_ptr(ptr: usize) -> AXNode {
        AXNode {
            element_index: Some(0),
            role: String::new(),
            title: None,
            value: None,
            description: None,
            identifier: None,
            help: None,
            actions: Vec::new(),
            element_ptr: ptr,
            depth: 0,
            parent_element_index: None,
            frame: None,
        }
    }

    /// The crash this guards against: while a click holds an element pointer,
    /// a concurrent `get_window_state` replaces the snapshot and its `Drop`
    /// `CFRelease`s the element to zero — freeing it under the in-flight click
    /// (use-after-free → `EXC_BREAKPOINT` in `AXUIElementCopyActionNames`).
    /// `get_element_retained` takes an extra retain under the lock so the
    /// element stays alive across the replace. This asserts that accounting.
    #[test]
    fn retained_element_survives_concurrent_snapshot_replace() {
        let s = CFString::new("cmux-cua-uaf-test-element-placeholder");
        let ptr = s.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(ptr as CFTypeRef) };

        // The action cache takes an independent retain before the walk result
        // is allowed to drop, then CachedSnapshot::drop releases it.
        let cache = ElementCache::new();
        cache.update(1, 2, &[node_with_ptr(ptr)]);
        assert_eq!(
            unsafe { CFGetRetainCount(ptr as CFTypeRef) },
            base + 1,
            "cache owns one retain"
        );

        // Borrow the element out for an action.
        let guard = cache
            .get_element_retained(1, 2, 0)
            .expect("element is cached");
        assert_eq!(
            unsafe { CFGetRetainCount(ptr as CFTypeRef) },
            base + 2,
            "guard adds a retain"
        );

        // Concurrent get_window_state replaces the snapshot → old one dropped →
        // CFRelease of the cache's retain. The guard's retain must remain.
        cache.update(1, 2, &[]);
        assert_eq!(
            unsafe { CFGetRetainCount(ptr as CFTypeRef) },
            base + 1,
            "after the replace, only the guard's retain remains — the element is still ALIVE \
             (pre-fix this would drop to `base` and a real AX element with no other owner would be freed)"
        );

        drop(guard);
        assert_eq!(
            unsafe { CFGetRetainCount(ptr as CFTypeRef) },
            base,
            "guard drop releases its retain"
        );
    }

    /// A missing index returns None without retaining anything.
    #[test]
    fn missing_index_returns_none() {
        let cache = ElementCache::new();
        assert!(cache.get_element_retained(1, 2, 0).is_none());
        cache.update(1, 2, &[]);
        assert!(cache.get_element_retained(1, 2, 5).is_none());
    }

    #[test]
    fn compact_diff_is_explicitly_revision_scoped() {
        let old = vec![CompactNode {
            id: "ax-node:1".into(),
            role: "AXButton".into(),
            label: Some("Before".into()),
            value: None,
            actions: vec!["AXPress".into()],
            frame: Some([1.0, 2.0, 3.0, 4.0]),
            element_index: Some(0),
            parent_id: None,
        }];
        let new = vec![CompactNode {
            label: Some("After".into()),
            ..old[0].clone()
        }];
        let diff = diff_nodes(7, &old, &new);
        assert_eq!(diff.from_revision, Some(6));
        assert_eq!(diff.to_revision, 7);
        assert_eq!(diff.updated, vec!["ax-node:1"]);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn warm_cache_reports_zero_ax_reads() {
        let cache = ElementCache::new();
        let pid = std::process::id() as i32;
        let key = cache
            .window_key(pid, 2)
            .expect("current process has a start stamp");
        if let Ok(mut windows) = cache.windows.lock() {
            windows.insert(
                key,
                WindowEntry {
                    key,
                    revision: 3,
                    observer_supported: true,
                    owned_elements: Some(Arc::new(RetainedNodeGuard::empty())),
                    ..WindowEntry::default()
                },
            );
        }
        let snapshot = cache.cached_window(pid, 2).expect("clean cache hit");
        assert!(snapshot.cache_hit);
        assert_eq!(snapshot.ax_reads, 0);
        assert!(snapshot.diff.added.is_empty());
        assert!(snapshot.diff.updated.is_empty());
        assert!(snapshot.diff.removed.is_empty());
    }
}
