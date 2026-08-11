fn main() {
    println!("cargo:rerun-if-changed=c/bridge.c");
    println!("cargo:rerun-if-changed=c/egl_bench.c");
    println!("cargo:rerun-if-changed=c/egl_blit.c");
    cc::Build::new()
        .file("c/bridge.c")
        .file("c/egl_bench.c")
        .file("c/egl_blit.c")
        .compile("bridge");
}
