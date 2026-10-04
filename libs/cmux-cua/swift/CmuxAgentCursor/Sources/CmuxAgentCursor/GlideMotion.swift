import Foundation

/// The cmux-cua macOS cursor motion (cursor-overlay
/// `RenderStateCore::tick_swift_constants`): a straight travel with a
/// smootherstep speed envelope, then a spring settle. Planned once per action
/// and sampled at `dt`; the samples become one keyframe animation, so the
/// render server interpolates and the app does no per-frame work.
public struct GlideMotion: Sendable, Equatable {
    public var peakSpeed: Double = 900
    public var minStartSpeed: Double = 300
    public var minEndSpeed: Double = 200
    public var springStiffness: Double = 400
    public var springDamping: Double = 17
    public var springOvershoot: Double = 0.8
    public var dt: Double = 1.0 / 120.0

    public init() {}
}

public struct GlideSample: Sendable, Equatable {
    public var x: Double
    public var y: Double
    /// Arrow heading in radians (tip direction).
    public var heading: Double
}

public struct GlidePlan: Sendable, Equatable {
    /// One sample per `dt` tick, starting after the first tick.
    public var samples: [GlideSample]
    /// 1-based tick at which the travel ended and the spring began.
    public var arrivedTick: Int
    public var dt: Double

    public var duration: Double { Double(samples.count) * dt }
}

extension GlideMotion {
    public func plan(fromX x0: Double, fromY y0: Double, toX x1: Double, toY y1: Double, endHeading: Double) -> GlidePlan {
        _ = (x0, y0, x1, y1, endHeading)
        return GlidePlan(samples: [], arrivedTick: 0, dt: dt)
    }
}
