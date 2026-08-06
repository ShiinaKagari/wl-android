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
            println!("   Ensure LAND_SOCKET points to a writable path");
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

    // Frame mode: SHM-only protocol — the App presents via ANativeWindow_lock.
    println!("Mode: SHM (CPU frame path — the only frame path)");
    println!();

    // PERF-02/03 (end-to-end latency p95): cross-process by nature — commit
    // time is stamped on the server, present-complete on the App. doctor is
    // server-side only and cannot measure both ends alone.
    println!("PERF-02/03 (end-to-end latency p95 @60/144Hz): measured at the App —");
    println!("  serial-timestamp diff of commit(server) ↔ present(App); App doctor");
    println!("  page aggregates p95. doctor (server-side) cannot measure");
    println!("  cross-process latency alone; frame flow is spot-checked by");
    println!("  scripts/deploy-test.sh verify (logcat 'Frame received|render:'");
    println!("  counts at T0 vs T3 — serials advance ⇒ frames are flowing).");
    println!();
    println!("doctor check complete.");
}
