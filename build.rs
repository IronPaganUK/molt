fn main() {
    // Embed the .ico plus proper version metadata and an application
    // manifest into the .exe. Beyond showing the Molt icon in Explorer,
    // a fully described, manifested binary looks like published software
    // rather than an anonymous stripped blob — which reduces the odds of
    // antivirus ML heuristics false-flagging it (molt's zero-fill + delete
    // behavior is otherwise wiper-like to a naive classifier).
    // Only runs when building on/for Windows; no-op elsewhere.
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            // A minimal, standard manifest: run as the invoking user (no
            // UAC elevation — avoids privilege-escalation heuristics) and
            // declare Windows 10/11 compatibility.
            let major = env!("CARGO_PKG_VERSION_MAJOR");
            let minor = env!("CARGO_PKG_VERSION_MINOR");
            let patch = env!("CARGO_PKG_VERSION_PATCH");
            let manifest = format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="IronPaganUK.Molt" version="{major}.{minor}.{patch}.0" processorArchitecture="*"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
      <supportedOS Id="{{1f676c76-80e1-4239-95bb-83d0f6d0da78}}"/>
    </application>
  </compatibility>
</assembly>"#
            );

            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/molt.ico")
                .set_manifest(&manifest)
                .set("ProductName", "Molt")
                .set(
                    "FileDescription",
                    "Molt — extract archives without doubling your disk usage",
                )
                .set("CompanyName", "IronPaganUK")
                .set("LegalCopyright", "MIT License. https://github.com/IronPaganUK/molt")
                .set("OriginalFilename", "molt-gui.exe");
            if let Err(e) = res.compile() {
                println!("cargo:warning=could not embed exe resources: {e}");
            }
        }
    }
}
