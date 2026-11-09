fn main() {
    // This script always runs on the build machine, so check what it's building for.
    #[cfg(target_os = "macos")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        link_swift_compatibility_libraries();
    }
    tauri_build::build()
}

/// The capture crate's Swift bridge needs these libraries. It only searches the
/// full Xcode layout, so machines with just the Command Line Tools fail to link.
#[cfg(target_os = "macos")]
fn link_swift_compatibility_libraries() {
    use std::path::Path;
    use std::process::Command;

    let Ok(output) = Command::new("xcode-select").arg("-p").output() else { return };
    let developer = String::from_utf8_lossy(&output.stdout).trim().to_string();
    for relative in [
        "usr/lib/swift/macosx",
        "Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx",
    ] {
        let path = Path::new(&developer).join(relative);
        if path.exists() {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    }
}
