// MinHook (BSD-2, minhook/), linked into the DLL. Same C runtime as Rust's own (the
// dynamic one), so the two never conflict.
fn main() {
    cc::Build::new()
        .files([
            "minhook/src/buffer.c",
            "minhook/src/hook.c",
            "minhook/src/trampoline.c",
            "minhook/src/hde/hde32.c",
            "minhook/src/hde/hde64.c",
        ])
        .include("minhook/include")
        .warnings(false)
        .compile("minhook");
    println!("cargo:rerun-if-changed=minhook");
}
