use async_trait::async_trait;
use cua_driver_core::{protocol::ToolResult, tool::{Tool, ToolDef}};
use serde_json::Value;

pub struct GetScreenSizeTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        // Matches `GetScreenSizeTool.swift` description verbatim.
        name: "get_screen_size".into(),
        description: "Return the logical size of the main display in points plus its backing \
            scale factor. Agents click in points; Retina displays have scale_factor 2.0. \
            Requires no TCC permissions.".into(),
        input_schema: serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for GetScreenSizeTool {
    fn def(&self) -> &ToolDef { def() }

    async fn invoke(&self, _args: Value) -> ToolResult {
        match main_screen_size() {
            Some((w, h, scale)) => {
                // Matches Swift text format 1:1.
                ToolResult::text(format!("✅ Main display: {w}x{h} points @ {scale}x"))
                    .with_structured(serde_json::json!({
                        "width": w, "height": h, "scale_factor": scale,
                    }))
            }
            None => ToolResult::error("No main display detected."),
        }
    }
}

/// Returns `(width_points, height_points, backing_scale_factor)` from
/// CoreGraphics — safe to call from any thread (no AppKit main-thread requirement).
///
/// The previous NSScreen-based implementation required `MainThreadMarker::new()`
/// which always returns `None` on async tokio threads, causing the tool to
/// return an error even when a display is attached.
pub(crate) fn main_screen_size() -> Option<(i64, i64, f64)> {
    use core_graphics::display::{CGMainDisplayID, CGDisplayBounds};

    // SAFETY: CGMainDisplayID / CGDisplayBounds are thread-safe CG APIs.
    let display_id = unsafe { CGMainDisplayID() };
    if display_id == 0 {
        return None;
    }
    let bounds = unsafe { CGDisplayBounds(display_id) };
    let w = bounds.size.width as i64;
    let h = bounds.size.height as i64;
    if w == 0 || h == 0 {
        return None;
    }

    let scale = get_backing_scale(display_id, w);
    Some((w, h, scale))
}

/// Resolve the display's backing scale from its current mode.
///
/// `CGDisplayPixelsWide` is not a reliable primary source on HiDPI Macs: it
/// can report the mode's logical point width even while the backing surface is
/// rendered at twice that many physical pixels. The current display mode keeps
/// both values, so use its physical-pixel/point ratio and retain the old API as
/// a fallback for older or partially configured displays.
pub(crate) fn get_backing_scale(display_id: u32, logical_w: i64) -> f64 {
    get_backing_scale_optional(display_id, logical_w).unwrap_or(1.0)
}

/// Same resolver as [`get_backing_scale`], retaining `None` when neither the
/// display mode nor the legacy framebuffer query can provide usable widths.
/// Callers that already have an independent AppKit scale can use that value as
/// the final fallback instead of treating an unavailable mode as a real 1×
/// display.
pub(crate) fn get_backing_scale_optional(display_id: u32, logical_w: i64) -> Option<f64> {
    use core_graphics::display::CGDisplay;
    if let Some(mode) = CGDisplay::new(display_id).display_mode() {
        if let Some(scale) = scale_from_widths(mode.pixel_width() as f64, mode.width() as f64) {
            return Some(scale);
        }
    }

    // Legacy framebuffer-width heuristic; correct on non-HiDPI modes and a
    // useful fallback when the mode copy is unavailable.
    use core_graphics::display::CGDisplayPixelsWide;
    let pixel_w = unsafe { CGDisplayPixelsWide(display_id) } as f64;
    scale_from_widths(pixel_w, logical_w as f64)
}

/// Pixels-per-point ratio rounded to the nearest 0.5 to absorb display-mode
/// floating-point noise. Returns `None` for unusable dimensions.
fn scale_from_widths(pixel_w: f64, point_w: f64) -> Option<f64> {
    if pixel_w > 0.0 && point_w > 0.0 {
        Some((((pixel_w / point_w) * 2.0).round() / 2.0).max(1.0))
    } else {
        None
    }
}

#[cfg(test)]
mod backing_scale_tests {
    use super::{get_backing_scale, scale_from_widths, GetScreenSizeTool};
    use cua_driver_core::tool::Tool;

    #[test]
    fn display_mode_dimensions_preserve_retina_ratio() {
        // HiDPI mode: 3024 physical pixels over 1512 logical points. The
        // legacy CGDisplayPixelsWide path sees 1512/1512 and incorrectly
        // collapses this to 1×.
        assert_eq!(scale_from_widths(3024.0, 1512.0), Some(2.0));
        assert_eq!(scale_from_widths(1512.0, 1512.0), Some(1.0));
        // Scaled modes can legitimately use a half-step ratio.
        assert_eq!(scale_from_widths(3600.0, 2400.0), Some(1.5));
        assert_eq!(
            scale_from_widths(600.0, 1200.0),
            Some(1.0),
            "a malformed sub-1x ratio must not be exposed as a backing scale"
        );
        assert_eq!(scale_from_widths(0.0, 1512.0), None);
        assert_eq!(scale_from_widths(3024.0, 0.0), None);
    }

    #[test]
    fn live_display_mode_is_the_backing_scale_oracle_when_available() {
        use core_graphics::display::{CGDisplay, CGDisplayBounds, CGMainDisplayID};

        let display_id = unsafe { CGMainDisplayID() };
        if display_id == 0 {
            return;
        }
        let bounds = unsafe { CGDisplayBounds(display_id) };
        let Some(mode) = CGDisplay::new(display_id).display_mode() else {
            // A headless WindowServer can expose an id without a mode.
            return;
        };
        let Some(expected) = scale_from_widths(mode.pixel_width() as f64, mode.width() as f64)
        else {
            return;
        };

        let actual = get_backing_scale(display_id, bounds.size.width.round() as i64);
        assert_eq!(
            actual, expected,
            "the screen-size protocol and cursor rasterizer must use the mode's physical/logical ratio"
        );
    }

    #[tokio::test]
    async fn screen_size_protocol_reports_the_live_mode_scale() {
        use core_graphics::display::{CGDisplay, CGDisplayBounds, CGMainDisplayID};

        let display_id = unsafe { CGMainDisplayID() };
        if display_id == 0 {
            return;
        }
        let bounds = unsafe { CGDisplayBounds(display_id) };
        let Some(mode) = CGDisplay::new(display_id).display_mode() else {
            return;
        };
        let expected = scale_from_widths(mode.pixel_width() as f64, mode.width() as f64)
            .expect("valid display-mode dimensions");

        let response = GetScreenSizeTool.invoke(serde_json::json!({})).await;
        assert_ne!(
            response.is_error,
            Some(true),
            "get_screen_size must succeed when a display mode is available"
        );
        let Some(structured) = response.structured_content else {
            panic!("get_screen_size returned no structured payload");
        };
        let reported = structured["scale_factor"]
            .as_f64()
            .expect("get_screen_size scale_factor");
        assert_eq!(
            reported,
            expected,
            "get_screen_size must expose the same mode scale used by cursor rasterization (bounds={bounds:?})"
        );
    }
}
