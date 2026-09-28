use std::path::PathBuf;

fn main() {
    // The Touch Bar guard needs an @try/@catch whose body calls the original
    // implementation directly, with no Rust frame inside — see the comment in
    // objc/gpui_macos_try_remove.m for why objc2::exception::catch is not enough
    // under panic = "abort".
    println!("cargo:rerun-if-changed=objc/gpui_macos_try_remove.m");

    // docs.rs has no compiler that can build Objective-C.
    if std::env::var_os("DOCS_RS").is_some() {
        return;
    }

    let mut builder = cc::Build::new();
    builder.flag("-xobjective-c");
    builder.flag("-fobjc-exceptions");
    // Match objc2-exception-helper: no ARC, memory management stays explicit.
    builder.flag("-fno-objc-arc");
    builder.file("objc/gpui_macos_try_remove.m");
    builder.compile("gpui_macos_objc_shim");

    // The shim needs the Objective-C runtime's types in Rust.
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    println!(
        "cargo:include={}",
        manifest_dir.join("objc").to_str().unwrap()
    );
}
