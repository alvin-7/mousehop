use shadow_rs::ShadowBuilder;

fn main() {
    ShadowBuilder::builder()
        .deny_const(Default::default())
        .build()
        .expect("shadow build");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/mousehop.ico");
        winresource::WindowsResource::new()
            .set_icon("assets/mousehop.ico")
            .set("ProductName", "Mousehop")
            .set("FileDescription", "Mousehop")
            .set("OriginalFilename", "mousehop.exe")
            .compile()
            .expect("compile Windows application resources");
    }
}
