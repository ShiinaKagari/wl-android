fn main() {
    cc::Build::new()
        .file("c/bridge.c")
        .compile("bridge");
}
