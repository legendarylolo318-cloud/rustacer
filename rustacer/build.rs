fn main() {
    println!("cargo:rerun-if-changed=rustacer.rc");
    println!("cargo:rerun-if-changed=rustacer.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("rustacer.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed the application manifest");
    }
}
