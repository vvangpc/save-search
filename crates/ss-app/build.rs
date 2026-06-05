// 把 app.rc（其中引用 app.manifest）编译并链接进 exe。
// manifest 声明 requireAdministrator + PerMonitorV2 DPI + Common-Controls v6。
fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.rc");
    embed_resource::compile("app.rc", embed_resource::NONE);
}
