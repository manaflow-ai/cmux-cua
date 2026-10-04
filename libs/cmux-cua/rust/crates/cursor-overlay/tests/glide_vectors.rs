//! The agent cursor's macOS motion (tick_swift_constants) replayed against
//! the shared glide vectors that the Swift CmuxAgentCursor package also
//! replays, so the CoreAnimation cursor moves exactly like this renderer.

use cursor_overlay::{CursorConfig, OverlayCommand, RenderStateCore};
use serde_json::Value;

// The vectors live in the Swift package so the package stays self-contained
// when the cmux app vendors it (plans/cmux-next/agent-cursor.md).
const VECTORS: &str = include_str!(
    "../../../../swift/CmuxAgentCursor/Tests/CmuxAgentCursorTests/Resources/glide_vectors.json"
);

#[test]
fn macos_motion_matches_the_shared_glide_vectors() {
    let doc: Value = serde_json::from_str(VECTORS).expect("vectors parse");
    let dt = doc["dt"].as_f64().expect("dt");
    let tolerance = doc["tolerance"].as_f64().expect("tolerance");
    for case in doc["cases"].as_array().expect("cases") {
        let from = &case["from"];
        let to = &case["to"];
        let (x0, y0) = (from[0].as_f64().unwrap(), from[1].as_f64().unwrap());
        let (x1, y1) = (to[0].as_f64().unwrap(), to[1].as_f64().unwrap());
        let end_heading = case["end_heading"].as_f64().unwrap();
        let expected = case["positions"].as_array().unwrap();
        let headings = case["headings"].as_array().unwrap();

        let cfg = CursorConfig::parse(&["--cursor-shape".to_owned(), "cmux".to_owned()]);
        let mut core = RenderStateCore::new(cfg);
        core.place_at(x0, y0);
        assert!(core.apply_command_base(
            OverlayCommand::MoveTo { x: x1, y: y1, end_heading_radians: end_heading },
            false,
            false,
        ));
        let mut arrived_tick = None;
        for (index, want) in expected.iter().enumerate() {
            if core.tick_swift_constants(dt) {
                arrived_tick = Some(index + 1);
            }
            let (wx, wy) = (want[0].as_f64().unwrap(), want[1].as_f64().unwrap());
            let label = format!("case {from} -> {to}, tick {}", index + 1);
            assert!((core.pos.0 - wx).abs() <= tolerance, "{label}: x {} vs {wx}", core.pos.0);
            assert!((core.pos.1 - wy).abs() <= tolerance, "{label}: y {} vs {wy}", core.pos.1);
            let wh = headings[index].as_f64().unwrap();
            assert!((core.heading - wh).abs() <= tolerance, "{label}: heading {} vs {wh}", core.heading);
        }
        assert_eq!(arrived_tick, case["arrived_tick"].as_u64().map(|t| t as usize), "case {from} -> {to}: arrival tick");
        assert!(core.path.is_none() && core.spring.is_none(), "case {from} -> {to}: settled at the last vector tick");
    }
}
