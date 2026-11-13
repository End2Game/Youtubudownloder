fn main() {
    // Embed the Grab icon and Windows version metadata into the .exe.
    // The Linux build remains unaffected.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winres::WindowsResource::new();
        resource
            .set_icon("assets/grab.ico")
            .set("ProductName", "Grab")
            .set("FileDescription", "Grab Download Manager")
            .set("InternalName", "grab")
            .set("OriginalFilename", "grab.exe")
            .set("CompanyName", "Grab");

        resource.compile().expect("Failed to embed Grab Windows icon");
    }

    println!("cargo:rerun-if-changed=assets/grab.ico");
}
