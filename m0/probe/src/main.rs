use ash::vk;
use std::ffi::CString;

fn arr_to_str(arr: &[u8]) -> String {
    let end = arr.iter().position(|&b| b == 0).unwrap_or(arr.len());
    String::from_utf8_lossy(&arr[..end]).to_string()
}

fn main() {
    println!("=== M0: Host Vulkan Driver Probe ===");
    println!("Target: Snapdragon 8 Elite / Adreno 830 / OnePlus Pad 3");
    println!();

    let app_name = CString::new("m0-probe").unwrap();
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .application_version(vk::make_api_version(0, 1, 0, 0))
        .engine_name(&app_name)
        .engine_version(vk::make_api_version(0, 1, 0, 0))
        .api_version(vk::API_VERSION_1_3);

    let instance_extensions: Vec<&str> = vec![];
    let layer_names: Vec<&str> = vec![];

    let ext_names: Vec<CString> = instance_extensions
        .iter()
        .map(|e| CString::new(*e).unwrap())
        .collect();
    let layer_cstrs: Vec<CString> = layer_names
        .iter()
        .map(|l| CString::new(*l).unwrap())
        .collect();

    let ext_ptrs: Vec<_> = ext_names.iter().map(|e| e.as_ptr()).collect();
    let layer_ptrs: Vec<_> = layer_cstrs.iter().map(|l| l.as_ptr()).collect();

    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&ext_ptrs)
        .enabled_layer_names(&layer_ptrs);

    let entry = unsafe { ash::Entry::load() }.expect("Failed to load Vulkan loader");
    let instance = unsafe { entry.create_instance(&create_info, None) }
        .expect("Failed to create Vulkan instance");

    // Enumerate instance extensions
    println!("--- Instance Extensions ---");
    let extensions = unsafe {
        entry.enumerate_instance_extension_properties(None)
    }
    .unwrap_or_default();
    let inst_ext_names: Vec<String> = extensions
        .iter()
        .map(|e| arr_to_str(&e.extension_name))
        .collect();

    check_ext("VK_KHR_external_memory_fd", &inst_ext_names);
    check_ext("VK_KHR_external_semaphore_fd", &inst_ext_names);
    check_ext("VK_KHR_external_fence_fd", &inst_ext_names);
    check_ext("VK_KHR_get_physical_device_properties2", &inst_ext_names);

    // Get physical device
    let pdevices = unsafe { instance.enumerate_physical_devices() }
        .expect("Failed to enumerate physical devices");
    if pdevices.is_empty() {
        eprintln!("ERROR: No Vulkan physical devices found!");
        return;
    }

    let pdevice = pdevices[0];
    let props = unsafe { instance.get_physical_device_properties(pdevice) };
    let device_name = arr_to_str(&props.device_name);
    let api_ver = (
        vk::api_version_major(props.api_version),
        vk::api_version_minor(props.api_version),
        vk::api_version_patch(props.api_version),
    );

    println!();
    println!("--- Device Info ---");
    println!("  Name:        {device_name}");
    println!("  Vulkan API:  {}.{}.{}", api_ver.0, api_ver.1, api_ver.2);
    println!(
        "  Driver ver:  0x{:08X}",
        props.driver_version
    );
    println!(
        "  Device type: {:?}",
        props.device_type
    );

    // Enumerate device extensions
    println!();
    println!("--- Device Extensions ---");
    let dev_extensions = unsafe {
        instance.enumerate_device_extension_properties(pdevice)
    }
    .unwrap_or_default();
    let dev_ext_names: Vec<String> = dev_extensions
        .iter()
        .map(|e| arr_to_str(&e.extension_name))
        .collect();

    // Critical for direct dmabuf import path
    println!();
    println!("=== KEY EXTENSIONS (for wl-android paths) ===");
    check_ext("VK_EXT_external_memory_dma_buf", &dev_ext_names);
    check_ext("VK_EXT_image_drm_format_modifier", &dev_ext_names);
    check_ext("VK_ANDROID_external_memory_android_hardware_buffer", &dev_ext_names);

    // Determine path recommendation
    let has_dma_import = dev_ext_names.contains(&"VK_EXT_external_memory_dma_buf".to_string());
    let has_modifier = dev_ext_names.contains(&"VK_EXT_image_drm_format_modifier".to_string());
    let has_ahb = dev_ext_names.contains(
        &"VK_ANDROID_external_memory_android_hardware_buffer".to_string(),
    );

    println!();
    println!("=== RESULT ===");
    let dma_ok = has_dma_import && has_modifier;
    println!("  Direct dmabuf import:   {}", ok_str(dma_ok));
    println!("  AHB fallback (blit):    {}", ok_str(has_ahb));
    println!();

    if dma_ok {
        println!("  → Primary path:  DIRECT dmabuf import");
        println!("  → Fallback path:  Not needed, but available via AHB");
        println!("  → Set LAND_MODE=direct (or auto will select direct)");
    } else if has_ahb {
        println!("  → Primary path:  BLIT (via AHardwareBuffer pool)");
        println!("  → Set LAND_MODE=blit for explicit control");
        println!("  → Direct path unavailable on this driver");
    } else {
        println!("  → ERROR: Neither dmabuf import nor AHB available!");
        println!("  → wl-android cannot render frames on this device");
    }

    // Additional useful extensions
    println!();
    println!("--- Other Relevant Device Extensions ---");
    for ext in &[
        "VK_KHR_swapchain",
        "VK_KHR_maintenance1",
        "VK_KHR_maintenance2",
        "VK_KHR_maintenance3",
        "VK_EXT_swapchain_maintenance1",
        "VK_KHR_swapchain_mutable_format",
    ] {
        check_ext(ext, &dev_ext_names);
    }

    unsafe {
        instance.destroy_instance(None);
    }
}

fn check_ext(name: &str, available: &[String]) {
    let found = available.iter().any(|e| e == name);
    println!("  {:>50}  {}", name, ok_str(found));
}

fn ok_str(ok: bool) -> &'static str {
    if ok {
        "✅ OK"
    } else {
        "❌ MISSING"
    }
}
