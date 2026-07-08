fn main() {
    // Embed the .ico into the .exe so Explorer shows the Molt icon.
    // Only runs when building on/for Windows; no-op elsewhere.
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/molt.ico");
            if let Err(e) = res.compile() {
                println!("cargo:warning=could not embed exe icon: {e}");
            }
        }
    }
}
