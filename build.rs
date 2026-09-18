fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        // Tolerated as missing so a fresh clone builds before the art does.
        if std::path::Path::new("assets/icon.ico").exists() {
            res.set_icon("assets/icon.ico");
        }
        if std::path::Path::new("assets/app.manifest").exists() {
            res.set_manifest_file("assets/app.manifest");
        }
        res.set("FileDescription", "Cota");
        res.set("ProductName", "Cota");
        res.set("LegalCopyright", "MIT licensed");
        if let Err(e) = res.compile() {
            eprintln!("cargo:warning=resource compile failed: {e}");
        }
    }
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
