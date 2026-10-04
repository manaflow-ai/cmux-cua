import QuartzCore

/// Turns a [`GlidePlan`] into CoreAnimation animations for the cursor layer.
public enum CursorAnimation {
    /// `position` keyframes, one per sample, linear between samples.
    public static func position(_ plan: GlidePlan) -> CAKeyframeAnimation {
        _ = plan
        return CAKeyframeAnimation(keyPath: "position")
    }

    /// `transform.rotation.z` keyframes that follow the arrow heading.
    public static func rotation(_ plan: GlidePlan) -> CAKeyframeAnimation {
        _ = plan
        return CAKeyframeAnimation(keyPath: "transform.rotation.z")
    }
}
