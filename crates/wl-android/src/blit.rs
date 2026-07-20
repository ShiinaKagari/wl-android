/// Vulkan GPU-blit engine (blit fallback path).
/// Imports dmabuf from KWin (turnip) and dmabuf from AHB pool (App),
/// blits KWin output into the AHB target, then signals completion.
///
/// Uses ash for Vulkan. Full Vulkan init + blit is deferred to M6b
/// when turnip is available in the container for testing.
/// This module provides the skeleton API.
use std::os::fd::OwnedFd;

#[allow(dead_code)]
pub struct BlitEngine {
    // M6b: ash Instance, Device, Queue, CommandPool, etc.
    initialized: bool,
}

#[allow(dead_code)]
pub struct BlitResult {
    /// Whether the blit was queued successfully.
    pub queued: bool,
    /// The target slot index that received the blit.
    pub slot: u32,
}

#[allow(dead_code)]
impl BlitEngine {
    pub fn new() -> Self {
        Self { initialized: false }
    }

    /// Initialize Vulkan (ash). Called once at startup.
    /// Returns Ok if Vulkan device is available (turnip).
    pub fn init(&mut self) -> Result<(), String> {
        // M6b: ash::Entry::load() → pick physical device (Adreno)
        // → create logical device with VK_KHR_external_memory_fd
        // → create command pool, transfer queue
        // For now: probe-only, return Ok if libvulkan is loadable
        match unsafe { ash::Entry::load() } {
            Ok(_entry) => {
                self.initialized = true;
                Ok(())
            }
            Err(_) => Err("Vulkan loader not found".into()),
        }
    }

    /// Import a dmabuf from KWin as a source VkImage.
    /// Returns an opaque handle for the imported image.
    pub fn import_source(&self, _fd: OwnedFd, _width: u32, _height: u32, _format: u32, _modifier: u64) -> Result<u64, String> {
        if !self.initialized {
            return Err("blit engine not initialized".into());
        }
        // M6b: vkCreateImage + vkImportMemoryFd + vkBindImageMemory
        Ok(0)
    }

    /// Import a dmabuf from AHB pool as a target VkImage.
    pub fn import_target(&self, _fd: OwnedFd, _width: u32, _height: u32) -> Result<u64, String> {
        if !self.initialized {
            return Err("blit engine not initialized".into());
        }
        Ok(0)
    }

    /// Blit from source image to target image (slot).
    /// Returns immediately; caller must wait for fence before reading target.
    pub fn blit(&self, _src_handle: u64, _dst_handle: u64, _width: u32, _height: u32) -> Result<BlitResult, String> {
        if !self.initialized {
            return Err("blit engine not initialized".into());
        }
        Ok(BlitResult { queued: true, slot: 0 })
    }

    /// Wait for a previously queued blit to complete.
    pub fn wait_blit(&self, _slot: u32) -> Result<(), String> {
        if !self.initialized {
            return Err("blit engine not initialized".into());
        }
        Ok(())
    }

    /// Destroy an imported image.
    pub fn destroy_image(&self, _handle: u64) {}
}
