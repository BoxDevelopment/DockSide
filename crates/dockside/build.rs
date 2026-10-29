//! Embeds the DockSide icon (assets/dockside.ico) into the exe, and builds
//! `dockside_hook.dll` so the exe can carry it (see genie/injector.rs).

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=dockside.rc");
    println!("cargo:rerun-if-env-changed=DOCKSIDE_BUILD");
    println!("cargo:rerun-if-changed=../../assets/dockside.ico");
    embed_resource::compile("dockside.rc", embed_resource::NONE).manifest_optional().unwrap();
    build_hook();
}

/// Runs a second cargo for the hook crate, with the same profile and target, in a
/// target dir of its own (the outer build holds the lock on ours), and copies the
/// DLL into OUT_DIR for `include_bytes!`.
fn build_hook() {
    println!("cargo:rerun-if-changed=../dockside-hook");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR is <target>[/<triple>]/<profile>/build/dockside-<hash>/out.
    let profile_dir = out.ancestors().nth(3).unwrap();
    let profile = match profile_dir.file_name().unwrap().to_str().unwrap() {
        "debug" => "dev",
        p => p,
    };
    let target = std::env::var("TARGET").unwrap();
    let hook_target = profile_dir.parent().unwrap().join("hook");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dockside-hook/Cargo.toml");
    let status = Command::new(std::env::var("CARGO").unwrap())
        .args(["build", "--profile", profile, "--target", &target, "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&hook_target)
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("running cargo for dockside-hook");
    assert!(status.success(), "building dockside-hook failed");
    let profile_out = if profile == "dev" { "debug" } else { profile };
    let dll = hook_target.join(&target).join(profile_out).join("dockside_hook.dll");
    std::fs::copy(&dll, out.join("dockside_hook.dll")).unwrap_or_else(|e| panic!("copying {}: {e}", dll.display()));
}
