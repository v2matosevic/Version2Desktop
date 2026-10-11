fn main() {
    println!("cargo:rerun-if-changed=../../shared/assets/sonora/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("../../shared/assets/sonora/icon.ico")
            .set("ProductName", "Sonora")
            .set("FileDescription", "Sonora offline music player")
            .set("CompanyName", "Version2")
            .set(
                "LegalCopyright",
                "Copyright 2026 Marko / Version2. All rights reserved.",
            );
        resource
            .compile()
            .expect("compile Sonora Windows icon and version resources");
    }
}
