//! Gives the executable its icon, the description Windows shows for it, and
//! a manifest that says what it is: a program for any user, which never asks
//! to run as administrator by itself, made for Windows 10 and 11, and aware
//! of each monitor's scale. Programs that say nothing about themselves look
//! the more suspect to virus scanners.

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <assemblyIdentity type="win32" name="Bruma" version="{version}.0" processorArchitecture="*"/>
  <description>Bruma, a screenshot tool</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=assets/bruma.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/bruma.ico");
    resource.set("ProductName", "Bruma");
    resource.set("FileDescription", "Bruma screenshot tool");
    resource.set("CompanyName", "Bruma");
    resource.set("LegalCopyright", "MIT or Apache-2.0");
    resource.set("OriginalFilename", "bruma.exe");
    resource.set("InternalName", "bruma");
    resource.set("Comments", "https://github.com/0301091405/bruma");
    resource.set_manifest(&MANIFEST.replace("{version}", &version));
    if let Err(e) = resource.compile() {
        // Without a resource compiler the build still works; the executable
        // just has no icon.
        println!("cargo:warning=no icon: {e}");
    }
}
