import Foundation

/// One agent input (`automation.input` v1, manaflow-ai/cmux
/// schemas/automation-input/event.schema.json). Drivers publish it after
/// their policy and frame checks, right before dispatch. Coordinates are
/// never screen points: `viewport` is unzoomed CSS px of the top-level
/// document viewport, `window` is window-local points of a desktop window.
public struct AutomationInputEvent: Sendable, Equatable {
    public enum Kind: String, Sendable, CaseIterable {
        case move, click, drag, type, key, scroll
        case doubleClick = "double_click"
        case rightClick = "right_click"
    }

    public enum Space: String, Sendable {
        case viewport, window
    }

    public struct Point: Sendable, Equatable {
        public var x: Double
        public var y: Double
        public init(x: Double, y: Double) {
            self.x = x
            self.y = y
        }
    }

    public struct Rect: Sendable, Equatable {
        public var x: Double
        public var y: Double
        public var w: Double
        public var h: Double
        public init(x: Double, y: Double, w: Double, h: Double) {
            self.x = x
            self.y = y
            self.w = w
            self.h = h
        }
    }

    public var sessionID: String
    public var targetID: String
    public var seq: UInt64
    public var kind: Kind
    public var space: Space
    public var point: Point?
    public var rect: Rect?
    public var to: Point?
    /// Page zoom at emit time (viewport space only); `nil` means 1.
    public var zoom: Double?
    public var tMs: Double

    public init(
        sessionID: String, targetID: String, seq: UInt64, kind: Kind, space: Space,
        point: Point? = nil, rect: Rect? = nil, to: Point? = nil, zoom: Double? = nil, tMs: Double
    ) {
        self.sessionID = sessionID
        self.targetID = targetID
        self.seq = seq
        self.kind = kind
        self.space = space
        self.point = point
        self.rect = rect
        self.to = to
        self.zoom = zoom
        self.tMs = tMs
    }
}

public enum AutomationInputEventError: Error, Equatable {
    case notAnObject
    case unknownField(String)
    case missingField(String)
    case invalidField(String)
    case unsupportedVersion
    case pointRequired
    case toRequired
    case wrongSpace
    case zoomNotAllowed
}

extension AutomationInputEvent {
    /// Decodes and validates one event; rejects anything the schema rejects.
    public static func decode(_ data: Data) throws -> AutomationInputEvent {
        _ = data
        throw AutomationInputEventError.notAnObject
    }
}
