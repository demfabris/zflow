fn main() {
    println!("cargo:rerun-if-changed=src/windows/input.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new()
            .file("src/windows/input.c")
            .warnings(true)
            .compile("zflow_windows_input");
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=wtsapi32");
        println!("cargo:rustc-link-lib=advapi32");
    }
    println!("cargo:rerun-if-changed=src/macos/capture_bridge.c");
    println!("cargo:rerun-if-changed=src/macos/awdl_client.c");
    println!("cargo:rerun-if-changed=src/macos/inject.c");
    println!("cargo:rerun-if-changed=src/macos/media.m");
    println!("cargo:rerun-if-changed=src/macos/pasteboard.m");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    cc::Build::new()
        .file("src/macos/capture_bridge.c")
        .file("src/macos/awdl_client.c")
        .file("src/macos/inject.c")
        .flag("-std=c11")
        .warnings(true)
        .compile("zflow_macos_capture");
    // Objective-C rejects -std=c11, so it builds on its own.
    cc::Build::new()
        .file("src/macos/media.m")
        .file("src/macos/pasteboard.m")
        .flag("-fobjc-arc")
        .warnings(true)
        .compile("zflow_macos_media");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=ApplicationServices");
    println!("cargo:rustc-link-lib=framework=Carbon");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=ImageIO");
    println!("cargo:rustc-link-lib=framework=IOKit");
    println!("cargo:rustc-link-lib=objc");
}
