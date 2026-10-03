//! Long-lived Accessibility observers used by the per-window AX cache.
//!
//! `AXObserver` callbacks are deliberately tiny.  They do not read an AX
//! attribute, walk a child list, or call into application code.  They only
//! classify the notification and append a small dirty record to a queue.  A
//! cache refresh consumes that queue on its normal worker thread, which keeps
//! re-entrant AX IPC out of the callback and makes callback latency bounded.

use super::bindings::{kAXErrorSuccess, AXError, AXUIElementCreateApplication, AXUIElementRef};
use core_foundation::base::TCFType;
use core_foundation::base::{CFRelease, CFTypeRef};
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRemoveSource,
    CFRunLoopRunInMode, CFRunLoopSourceRef,
};
use core_foundation::string::{CFString, CFStringRef};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[repr(C)]
struct __AXObserver(c_void);
type AXObserverRef = *mut __AXObserver;
type AXObserverCallback = extern "C" fn(
    observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    context: *mut c_void,
);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXObserverCreate(
        application: i32,
        callback: AXObserverCallback,
        observer: *mut AXObserverRef,
    ) -> AXError;
    fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) -> AXError;
    fn AXObserverRemoveNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
    ) -> AXError;
    fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> CFRunLoopSourceRef;
}

/// Cache scope.  A generation is allocated by the cache when a pid is first
/// observed and is replaced when the process disappears or an AX handle fails
/// as invalid.  This prevents a recycled pid from consuming an old snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObserverKey {
    pub pid: i32,
    pub window_id: u32,
    pub process_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirtyKind {
    Value,
    Children,
    Focus,
    Layout,
    Destroyed,
    Unknown,
}

impl DirtyKind {
    fn from_notification(name: &str) -> Self {
        match name {
            "AXValueChanged" | "AXTitleChanged" => Self::Value,
            "AXSelectedChildrenChanged"
            | "AXChildrenChanged"
            | "AXUIElementCreated"
            | "AXCreated"
            | "AXSelectedRowsChanged"
            | "AXSelectedColumnsChanged"
            | "AXRowCountChanged"
            | "AXRowExpanded"
            | "AXRowCollapsed" => Self::Children,
            "AXFocusedUIElementChanged" | "AXFocusedWindowChanged" => Self::Focus,
            "AXLayoutChanged" | "AXLayoutComplete" | "AXLoadComplete" | "AXMoved" | "AXResized"
            | "AXWindowMoved" | "AXWindowResized" => Self::Layout,
            "AXUIElementDestroyed" => Self::Destroyed,
            _ => Self::Unknown,
        }
    }
}

/// A notification with an owned AX element retain. AXObserver gives the
/// callback a borrowed element pointer; retaining it here keeps the handle
/// valid until the cache consumes the event, even if the app destroys a node
/// immediately after emitting its notification.
#[derive(Debug)]
pub struct DirtyEvent {
    pub key: ObserverKey,
    pub element: usize,
    pub kind: DirtyKind,
    retained: Option<RetainedElement>,
}

impl DirtyEvent {
    fn from_callback(key: ObserverKey, element: AXUIElementRef, kind: DirtyKind) -> Self {
        let retained = (!element.is_null()).then(|| {
            // The retain is intentionally the only Core Foundation operation
            // performed by the callback. It does not query the target app.
            unsafe { core_foundation::base::CFRetain(element as CFTypeRef) };
            RetainedElement(element as usize)
        });
        Self {
            key,
            element: element as usize,
            kind,
            retained,
        }
    }

    #[cfg(test)]
    fn test(key: ObserverKey, element: usize, kind: DirtyKind) -> Self {
        Self {
            key,
            element,
            kind,
            retained: None,
        }
    }
}

#[derive(Debug)]
struct RetainedElement(usize);

unsafe impl Send for RetainedElement {}

impl Drop for RetainedElement {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { CFRelease(self.0 as AXUIElementRef as CFTypeRef) };
        }
    }
}

#[derive(Default)]
pub struct DirtyQueue {
    events: Mutex<VecDeque<DirtyEvent>>,
}

const MAX_DIRTY_EVENTS: usize = 4096;

impl DirtyQueue {
    fn push(&self, event: DirtyEvent) {
        let Ok(mut events) = self.events.lock() else {
            return;
        };
        // Coalesce duplicate events for one element while preserving the
        // strongest structural kind.  This keeps notification storms from
        // turning into repeated subtree reads.
        if let Some(existing) = events
            .iter_mut()
            .find(|existing| existing.key == event.key && existing.element == event.element)
        {
            existing.kind = merge_kind(existing.kind, event.kind);
            return;
        }
        if events.len() >= MAX_DIRTY_EVENTS {
            // Drop the incoming retained handle and collapse the overflow to
            // one structural marker. The cache will take its bounded refresh
            // safety path instead of retaining an unbounded notification log.
            if !events
                .iter()
                .any(|existing| existing.key == event.key && existing.kind == DirtyKind::Unknown)
            {
                events.push_back(DirtyEvent {
                    key: event.key,
                    element: 0,
                    kind: DirtyKind::Unknown,
                    retained: None,
                });
            }
            return;
        }
        events.push_back(event);
    }

    pub fn drain(&self, key: ObserverKey) -> Vec<DirtyEvent> {
        let Ok(mut events) = self.events.lock() else {
            return Vec::new();
        };
        let mut selected = Vec::new();
        let mut retained = VecDeque::with_capacity(events.len());
        while let Some(event) = events.pop_front() {
            if event.key == key {
                selected.push(event);
            } else {
                retained.push_back(event);
            }
        }
        *events = retained;
        selected
    }

    pub fn len(&self) -> usize {
        self.events.lock().map(|events| events.len()).unwrap_or(0)
    }
}

fn merge_kind(left: DirtyKind, right: DirtyKind) -> DirtyKind {
    // Structural events dominate value/focus updates.  Destroyed is kept
    // distinct so the cache can evict a handle before the next AX read.
    match (left, right) {
        (DirtyKind::Destroyed, _) | (_, DirtyKind::Destroyed) => DirtyKind::Destroyed,
        (DirtyKind::Children, _) | (_, DirtyKind::Children) => DirtyKind::Children,
        (DirtyKind::Layout, _) | (_, DirtyKind::Layout) => DirtyKind::Layout,
        (DirtyKind::Value, _) | (_, DirtyKind::Value) => DirtyKind::Value,
        (DirtyKind::Focus, DirtyKind::Focus) => DirtyKind::Focus,
        _ => DirtyKind::Unknown,
    }
}

struct ObserverContext {
    key: ObserverKey,
    queue: Arc<DirtyQueue>,
}

struct Registration {
    observer: AXObserverRef,
    app: AXUIElementRef,
    source: CFRunLoopSourceRef,
    context: Box<ObserverContext>,
    /// Keep the exact retained AX references and CF notification strings that
    /// were accepted by AXObserverAddNotification for symmetric cleanup.
    watched: Vec<(AXUIElementRef, CFString)>,
    /// One extra retain per target, independent of the number of notification
    /// names registered for it. This keeps target handles alive until every
    /// watcher has been removed on the observer thread.
    owned_elements: Vec<AXUIElementRef>,
    targets: Vec<usize>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        unsafe {
            if !self.source.is_null() {
                CFRunLoopRemoveSource(CFRunLoopGetCurrent(), self.source, kCFRunLoopDefaultMode);
            }
            for (element, notification) in &self.watched {
                let _ = AXObserverRemoveNotification(
                    self.observer,
                    *element,
                    notification.as_concrete_TypeRef(),
                );
            }
            for element in &self.owned_elements {
                CFRelease(*element as CFTypeRef);
            }
            if !self.observer.is_null() {
                CFRelease(self.observer as CFTypeRef);
            }
            if !self.app.is_null() {
                CFRelease(self.app as CFTypeRef);
            }
        }
    }
}

enum Command {
    Register {
        key: ObserverKey,
        elements: Vec<usize>,
        response: Sender<bool>,
    },
    Remove(ObserverKey),
    Stop,
}

/// A process-lifetime observer owner.  AXObserver's run-loop source must be
/// serviced continuously; putting it on this private thread prevents cache
/// refreshes from depending on whichever executor happened to call the tool.
pub struct ObserverHub {
    tx: Sender<Command>,
    queue: Arc<DirtyQueue>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ObserverHub {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let queue = Arc::new(DirtyQueue::default());
        let thread_queue = queue.clone();
        let thread = thread::Builder::new()
            .name("cmux-cua-ax-observer".to_owned())
            .spawn(move || observer_thread(rx, thread_queue))
            .expect("failed to start AX observer thread");
        Arc::new(Self {
            tx,
            queue,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn queue(&self) -> Arc<DirtyQueue> {
        self.queue.clone()
    }

    /// Register the window and known descendants for value, focus, children,
    /// and layout notifications.  Returns false when AXObserver cannot be
    /// created or no requested watcher was accepted; callers then use their
    /// bounded refresh safety net and report `observer_supported:false`.
    pub fn register(&self, key: ObserverKey, elements: Vec<usize>) -> bool {
        let (response_tx, response_rx) = mpsc::channel();
        if self
            .tx
            .send(Command::Register {
                key,
                elements,
                response: response_tx,
            })
            .is_err()
        {
            return false;
        }
        response_rx.recv().unwrap_or(false)
    }

    pub fn remove(&self, key: ObserverKey) {
        let _ = self.tx.send(Command::Remove(key));
    }

    pub fn drain(&self, key: ObserverKey) -> Vec<DirtyEvent> {
        self.queue.drain(key)
    }
}

impl Drop for ObserverHub {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(thread) = self.thread.get_mut().ok().and_then(Option::take) {
            let _ = thread.join();
        }
    }
}

fn observer_thread(rx: Receiver<Command>, queue: Arc<DirtyQueue>) {
    let mut registrations: HashMap<ObserverKey, Registration> = HashMap::new();
    let mut stopping = false;
    while !stopping {
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::Register {
                    key,
                    elements,
                    response,
                } => {
                    let mut elements = elements;
                    elements.sort_unstable();
                    elements.dedup();
                    if registrations
                        .get(&key)
                        .is_some_and(|registration| registration.targets == elements)
                    {
                        let _ = response.send(true);
                        continue;
                    }
                    registrations.remove(&key);
                    let supported = create_registration(key, elements, queue.clone())
                        .map(|registration| {
                            registrations.insert(key, registration);
                            true
                        })
                        .unwrap_or(false);
                    let _ = response.send(supported);
                }
                Command::Remove(key) => {
                    registrations.remove(&key);
                }
                Command::Stop => {
                    stopping = true;
                    break;
                }
            }
        }
        if stopping {
            break;
        }
        if registrations.is_empty() {
            // CFRunLoopRunInMode returns immediately when no source is
            // attached. Avoid a hot spin before the first registration while
            // retaining a bounded command handoff latency.
            thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        unsafe {
            // This timeout is only the command polling cadence.  AX calls
            // never run in the callback and are not bounded by this loop.
            let _ = CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 0);
        }
    }
    registrations.clear();
}

fn create_registration(
    key: ObserverKey,
    elements: Vec<usize>,
    queue: Arc<DirtyQueue>,
) -> Option<Registration> {
    unsafe {
        let target_addresses = elements.clone();
        let app = AXUIElementCreateApplication(key.pid);
        if app.is_null() {
            return None;
        }
        let mut observer = std::ptr::null_mut();
        let error = AXObserverCreate(key.pid, observer_callback, &mut observer);
        if error != kAXErrorSuccess || observer.is_null() {
            CFRelease(app as CFTypeRef);
            return None;
        }
        let source = AXObserverGetRunLoopSource(observer);
        if source.is_null() {
            CFRelease(observer as CFTypeRef);
            CFRelease(app as CFTypeRef);
            return None;
        }
        let mut context = Box::new(ObserverContext { key, queue });
        let context_ptr = context.as_mut() as *mut ObserverContext as *mut c_void;
        let mut watched = Vec::new();
        let mut owned_elements = Vec::new();
        let mut owned_addresses = std::collections::HashSet::new();
        // Register on the app root as well as each currently-known element.
        // Chromium often emits structural notifications only on the nearest
        // AXWebArea/AXGroup, so watching descendants is necessary for local
        // subtree invalidation.
        let mut targets: Vec<AXUIElementRef> = Vec::with_capacity(elements.len() + 1);
        targets.push(app);
        targets.extend(
            elements
                .into_iter()
                .filter_map(|ptr| (ptr != 0).then_some(ptr as AXUIElementRef)),
        );
        let notifications = [
            "AXValueChanged",
            "AXTitleChanged",
            "AXSelectedChildrenChanged",
            "AXChildrenChanged",
            "AXUIElementCreated",
            "AXCreated",
            "AXSelectedRowsChanged",
            "AXSelectedColumnsChanged",
            "AXUIElementDestroyed",
            "AXFocusedUIElementChanged",
            "AXFocusedWindowChanged",
            "AXLayoutChanged",
            "AXLayoutComplete",
            "AXLoadComplete",
            "AXMoved",
            "AXResized",
            "AXWindowMoved",
            "AXWindowResized",
        ];
        for target in targets {
            if target != app && owned_addresses.insert(target as usize) {
                // The cache owns its snapshot handles, but registrations may
                // outlive a snapshot replacement until RemoveNotification has
                // run on this thread. Hold an observer-owned retain as well.
                core_foundation::base::CFRetain(target as CFTypeRef);
                owned_elements.push(target);
            }
            for name in notifications {
                let notification = CFString::new(name);
                if AXObserverAddNotification(
                    observer,
                    target,
                    notification.as_concrete_TypeRef(),
                    context_ptr,
                ) == kAXErrorSuccess
                {
                    watched.push((target, notification));
                }
            }
        }
        if watched.is_empty() {
            CFRelease(observer as CFTypeRef);
            CFRelease(app as CFTypeRef);
            for element in owned_elements {
                CFRelease(element as CFTypeRef);
            }
            return None;
        }
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
        Some(Registration {
            observer,
            app,
            source,
            context,
            watched,
            owned_elements,
            targets: target_addresses,
        })
    }
}

extern "C" fn observer_callback(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // This callback intentionally performs no AX reads.  Converting the
    // notification name is a local CFString operation; all tree work happens
    // after the event has been queued and the callback has returned.
    let context = unsafe { &*(context as *const ObserverContext) };
    let name = unsafe { CFString::wrap_under_get_rule(notification as _) }.to_string();
    context.queue.push(DirtyEvent::from_callback(
        context.key,
        element,
        DirtyKind::from_notification(&name),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_coalesces_and_prefers_structural_events() {
        let queue = DirtyQueue::default();
        let key = ObserverKey {
            pid: 1,
            window_id: 2,
            process_generation: 3,
        };
        queue.push(DirtyEvent::test(key, 4, DirtyKind::Value));
        queue.push(DirtyEvent::test(key, 4, DirtyKind::Layout));
        let events = queue.drain(key);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, DirtyKind::Layout);
    }

    #[test]
    fn queue_keeps_window_scopes_isolated() {
        let queue = DirtyQueue::default();
        let first = ObserverKey {
            pid: 1,
            window_id: 2,
            process_generation: 3,
        };
        let second = ObserverKey {
            pid: 1,
            window_id: 7,
            process_generation: 3,
        };
        queue.push(DirtyEvent::test(first, 4, DirtyKind::Value));
        queue.push(DirtyEvent::test(second, 5, DirtyKind::Focus));
        assert_eq!(queue.drain(first).len(), 1);
        assert_eq!(queue.drain(second).len(), 1);
    }

    #[test]
    fn notification_classification_covers_requested_mutations() {
        assert_eq!(
            DirtyKind::from_notification("AXValueChanged"),
            DirtyKind::Value
        );
        assert_eq!(
            DirtyKind::from_notification("AXFocusedUIElementChanged"),
            DirtyKind::Focus
        );
        assert_eq!(
            DirtyKind::from_notification("AXLayoutChanged"),
            DirtyKind::Layout
        );
        assert_eq!(
            DirtyKind::from_notification("AXCreated"),
            DirtyKind::Children
        );
        assert_eq!(
            DirtyKind::from_notification("AXUIElementCreated"),
            DirtyKind::Children
        );
    }
}
