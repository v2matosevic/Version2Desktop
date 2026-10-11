fn main() {
    println!("cargo:rerun-if-changed=../../shared/assets/vesper/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set("ProductName", "Vesper")
            .set("FileDescription", "Vesper offline video player")
            .set("CompanyName", "Version2")
            .set("LegalCopyright", "Copyright (c) 2026 Version2");
        let icon = "../../shared/assets/vesper/icon.ico";
        if std::fs::metadata(icon)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
        {
            resource.set_icon(icon);
        }
        resource
            .compile()
            .expect("Windows version resources must compile");
    }
}
