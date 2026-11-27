fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("band.ico");
        resource.set("ProductName", "Anyinstaller Backend");
        resource.set("FileDescription", "Anyinstaller Backend");
        resource
            .compile()
            .expect("failed to embed Windows resources");
    }
}
