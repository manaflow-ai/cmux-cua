//! Session colors replayed against the shared palette vectors that the Swift
//! CmuxAgentCursor package also replays, so the cmux app's cursor, lease badge
//! and activity rows use the same color per session as this renderer.

use cursor_overlay::Palette;
use serde_json::Value;

const VECTORS: &str = include_str!(
    "../../../../swift/CmuxAgentCursor/Tests/CmuxAgentCursorTests/Resources/palette_vectors.json"
);

fn rgba(value: &Value) -> [u8; 4] {
    let a = value.as_array().expect("rgba array");
    [0, 1, 2, 3].map(|i| a[i].as_u64().expect("channel") as u8)
}

#[test]
fn session_colors_match_the_shared_palette_vectors() {
    let doc: Value = serde_json::from_str(VECTORS).expect("vectors parse");
    for case in doc["cases"].as_array().expect("cases") {
        let id = case["id"].as_str().expect("id");
        let palette = Palette::for_instance(id);
        assert_eq!(palette.name, case["palette"].as_str().unwrap(), "{id:?}: palette");
        assert_eq!(palette.cursor_start, rgba(&case["cursor_start"]), "{id:?}");
        assert_eq!(palette.cursor_mid, rgba(&case["cursor_mid"]), "{id:?}");
        assert_eq!(palette.cursor_end, rgba(&case["cursor_end"]), "{id:?}");
        assert_eq!(palette.bloom_outer, rgba(&case["bloom_outer"]), "{id:?}");
        assert_eq!(palette.bloom_inner, rgba(&case["bloom_inner"]), "{id:?}");
        for (t, want) in case["gradient"].as_object().expect("gradient") {
            let t: f64 = t.parse().expect("t");
            assert_eq!(palette.gradient_at(t), rgba(want), "{id:?} gradient_at({t})");
        }
    }
}
