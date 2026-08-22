fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/embrasure.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_manifest_file("packaging/windows/embrasure.exe.manifest");
        resource
            .compile()
            .expect("could not compile Windows version resources");
    }
}
