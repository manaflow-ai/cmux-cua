//! Collision-safe identity handles for retained AX elements.
//!
//! AX object addresses are only a process-local observation and cannot be used
//! as durable element IDs. `IdentityRegistry` keeps a bounded set of retained
//! AX references, compares candidates with Core Foundation equality, and gives
//! each distinct object a window-scoped handle. Semantic labels are metadata;
//! they never collapse two distinct AX objects into one ID.

use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};
use std::collections::VecDeque;

use super::bindings::AXUIElementRef;

const DEFAULT_MAX_HANDLES: usize = 4096;

struct Entry {
    element: AXUIElementRef,
    id: String,
    /// Kept for diagnostics and future reconciliation. It is intentionally not
    /// an identity key: labels and identifiers can be duplicated or mutate.
    semantic: String,
}

/// Retained AX identity handles scoped to one process/window pair.
pub struct IdentityRegistry {
    process_id: i32,
    window_id: u32,
    process_generation: u64,
    max_handles: usize,
    next_id: u64,
    entries: VecDeque<Entry>,
}

// AXUIElement references are Core Foundation objects and may be retained or
// released from worker threads. IdentityRegistry is always accessed behind
// ElementCache's mutex, so no two threads mutate its registry concurrently.
unsafe impl Send for IdentityRegistry {}

impl IdentityRegistry {
    pub fn new(process_id: i32, window_id: u32) -> Self {
        Self::with_scope(process_id, window_id, 0, DEFAULT_MAX_HANDLES)
    }

    pub fn with_capacity(process_id: i32, window_id: u32, max_handles: usize) -> Self {
        Self::with_scope(process_id, window_id, 0, max_handles)
    }

    pub fn with_scope(
        process_id: i32,
        window_id: u32,
        process_generation: u64,
        max_handles: usize,
    ) -> Self {
        Self {
            process_id,
            window_id,
            process_generation,
            max_handles: max_handles.max(1),
            next_id: 0,
            entries: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return the stable handle for an AX object, retaining a reference while
    /// it is in the registry. Distinct objects with duplicate semantic labels
    /// receive distinct handles; a label change on an equal object preserves
    /// its existing handle.
    ///
    /// # Safety
    /// `element` must be a valid `AXUIElementRef` for the registry's process.
    pub unsafe fn stable_id(
        &mut self,
        element: AXUIElementRef,
        semantic_identifier: Option<&str>,
        role: &str,
        parent_id: Option<&str>,
    ) -> String {
        if element.is_null() {
            return format!("ax-{}-{}-invalid", self.process_id, self.window_id);
        }

        if let Some(existing) = self
            .entries
            .iter()
            .find(|entry| CFEqual(entry.element as CFTypeRef, element as CFTypeRef) != 0)
        {
            return existing.id.clone();
        }

        while self.entries.len() >= self.max_handles {
            if let Some(evicted) = self.entries.pop_front() {
                CFRelease(evicted.element as CFTypeRef);
            }
        }

        let id = format!(
            "ax-{}-{}-{}-{}",
            self.process_id, self.window_id, self.process_generation, self.next_id
        );
        self.next_id = self.next_id.saturating_add(1);
        CFRetain(element as CFTypeRef);
        let semantic = format!(
            "{}|{}|{}",
            semantic_identifier.unwrap_or_default(),
            role,
            parent_id.unwrap_or_default()
        );
        self.entries.push_back(Entry {
            element,
            id: id.clone(),
            semantic,
        });
        id
    }
}

impl Drop for IdentityRegistry {
    fn drop(&mut self) {
        while let Some(entry) = self.entries.pop_front() {
            unsafe { CFRelease(entry.element as CFTypeRef) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::{base::TCFType, string::CFString};

    #[test]
    fn handles_are_scoped_and_capacity_is_bounded() {
        let mut registry = IdentityRegistry::with_capacity(42, 7, 2);
        assert_eq!(registry.process_id, 42);
        assert_eq!(registry.window_id, 7);
        assert_eq!(registry.max_handles, 2);
        let first = CFString::new("first");
        let second = CFString::new("second");
        let third = CFString::new("third");
        unsafe {
            registry.stable_id(
                first.as_concrete_TypeRef() as AXUIElementRef,
                Some("same"),
                "AXButton",
                None,
            );
            registry.stable_id(
                second.as_concrete_TypeRef() as AXUIElementRef,
                Some("same"),
                "AXButton",
                None,
            );
            assert_eq!(registry.len(), 2);
            registry.stable_id(
                third.as_concrete_TypeRef() as AXUIElementRef,
                Some("same"),
                "AXButton",
                None,
            );
        }
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn semantic_duplicate_ids_are_not_collapsed() {
        let mut registry = IdentityRegistry::new(42, 7);
        let first = CFString::new("first AX object");
        let second = CFString::new("second AX object");
        let (first_id, second_id) = unsafe {
            (
                registry.stable_id(
                    first.as_concrete_TypeRef() as AXUIElementRef,
                    Some("OK"),
                    "AXButton",
                    None,
                ),
                registry.stable_id(
                    second.as_concrete_TypeRef() as AXUIElementRef,
                    Some("OK"),
                    "AXButton",
                    None,
                ),
            )
        };
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn label_changes_are_metadata_not_identity() {
        let mut registry = IdentityRegistry::new(42, 7);
        let first = CFString::new("same AX object");
        let second = CFString::new("same AX object");
        let (before, after) = unsafe {
            (
                registry.stable_id(
                    first.as_concrete_TypeRef() as AXUIElementRef,
                    Some("Old label"),
                    "AXButton",
                    None,
                ),
                registry.stable_id(
                    second.as_concrete_TypeRef() as AXUIElementRef,
                    Some("New label"),
                    "AXButton",
                    None,
                ),
            )
        };
        assert_eq!(before, after);
    }

    #[test]
    fn drop_releases_retained_entries() {
        let mut registry = IdentityRegistry::new(42, 7);
        let element = CFString::new(
            "a sufficiently long AX identity placeholder to measure owned retain accounting",
        );
        let ptr = element.as_concrete_TypeRef() as CFTypeRef;
        let baseline = unsafe { core_foundation::base::CFGetRetainCount(ptr) };
        unsafe {
            registry.stable_id(
                element.as_concrete_TypeRef() as AXUIElementRef,
                Some("label"),
                "AXButton",
                None,
            );
        }
        assert_eq!(registry.len(), 1);
        assert_eq!(
            unsafe { core_foundation::base::CFGetRetainCount(ptr) },
            baseline + 1
        );
        drop(registry);
        assert_eq!(
            unsafe { core_foundation::base::CFGetRetainCount(ptr) },
            baseline
        );
    }
}
