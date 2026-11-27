//! Records the person's real pointer position (CGEventGetLocation) and the
//! frontmost app before and after a click/type/drag sequence, to show that
//! background delivery never moves the real pointer or takes focus.
//!
//! usage: cursor_proof <target-pid> <x> <y>
//! Run once with CMUX_CUA_ALLOW_REAL_INPUT unset (the shipped default) and once
//! with it set to 1 as the control (the old behaviour: the pointer moves).
#[cfg(target_os = "macos")]
fn main() {
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use platform_macos::{apps, input};

    let a: Vec<String> = std::env::args().collect();
    let pid: i32 = a.get(1).and_then(|s| s.parse().ok()).expect("target pid");
    let x: f64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(700.0);
    let y: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(500.0);

    let cursor = || {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        let p = CGEvent::new(src).unwrap().location();
        (p.x, p.y)
    };
    let mut moved = false;
    let mut step = |name: &str, f: &mut dyn FnMut() -> String| {
        let (bx, by) = cursor();
        let (bf, r) = (apps::frontmost_pid(), f());
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (ax, ay) = cursor();
        let af = apps::frontmost_pid();
        let same = (bx, by) == (ax, ay) && bf == af;
        moved |= !same;
        println!(
            "{name:34} cursor ({bx:.1},{by:.1}) -> ({ax:.1},{ay:.1})  front {bf:?} -> {af:?}  {}  [{r}]",
            if same { "UNCHANGED" } else { "CHANGED" }
        );
    };
    let ok = |r: anyhow::Result<()>| match r {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("err: {}", e.to_string().chars().take(48).collect::<String>()),
    };

    println!("CMUX_CUA_ALLOW_REAL_INPUT={:?}", std::env::var("CMUX_CUA_ALLOW_REAL_INPUT").ok());
    step("background click (pid-routed)", &mut || ok(input::mouse::click_at_xy(pid, x, y, 1, &[])));
    step("background type_text", &mut || ok(input::keyboard::type_text(pid, "x")));
    step("background press_key", &mut || ok(input::keyboard::press_key(pid, "shift", &[])));
    step("foreground-assist click", &mut || {
        match input::skylight::with_foreground_assist(pid, 0, || input::mouse::click_at_xy(pid, x, y, 1, &[])) {
            Ok(fronted) => format!("ok fronted={fronted}"),
            Err(e) => format!("err: {e}"),
        }
    });
    step("desktop-scope click (HID+warp)", &mut || ok(input::mouse::click_at_xy_desktop(x + 150.0, y + 100.0, 1, "left")));
    step("foreground drag (HID+warp)", &mut || {
        ok(input::mouse::drag_at_xy_foreground(x, y, x + 60.0, y + 40.0, 100, 4, &[], input::mouse::DragButton::Left))
    });
    println!("{}", if moved { "RESULT: the real pointer or focus CHANGED" } else { "RESULT: real pointer and focus UNCHANGED across every step" });
}
#[cfg(not(target_os = "macos"))]
fn main() {}
