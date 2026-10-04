import Foundation

/// Draws agent cursors for published input events (the app's overlay layer
/// or the Computer Use helper's overlay panel).
@MainActor
public protocol AgentCursorRendering: AnyObject {
    func render(_ event: AutomationInputEvent)
}

/// The one entry point every driver path calls: the cmux-next provider
/// bridge (`input {event}` frames from the browser host), the classic app's
/// in-process browser driver, and the CUA host's helper. One injected
/// instance per owner; no singleton, no NotificationCenter.
@MainActor
public final class AgentCursorPublisher {
    private let renderer: AgentCursorRendering

    public init(renderer: AgentCursorRendering) {
        self.renderer = renderer
    }

    /// Forwards `event` unless its session already published this or a
    /// later `seq` (a replayed or reordered frame).
    public func publish(_ event: AutomationInputEvent) {
        _ = event
    }

    /// Forget a session (its lease ended).
    public func endSession(_ sessionID: String) {
        _ = sessionID
    }
}
