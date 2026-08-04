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

    probe_fence_capabilities();

    // Protocol version
    println!();
    println!("Protocol: v{}", wl_android_common::proto::PROTOCOL_VERSION);

    // Effective frame mode (TODO 32): resolved from LAND_MODE, not hardcoded.
    let land_mode = std::env::var("LAND_MODE").unwrap_or_default();
    println!("Mode: {}", effective_mode(&land_mode));
    println!();

    // PERF-11 (steady-state zero-import): the import counter lives inside the
    // RUNNING server (state.rs frame_images buffer-id cache / blit.rs lazy
    // import). doctor is a SEPARATE process (`wl-android doctor`) and cannot
    // read the server's memory, so it reports the MEASUREMENT METHOD rather
    // than a fabricated number. Honest method-report, no fake stats.
    println!("PERF-11 (steady-state zero-import): counter lives in the RUNNING server");
    println!("  (state.rs frame_images buffer-id cache, lazy import in blit_and_send_frame)");
    println!("  — doctor is a separate process and cannot read it. Measure at runtime:");
    println!("    server log 'dmabuf imported' (debug level, comp/dmabuf.rs) with");
    println!("    RUST_LOG=debug (README also documents LAND_LOG=debug): grep the log");
    println!("    for import events; steady state = NO new imports after warmup");
    println!("    (buffer_id cache hit ⇒ App reuses imported images, counter stays flat).");
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

    // App-side fence capability: asserted at App startup (lane 30), NOT here —
    // this doctor probe covers the server-side turnip driver only.
    println!("App SYNC_FD: asserted at App runtime (nativeSetSurface log");
    println!("  'SYNC_FD semaphore import SUPPORTED|UNSUPPORTED')");
    println!();
    println!("doctor check complete.");
}

/// Resolve the effective frame mode from `LAND_MODE`.
///
/// `auto` is the default — used for unset/empty/garbage values — and resolves
/// to the dmabuf blit path, the only frame path available on Adreno 830 (the
/// host driver does not support direct dmabuf import, so `direct` is
/// documented but unavailable). `shm` forces the debug CPU path; `blit` forces
/// the blit path explicitly. Matching is case-insensitive on trimmed input;
/// malformed values never panic and always fall back to `auto`.
///
/// Pure seam — tested verbatim in `.omo/start-work/doctor-harness/`.
pub fn effective_mode(env: &str) -> &'static str {
    let mode = env.trim().to_ascii_lowercase();
    match mode.as_str() {
        "shm" => "shm (debug CPU path)",
        "direct" => "direct (unavailable on Adreno 830 — dmabuf import unsupported by host driver)",
        "blit" => "blit (explicit dmabuf blit path)",
        // "auto", "", or unknown → the default dmabuf blit path
        _ => "blit (default: dmabuf blit path)",
    }
}

fn probe_fence_capabilities() {
    use ash::vk;
    use std::ffi::CString;

    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            println!("⚠️  FENCE: vulkan unavailable — cannot probe (Entry::load: {e})");
            return;
        }
    };

    let app_name = CString::new("wl-android-doctor").unwrap();
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(vk::API_VERSION_1_3);
    let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    let instance = match unsafe { entry.create_instance(&create_info, None) } {
        Ok(i) => i,
        Err(e) => {
            println!("⚠️  FENCE: vulkan unavailable — cannot probe (create_instance: {e})");
            return;
        }
    };

    let pdevices = match unsafe { instance.enumerate_physical_devices() } {
        Ok(d) if !d.is_empty() => d,
        other => {
            match other {
                Ok(_) => println!("⚠️  FENCE: vulkan unavailable — cannot probe (no physical devices)"),
                Err(e) => println!("⚠️  FENCE: vulkan unavailable — cannot probe (enumerate: {e})"),
            }
            unsafe { instance.destroy_instance(None) };
            return;
        }
    };

    let info = vk::PhysicalDeviceExternalFenceInfo::default()
        .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
    let mut props = vk::ExternalFenceProperties::default();
    unsafe {
        instance.get_physical_device_external_fence_properties(pdevices[0], &info, &mut props);
    }
    unsafe { instance.destroy_instance(None) };

    let features = props.external_fence_features;
    let exportable = features.contains(vk::ExternalFenceFeatureFlags::EXPORTABLE);
    let importable = features.contains(vk::ExternalFenceFeatureFlags::IMPORTABLE);
    let roundtrip = props
        .export_from_imported_handle_types
        .contains(vk::ExternalFenceHandleTypeFlags::SYNC_FD);

    if exportable {
        println!("✅ FENCE: SYNC_FD export supported");
    }
    if importable {
        println!("✅ FENCE: SYNC_FD import supported");
    }
    if !exportable || !importable {
        println!("⚠️  FENCE: SYNC_FD not supported → CPU-wait downgrade path");
        println!("   features = {features:?}");
    } else {
        println!(
            "   import→export roundtrip: {}",
            if roundtrip { "supported" } else { "NOT supported" }
        );
    }
}
