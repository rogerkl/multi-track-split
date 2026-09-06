fn main() {
    // Only when compiling for Windows: embed the icon and version info.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        // Version, FileDescription etc. come from [package] in Cargo.toml;
        // ProductName/LegalCopyright from [package.metadata.winresource].
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon/multi-track-split.ico");
        if let Err(e) = res.compile() {
            eprintln!("Failed to compile Windows resources: {e}");
            std::process::exit(1);
        }
    }
}
