//! Windows resource embedding: the .ico goes into the PE so the single exe
//! carries its own icon (Explorer, taskbar, shortcut IconLocation).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/nexuslink.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embed nexuslink.ico into the exe");
    }
}
