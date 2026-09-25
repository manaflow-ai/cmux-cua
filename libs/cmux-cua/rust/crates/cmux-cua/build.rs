// Bake Swift runtime rpaths into the cmux-cua binary on macOS.
//
// The `screencapturekit` dep ships a small Swift-bridge shim that links
// against the Swift Concurrency runtime (`@rpath/libswift_Concurrency.dylib`
// and friends). Its own build.rs emits `cargo:rustc-link-arg=-Wl,-rpath,…`
// directives, but those only flow through to the binary linker when the
// emitting crate is the final binary crate — for transitive deps Cargo
// silently drops them. So we re-emit the same rpaths from here.
//
// On Windows, embed the Per-Monitor V2 DPI-awareness manifest so the
// process sees physical pixels (no DWM coordinate virtualization) at
// 125%/150%/200% scaling and clicks land where screenshots say they do.

fn main() {
    #[cfg(target_os = "windows")]
    {
        embed_resource::compile("cmux-cua.rc", embed_resource::NONE);
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    emit_sdk_framework_search_path();
    emit_swift_runtime_link_args();
}

fn emit_sdk_framework_search_path() {
    let sdk_root = std::env::var("SDKROOT").unwrap_or_else(|_| {
        std::process::Command::new("xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|out| out.trim().to_owned())
            .unwrap_or_default()
    });

    if !sdk_root.is_empty() {
        println!("cargo:rustc-link-search=framework={sdk_root}/System/Library/Frameworks");
    }
}

fn emit_swift_runtime_link_args() {
    // The Swift runtime shipped by macOS is the only runtime search root that
    // belongs in a redistributable binary. xcode-select/xcrun also expose the
    // build machine's toolchain directories, but embedding those absolute
    // paths makes a quarantined bundle fail Gatekeeper on machines that do not
    // have that exact Xcode installation (and is rejected by syspolicy_check).
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
