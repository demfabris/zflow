fn main() {
    println!("cargo:rerun-if-changed=src/macos/capture_bridge.c");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    cc::Build::new()
        .file("src/macos/capture_bridge.c")
        .flag("-std=c11")
        .warnings(true)
        .compile("zflow_macos_capture");
    println!("cargo:rustc-link-lib=framework=ApplicationServices");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}
