//! Gives the executable its icon and the name Windows shows for it.

fn main() {
    println!("cargo:rerun-if-changed=assets/bruma.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/bruma.ico");
    resource.set("ProductName", "Bruma");
    resource.set("FileDescription", "Bruma");
    if let Err(e) = resource.compile() {
        // Without a resource compiler the build still works; the executable
        // just has no icon.
        println!("cargo:warning=no icon: {e}");
    }
}
