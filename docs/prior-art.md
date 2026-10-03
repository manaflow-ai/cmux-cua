# macOS Electron accessibility and click prior art

Surveyed 2026-10-03, before the cache implementation. Sources below are pinned to
the inspected revision. “Accepted” AX actions are not evidence that a click
changed the application. Public screenshot examples do not establish how the
closed Codex desktop app implements accessibility.

| Project and license | Full Electron tree | Click delivery | Verification | Tree caching |
| --- | --- | --- | --- | --- |
| [trycua/cua](https://github.com/trycua/cua/tree/e6127c6412eb2348f71ac40c70d088f6ca115a60/libs/cua-driver), MIT | Native `ax/enablement.rs` opts in with AXManualAccessibility, falls back to AXEnhancedUserInterface only when unsupported, and settles for AXWebArea. `ax/tree.rs` unions app children/windows, deduplicates with CFEqual, and binds the requested window. Individual attribute reads; bounded DFS. | AXPress/AXShowMenu, target-window hit testing, background input, explicit foreground fallback. | Exact-target delayed AXValue/AXSelected observations; generic accepted actions can remain unverified. | Snapshot/token store and invalidation, but no observer-backed incremental tree cache. |
| [Peekaboo](https://github.com/openclaw/Peekaboo/tree/910f413349c65476a3945dd08364ae814eeddaef/Core/PeekabooAutomationKit/Sources/PeekabooAutomationKit/Services/UI), MIT | AXTreeCollector scopes the window and guards cycles; AXDescriptorReader batches 14 attributes with CopyMultipleAttributeValues and handles per-attribute errors. Small frames are excluded. | AX actions and a background input driver with target/process receipts; explicit foreground recovery. | Target/process-generation-bound mutation observations. Focus receipts alone do not prove action effects; indeterminate outcomes are explicit. | ElementDetectionCache has a 1.5-second TTL, process-start identity, and invalidation watermark; incomplete results are not cached. |
| [AXorcist](https://github.com/openclaw/AXorcist/tree/1668298badaee5afafbd71f2d4867b2d2260fc6e/Sources/AXorcist/Core), MIT | Element+Hierarchy combines children/windows, visible/web/row/tab alternatives and focused elements; CFEqual/CFHash deduplication. Traversal offers DFS/BFS and descent predicates. | AX action/attribute primitives; not a universal background-click postcondition. | Caller-defined queries/readback; no automatic proof for every click found. | AXObserverCenter reuses process-generation-scoped observers on a serial owner; application lifecycle monitor removes stale registrations. No compact per-window snapshot diff found. |
| [Terminator, historical macOS](https://github.com/mediar-ai/terminator/blob/220e8b630f22e665b9384614859035d8a987fca3/crates/terminator/src/platforms/macos.rs), MIT | Windows/main-window-first recursive children and individual attributes. No Electron opt-in found. Current [73a381c](https://github.com/mediar-ai/terminator/tree/73a381c0c1c33eda55f2c0ecb1d918bf5ec7561a) removed macOS/Linux support. | Browser name heuristic chooses global HID clicks; other elements try AXPress, AXClick, then mouse. | No effect verifier found. | Application handles only; element hashes incorporate labels/geometry and do not meet stable snapshot identity requirements. |
| [Hammerspoon](https://github.com/Hammerspoon/hammerspoon/tree/23e387e2805a9890066366e0ac96c71b27f0cfd5/extensions/axuielement), MIT | Cancellable/yielding breadth-first elementSearch, CFEqual identity, batched allAttributeValues, elementAtPosition. No Chromium accessibility opt-in found. | AX performAction or global CGEvent eventtap clicks. | Documentation explicitly says action acceptance does not prove success; caller supplies readback. | observer.m exposes AXObserver notification registration/run-loop lifecycle, including value/focus/layout/children changes; no built-in tree cache. |
| [OpenAI CUA sample](https://github.com/openai/openai-cua-sample-app/tree/f2a3dc523ae406f9b704f9a420a05402a63b4522/python-app), MIT; [public Codex](https://github.com/openai/codex/tree/b172810921f89847cd310ecc496f9c901760e933), Apache-2.0 | Sample uses screenshots/PyAutoGUI with Retina normalization, not AX. Public Codex computer-use configuration does not reveal the closed desktop AX backend. | Foreground PyAutoGUI in sample. | Screenshot feedback to agent; no native background verifier found. | Persistent execution globals, not AX caching. Closed desktop implementation unknown. |
| [Anthropic computer-use demo](https://github.com/anthropics/claude-quickstarts/tree/3994db7dc2464d9ab255aba1dfda3594fc994c21/computer-use-demo) and best-practices example, MIT | Linux demo uses xdotool and screenshots; best-practices example uses foreground PyAutoGUI/Retina normalization. Neither implements macOS AX. | Global foreground mouse events. | Screenshot feedback (demo has a two-second settle); no native automatic click postcondition. | No AX tree cache. |

## Reuse and remaining gaps

Port the compatible upstream CUA enablement and exact-target verification
approach rather than extending ad hoc timers. Preserve this fork's native
branding, permission attribution and window scoping; a whole-branch rebase
would mix unrelated changes into this repair.

Use Peekaboo's batched descriptor/error handling pattern, AXorcist's CF identity
and process-generation observer lifecycle, and Hammerspoon's notification
registration pattern. Their MIT licenses permit reuse; preserve attribution
for copied code. Our existing `tools/codex_compat.rs` already contains AXObserver
FFI/run-loop ownership that can be reused locally.

The integration gaps are a per-window cache updated from notifications,
visible/focused-first traversal with offscreen and zero-size pruning, collapsed
unlabeled one-child wrappers, stable serialized identities, window-relative
frames, compact actionable/labeled output, and explicit diffs and role/label/
region queries. A deadline remains a final safety bound, not the performance
strategy. Focus changes must not produce false click-effect confirmations.
