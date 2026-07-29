/// Vulkan rendering state (Android only, cfg-gated).
#[cfg(target_os = "android")]
pub struct RenderState {
    initialized: bool,
}

#[cfg(not(target_os = "android"))]
pub struct RenderState {
    initialized: bool,
}

impl RenderState {
    pub fn new() -> Self {
        Self { initialized: false }
    }

    #[allow(unused_variables)]
    pub fn init(&mut self, window: *mut std::ffi::c_void) -> Result<(), String> {
        // M6b: Vulkan init + swapchain from ANativeWindow
        Ok(())
    }

    #[allow(unused_variables)]
    pub fn import_frame(&self, fd: std::os::fd::RawFd, width: u32, height: u32, format: u32) -> Result<u64, String> {
        Ok(0)
    }

    #[allow(unused_variables)]
    pub fn present_frame(&self, image_handle: u64) -> Result<(), String> {
        Ok(())
    }
}
