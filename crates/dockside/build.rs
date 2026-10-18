//! Embeds the DockSide icon (assets/dockside.ico) into the exe.

fn main() {
    println!("cargo:rerun-if-changed=dockside.rc");
    println!("cargo:rerun-if-changed=../../assets/dockside.ico");
    embed_resource::compile("dockside.rc", embed_resource::NONE).manifest_optional().unwrap();
}
