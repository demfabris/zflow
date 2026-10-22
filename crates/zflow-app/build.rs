fn main() {
    println!("cargo:rerun-if-changed=zflow.rc");
    println!("cargo:rerun-if-changed=../../assets/windows/zflow.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("zflow.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("the icon resource compiles");
    }
}
