fn main() {
    println!("cargo:rerun-if-changed=c/bridge.c");
    cc::Build::new()
        .file("c/bridge.c")
        .compile("bridge");
}
