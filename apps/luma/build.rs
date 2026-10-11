fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/luma.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.set_manifest_file("assets/luma.manifest");
        resource.set("FileDescription", "Luma offline photo viewer");
        resource.set("ProductName", "Luma");
        resource.set("CompanyName", "Version2");
        resource.set(
            "LegalCopyright",
            "Copyright (c) 2026 Marko / Version2. All rights reserved.",
        );
        resource
            .compile()
            .expect("compile executable icon and Windows manifest");
    }
}
