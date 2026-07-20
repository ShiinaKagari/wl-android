/// Doctor subcommand — diagnostic self-check
pub fn run() {
    println!("wl-android doctor");
    println!("==================");
    println!();

    // Check environment
    let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "land-0".into());
    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let land_socket = std::env::var("LAND_SOCKET")
        .unwrap_or_else(|_| "/run/wl-android/land.sock".into());

    println!("Environment:");
    println!("  WAYLAND_DISPLAY  = {wayland_display}");
    println!("  XDG_RUNTIME_DIR  = {xdg_runtime}");
    println!("  LAND_SOCKET      = {land_socket}");
    println!("  LAND_MODE        = {}", std::env::var("LAND_MODE").unwrap_or_else(|_| "auto".into()));
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
            println!("   Droidspaces bind mount needed:");
            println!("   /data/local/tmp/wl-android → /run/wl-android");
        }
    }

    // Check GPU devices
    for dev in &["/dev/kgsl-3d0", "/dev/dri/renderD128"] {
        if std::path::Path::new(dev).exists() {
            println!("✅ GPU device: {dev}");
        }
    }

    // Check for Vulkan (turnip)
    let turnip_check = std::process::Command::new("vulkaninfo")
        .arg("--summary")
        .output();
    match turnip_check {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains("Turnip") || stdout.contains("turnip") {
                println!("✅ turnip Vulkan driver detected");
            } else {
                println!("✅ vulkaninfo available (driver unknown)");
            }
        }
        _ => {
            println!("⚠️  vulkaninfo not found — install mesa-for-android-container >= 26.1");
        }
    }

    // Protocol version
    println!();
    println!("Protocol: v{}", wl_android_common::proto::PROTOCOL_VERSION);
    println!("Mode: blit (Adreno 830 — direct dmabuf import unavailable)");
    println!();
    println!("doctor check complete.");
}
