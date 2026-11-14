fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("slint compile failed");

    // Embed the application icon (and version info) into the Windows executable.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=packaging/skytrace.ico");
        #[cfg(windows)]
        {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("packaging/skytrace.ico");
            res.set("ProductName", "skytrace");
            res.set("FileDescription", "skytrace live flight tracker");
            res.compile().expect("embedding Windows icon failed");
        }
    }
}
