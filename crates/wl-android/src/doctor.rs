/// Doctor subcommand — diagnostic self-check
pub fn run() {
    println!("wl-android doctor");
    println!("==================");
    println!();

    // Check environment
    let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "land-0".into());
    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let land_socket = std::env::var("LAND_SOCKET")
        .unwrap_or_else(|_| {
            format!("{xdg_runtime}/wl-android/land.sock")
        });

    println!("Environment:");
    println!("  WAYLAND_DISPLAY  = {wayland_display}");
    println!("  XDG_RUNTIME_DIR  = {xdg_runtime}");
    println!("  LAND_SOCKET      = {land_socket}");
    println!();

    // Check Wayland socket path
    let wayland_socket = std::path::PathBuf::from(&xdg_runtime).join(&wayland_display);
    if wayland_socket.exists() {
        println!("✅ Wayland socket exists: {wayland_socket:?}");
    } else {
        println!("ℹ️  Wayland socket not yet created (expected — wl-android not running)");
    }

    // Check land socket directory
    let land_dir = std::path::Path::new(&land_socket).parent();
    if let Some(dir) = land_dir {
        if dir.exists() {
            println!("✅ Land socket directory exists: {dir:?}");
        } else {
            println!("❌ Land socket directory missing: {dir:?}");
            println!("   Ensure LAND_SOCKET points to a writable path");
        }
    }

    // Check GPU devices
    for dev in &["/dev/kgsl-3d0", "/dev/dri/renderD128"] {
        if std::path::Path::new(dev).exists() {
            println!("✅ GPU device: {dev}");
        }
    }

    // Protocol version
    println!();
    println!("Protocol: v{}", wl_android_common::proto::PROTOCOL_VERSION);

    // Frame path: KWin renders on GPU (EGL/dmabuf) and the server forwards
    // the buffer's pixel fd to the App zero-copy; SHM is the fallback.
    println!("Frame path: dmabuf zero-copy (KWin GPU → fd → App), SHM fallback");
    println!();

    println!("doctor check complete.");
}
