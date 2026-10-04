import Foundation

/// The color of one agent session's cursor: the ten named palettes of
/// cursor-overlay (`Palette::for_instance`, a port of AgentCursorPalette.cs).
/// The cursor, the lease badge and the activity rows all take a session's
/// color from here, so they always match the cmux-cua renderer.
public struct AgentCursorPalette: Sendable, Equatable {
    /// RGBA, 0...255.
    public typealias RGBA = [UInt8]

    public var name: String
    public var cursorStart: RGBA
    public var cursorMid: RGBA
    public var cursorEnd: RGBA
    public var bloomOuter: RGBA
    public var bloomInner: RGBA

    /// The palette for a session id (the agent cursor key).
    public static func forSession(_ sessionID: String) -> AgentCursorPalette {
        _ = sessionID
        return AgentCursorPalette(name: "", cursorStart: [], cursorMid: [], cursorEnd: [], bloomOuter: [], bloomInner: [])
    }

    /// Lerp along start -> mid -> end at `t` in 0...1 (mid at 0.53).
    public func gradient(at t: Double) -> RGBA {
        _ = t
        return []
    }
}
