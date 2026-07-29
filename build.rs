fn main() {
    println!("cargo:rerun-if-changed=assets/networkcopy-icon.ico");

    #[cfg(windows)]
    {
        winresource::WindowsResource::new()
            .set_icon("assets/networkcopy-icon.ico")
            .compile()
            .expect("failed to compile NetworkCopy Windows resources");
    }
}
