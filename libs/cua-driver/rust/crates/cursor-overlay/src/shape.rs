//! Custom cursor shape — rasterised from SVG / ICO / PNG.
//!
//! Used when `--cursor-icon <path>` is passed to the MCP binary, plus the
//! `teardrop()` and `cmux()` built-ins selectable via `--cursor-shape <name>`.
//! Always produces an RGBA pixel buffer.

use anyhow::{bail, Result};

/// Built-in cursor silhouette selectable via `--cursor-shape <name>` or the
/// MCP `cursor_icon` field. Used when no `--cursor-icon` custom file overrides
/// the choice.
///
/// `Teardrop` is the default; opt into another built-in with
/// `--cursor-shape <name>` (CLI) or `cursor_icon: "<name>"` (MCP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinShape {
    /// Procedural gradient diamond drawn from vector primitives — the
    /// original cua cursor. Sharp at any backing scale because nothing is
    /// rasterised; `draw_default_arrow` rebuilds the path each frame.
    Arrow,
    /// Embedded `cursor-up` SVG (teardrop with notched bottom). Rasterised
    /// once into a 52 px RGBA buffer and blitted with a runtime transform.
    Teardrop,
    /// The cmux-branded Sky kite with the blue-to-violet gradient and white
    /// outline. Its tip is the event hotspot.
    Cmux,
}

impl BuiltinShape {
    /// Canonical `(name → variant)` table. The single source of truth behind
    /// [`parse`](Self::parse), [`names`](Self::names),
    /// [`names_help`](Self::names_help), the CLI `--cursor-shape` help text, and
    /// every platform's MCP `cursor_icon` tool description. Add a built-in here
    /// and all of them pick it up — nothing else hardcodes the name list.
    const TABLE: &'static [(&'static str, Self)] = &[
        ("arrow", Self::Arrow),
        ("teardrop", Self::Teardrop),
        ("cmux", Self::Cmux),
    ];

    /// Parse the value of `--cursor-shape` / MCP `cursor_icon`. Case-insensitive.
    /// Returns `None` for unknown names so the caller can warn and fall back to
    /// the default.
    pub fn parse(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        Self::TABLE.iter().find(|(n, _)| *n == lower).map(|(_, v)| *v)
    }

    /// The accepted built-in names in declaration order, e.g.
    /// `["arrow", "teardrop", "cmux"]`.
    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::TABLE.iter().map(|(name, _)| *name)
    }

    /// Human-facing list of built-in names for help / tool-description text,
    /// e.g. `'arrow' | 'teardrop' | 'cmux'`. The single string the CLI `--help`
    /// and every MCP `cursor_icon` description render from, so the advertised
    /// vocabulary can never drift from what [`parse`](Self::parse) accepts.
    pub fn names_help() -> String {
        Self::names()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(" | ")
    }

}

impl Default for BuiltinShape {
    fn default() -> Self {
        Self::Teardrop
    }
}

/// What an MCP `cursor_icon` value resolves to. Built-in names drive the
/// overlay's `builtin_shape` (so either built-in is reachable regardless of
/// which is the default); a file path becomes a one-off shape override.
#[derive(Debug, Clone)]
pub enum CursorIconResolution {
    /// A built-in silhouette — apply via `OverlayCommand::SetBuiltinShape`,
    /// which sets `builtin_shape` and clears any custom override.
    Builtin(BuiltinShape),
    /// A custom image loaded from disk — apply via
    /// `OverlayCommand::SetShape(Some(_))`.
    Image(CursorShape),
}

/// Resolve an MCP `cursor_icon` (or CLI) value into a [`CursorIconResolution`].
///
/// - empty string → the configured default built-in ([`BuiltinShape::default`])
/// - a built-in name (`arrow` / `teardrop` / `cmux`, case-insensitive) → that built-in
/// - anything else → treated as a file path and loaded (`.svg` / `.png` / `.ico`)
///
/// This is the single resolver shared by the CLI flags and every platform's MCP
/// `set_agent_cursor_motion` handler, so the accepted vocabulary — and which
/// rendering path each value takes — stays identical across all of them.
pub fn resolve_cursor_icon(value: &str) -> Result<CursorIconResolution> {
    if value.is_empty() {
        return Ok(CursorIconResolution::Builtin(BuiltinShape::default()));
    }
    if let Some(builtin) = BuiltinShape::parse(value) {
        return Ok(CursorIconResolution::Builtin(builtin));
    }
    CursorShape::load(value).map(CursorIconResolution::Image)
}

/// Rasterised cursor shape at 64×64 RGBA.
#[derive(Debug, Clone)]
pub struct CursorShape {
    /// Raw RGBA pixels, row-major top-to-bottom, 4 bytes per pixel.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Source rasterisation size. Sized as 2× the runtime display target
/// (26 px in `paint_cursor`) so the downscale ratio stays a clean 2:1 —
/// bilinear handles that without smearing, and on 2× retina displays the
/// pixmap maps 1:1 to physical pixels for perfect crispness. Non-integer
/// ratios (e.g. 64→26 = 0.41×) produce visible downscale blur.
pub const CURSOR_SIZE: u32 = 52;

/// The cmux Sky asset uses a larger source buffer so its small kite silhouette
/// remains crisp when downscaled to the runtime cursor size.
pub const CMUX_CURSOR_SIZE: u32 = 64;

/// User-supplied "cursor-up" silhouette from svgrepo.com — an upward-pointing
/// teardrop with a notched bottom. Original fill was solid black; we substitute
/// the fleet linear gradient (light blue → teal, top-right to bottom-left) so
/// the runtime `gradient_colors` MCP override has a surface to tint, and add a
/// white stroke for visibility on dark backdrops. Embedded verbatim so the
/// daemon ships the built-in teardrop without an external asset.
const TEARDROP_CURSOR_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
<defs>
<linearGradient id="cursorGrad" x1="1" x2="0" y1="0" y2="1">
<stop offset="0" stop-color="#F0FBFF"/>
<stop offset="0.55" stop-color="#66D9FF"/>
<stop offset="1" stop-color="#35C6D8"/>
</linearGradient>
</defs>
<path d="M19.87,19.21l-6-15.92a2,2,0,0,0-3.74,0l-6,15.92a2,2,0,0,0,.65,2.3A2.21,2.21,0,0,0,6.17,22a2.24,2.24,0,0,0,1.23-.37L12,18.57l4.6,3.06a2.22,2.22,0,0,0,2.62-.12A2,2,0,0,0,19.87,19.21Z" fill="url(#cursorGrad)" stroke="#FFFFFF" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round"/>
</svg>"##;

/// The historical cmux Computer Use cursor: the Sky kite silhouette filled
/// with the cmux brand gradient and outlined in white. The duplicate paths
/// make the stroke-under-fill order explicit for SVG renderers.
const CMUX_CURSOR_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="37.17" height="37.17" viewBox="0 0 18.59 18.59">
<defs>
<linearGradient id="cmuxGradient" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="#12c7f5"/>
<stop offset="0.5" stop-color="#2d8cff"/>
<stop offset="1" stop-color="#6c5cff"/>
</linearGradient>
</defs>
<g transform="translate(3,3)">
<path d="M0.68 1.83 L3.63 9.78 Q4.67 12.59 5.3 9.66 L5.44 9.01 Q6.08 6.08 9.01 5.44 L9.66 5.3 Q12.59 4.67 9.78 3.63 L1.83 0.68 Q0 0 0.68 1.83 Z" fill="none" stroke="#FFFFFF" stroke-width="1.7" stroke-linejoin="round"/>
<path d="M0.68 1.83 L3.63 9.78 Q4.67 12.59 5.3 9.66 L5.44 9.01 Q6.08 6.08 9.01 5.44 L9.66 5.3 Q12.59 4.67 9.78 3.63 L1.83 0.68 Q0 0 0.68 1.83 Z" fill="url(#cmuxGradient)"/>
</g>
</svg>"##;

impl CursorShape {
    /// Load from `path`.  Supported: `.svg`, `.ico`, `.png`.
    pub fn load(path: &str) -> Result<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".svg") {
            Self::load_svg(path)
        } else if lower.ends_with(".ico") || lower.ends_with(".png") {
            Self::load_raster(path)
        } else {
            bail!("Unsupported cursor-icon format (expected .svg, .ico, or .png): {path}")
        }
    }

    /// The built-in teardrop cursor — embedded `cursor-up` silhouette
    /// rasterised once via `OnceLock` so callers can ask for it cheaply.
    /// Selected by `BuiltinShape::Teardrop`, which is the default silhouette.
    pub fn teardrop() -> &'static Self {
        static CACHE: std::sync::OnceLock<CursorShape> = std::sync::OnceLock::new();
        CACHE.get_or_init(|| {
            Self::load_svg_bytes(TEARDROP_CURSOR_SVG)
                .expect("embedded teardrop SVG should parse")
        })
    }

    /// The built-in cmux Sky kite cursor.
    pub fn cmux() -> &'static Self {
        static CACHE: std::sync::OnceLock<CursorShape> = std::sync::OnceLock::new();
        CACHE.get_or_init(|| {
            Self::load_svg_bytes_at(CMUX_CURSOR_SVG, CMUX_CURSOR_SIZE)
                .expect("embedded cmux cursor SVG should parse")
        })
    }

    fn load_svg(path: &str) -> Result<Self> {
        let data = std::fs::read(path)?;
        Self::load_svg_bytes(&data)
    }

    fn load_svg_bytes(data: &[u8]) -> Result<Self> {
        Self::load_svg_bytes_at(data, CURSOR_SIZE)
    }

    fn load_svg_bytes_at(data: &[u8], size: u32) -> Result<Self> {
        let opts = usvg::Options::default();
        let tree = usvg::Tree::from_data(data, &opts)?;

        let mut pixmap = tiny_skia::Pixmap::new(size, size)
            .ok_or_else(|| anyhow::anyhow!("Failed to create {size}×{size} pixmap"))?;

        let sx = size as f32 / tree.size().width();
        let sy = size as f32 / tree.size().height();
        resvg::render(&tree, tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());

        let raw = pixmap.take();
        let pixels = unpremultiply(raw);
        Ok(Self { pixels, width: size, height: size })
    }

    fn load_raster(path: &str) -> Result<Self> {
        let img = image::open(path)?.into_rgba8();
        let resized = image::imageops::resize(
            &img,
            CURSOR_SIZE,
            CURSOR_SIZE,
            image::imageops::FilterType::Lanczos3,
        );
        Ok(Self {
            pixels: resized.into_raw(),
            width: CURSOR_SIZE,
            height: CURSOR_SIZE,
        })
    }
}

/// Convert pre-multiplied RGBA → straight RGBA.
fn unpremultiply(mut data: Vec<u8>) -> Vec<u8> {
    for px in data.chunks_exact_mut(4) {
        let a = px[3];
        if a > 0 && a < 255 {
            let scale = 255.0 / a as f32;
            px[0] = (px[0] as f32 * scale).min(255.0) as u8;
            px[1] = (px[1] as f32 * scale).min(255.0) as u8;
            px[2] = (px[2] as f32 * scale).min(255.0) as u8;
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_shape_parses_known_names_case_insensitive() {
        assert_eq!(BuiltinShape::parse("arrow"), Some(BuiltinShape::Arrow));
        assert_eq!(BuiltinShape::parse("ARROW"), Some(BuiltinShape::Arrow));
        assert_eq!(BuiltinShape::parse("Arrow"), Some(BuiltinShape::Arrow));
        assert_eq!(BuiltinShape::parse("teardrop"), Some(BuiltinShape::Teardrop));
        assert_eq!(BuiltinShape::parse("TEARDROP"), Some(BuiltinShape::Teardrop));
        assert_eq!(BuiltinShape::parse("Teardrop"), Some(BuiltinShape::Teardrop));
        assert_eq!(BuiltinShape::parse("cmux"), Some(BuiltinShape::Cmux));
        assert_eq!(BuiltinShape::parse("CMUX"), Some(BuiltinShape::Cmux));
        assert_eq!(BuiltinShape::parse("Cmux"), Some(BuiltinShape::Cmux));
    }

    #[test]
    fn builtin_shape_rejects_unknown_names() {
        assert_eq!(BuiltinShape::parse(""), None);
        assert_eq!(BuiltinShape::parse("diamond"), None);
        assert_eq!(BuiltinShape::parse("arrow "), None); // whitespace-significant
        assert_eq!(BuiltinShape::parse("cua_brand"), None); // legacy name no longer recognised
        // The invented MCP names that never existed in the renderer must NOT parse —
        // they were doc-only fiction and are gone now.
        assert_eq!(BuiltinShape::parse("crosshair"), None);
        assert_eq!(BuiltinShape::parse("hand"), None);
        assert_eq!(BuiltinShape::parse("dot"), None);
    }

    #[test]
    fn names_help_lists_every_table_entry() {
        // Single source of truth: help text is derived from TABLE, so it always
        // matches what `parse` accepts.
        let help = BuiltinShape::names_help();
        for name in BuiltinShape::names() {
            assert!(help.contains(name), "names_help() missing {name}: {help}");
            assert!(BuiltinShape::parse(name).is_some(), "{name} listed but unparseable");
        }
        assert_eq!(help, "'arrow' | 'teardrop' | 'cmux'");
    }

    #[test]
    fn resolve_cursor_icon_matches_builtins_and_revert() {
        use CursorIconResolution::*;
        // Empty reverts to the configured default built-in (now teardrop).
        assert!(matches!(resolve_cursor_icon("").unwrap(), Builtin(BuiltinShape::Teardrop)));
        // Built-in names resolve to that built-in, case-insensitively —
        // crucially `arrow` stays reachable even though teardrop is the default.
        assert!(matches!(resolve_cursor_icon("arrow").unwrap(), Builtin(BuiltinShape::Arrow)));
        assert!(matches!(resolve_cursor_icon("TEARDROP").unwrap(), Builtin(BuiltinShape::Teardrop)));
        assert!(matches!(resolve_cursor_icon("cmux").unwrap(), Builtin(BuiltinShape::Cmux)));
        // A non-name, non-existent path is treated as a file and fails to load.
        assert!(resolve_cursor_icon("/no/such/cursor.png").is_err());
    }

    #[test]
    fn cmux_raster_contains_brand_gradient_and_white_outline() {
        let cmux = CursorShape::cmux();
        assert_eq!((cmux.width, cmux.height), (CMUX_CURSOR_SIZE, CMUX_CURSOR_SIZE));
        assert_eq!(cmux.pixels.len(), (CMUX_CURSOR_SIZE * CMUX_CURSOR_SIZE * 4) as usize);

        let has_cyan = cmux.pixels.chunks_exact(4).any(|pixel| {
            pixel[3] > 0 && pixel[0] < 80 && pixel[1] > 140 && pixel[2] > 200
        });
        let has_violet = cmux.pixels.chunks_exact(4).any(|pixel| {
            pixel[3] > 0 && pixel[0] > 70 && pixel[1] < 140 && pixel[2] > 200
        });
        let has_white = cmux.pixels.chunks_exact(4).any(|pixel| {
            pixel[3] > 0 && pixel[0] > 235 && pixel[1] > 235 && pixel[2] > 235
        });

        assert!(has_cyan, "cmux cursor should contain the cyan gradient stop");
        assert!(has_violet, "cmux cursor should contain the violet gradient stop");
        assert!(has_white, "cmux cursor should contain the white outline");
    }

    /// `Teardrop` is the default silhouette. If this assertion changes, the
    /// `personalize-cursor.mdx` doc and the CLI `--cursor-shape` help must
    /// change to match — both tell users which built-in they get when no flag
    /// is set.
    #[test]
    fn default_builtin_shape_is_teardrop() {
        assert_eq!(BuiltinShape::default(), BuiltinShape::Teardrop);
    }
}
