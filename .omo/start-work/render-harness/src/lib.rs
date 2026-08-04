// Pure-seam harness for android-app/native/src/render.rs (TODO 26).
// The helper functions below are a VERBATIM copy of the ones in render.rs;
// they only touch ash::vk POD types, so they compile and run on the host.

use ash::vk;

/// Preferred swapchain format: B8G8R8A8 is the de-facto Android compositor
/// format (gralloc maps it to AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM-class
/// buffers on Adreno). Fallback R8G8B8A8_UNORM matches lane 23's server blit
/// source (ABGR8888 == VK_FORMAT_R8G8B8A8_UNORM) exactly.
pub(crate) const PREFERRED_FORMATS: [vk::Format; 2] = [
    vk::Format::B8G8R8A8_UNORM,
    vk::Format::R8G8B8A8_UNORM,
];

/// Choose the swapchain surface format.
///
/// Preference order: B8G8R8A8_UNORM + SRGB_NONLINEAR, then R8G8B8A8_UNORM +
/// SRGB_NONLINEAR, then the first advertised format. A list whose only entry
/// is `FORMAT_UNDEFINED` (or an empty list, defensively) means "the driver
/// imposes no constraint", in which case we pick our first preference.
pub(crate) fn pick_format(formats: &[vk::SurfaceFormatKHR]) -> vk::SurfaceFormatKHR {
    let unconstrained = formats.is_empty()
        || (formats.len() == 1 && formats[0].format == vk::Format::UNDEFINED);
    if unconstrained {
        return vk::SurfaceFormatKHR {
            format: PREFERRED_FORMATS[0],
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        };
    }
    for wanted in PREFERRED_FORMATS {
        if let Some(f) = formats
            .iter()
            .find(|f| f.format == wanted && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
        {
            return *f;
        }
    }
    // Accept a preferred format with any color space before giving up.
    for wanted in PREFERRED_FORMATS {
        if let Some(f) = formats.iter().find(|f| f.format == wanted) {
            return *f;
        }
    }
    formats[0]
}

/// PERF-15: MAILBOX (low-latency triple buffering, never blocks the producer)
/// when available; FIFO is guaranteed by the spec to always be present.
pub(crate) fn pick_present_mode(modes: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    if modes.contains(&vk::PresentModeKHR::MAILBOX) {
        vk::PresentModeKHR::MAILBOX
    } else {
        vk::PresentModeKHR::FIFO
    }
}

/// Resolve the swapchain extent. Drivers may report `current_extent.width ==
/// u32::MAX` to mean "the app chooses within [min,max]" (Wayland-style);
/// Android SurfaceFlinger surfaces always report a real current extent, but
/// handle both per the spec.
pub(crate) fn clamp_extent(caps: &vk::SurfaceCapabilitiesKHR) -> vk::Extent2D {
    if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        vk::Extent2D {
            width: caps
                .current_extent
                .width
                .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: caps
                .current_extent
                .height
                .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        }
    }
}

/// Triple-buffer target (PERF-15 with MAILBOX), clamped to what the surface
/// allows. `max_image_count == 0` means unlimited.
pub(crate) fn pick_image_count(caps: &vk::SurfaceCapabilitiesKHR) -> u32 {
    let max = if caps.max_image_count == 0 {
        u32::MAX
    } else {
        caps.max_image_count
    };
    3.clamp(caps.min_image_count, max)
}

/// Composite alpha: prefer OPAQUE (no blending against the wallpaper, the
/// common Android SurfaceView case), else INHERIT, else the first advertised
/// bit.
pub(crate) fn pick_composite_alpha(
    supported: vk::CompositeAlphaFlagsKHR,
) -> vk::CompositeAlphaFlagsKHR {
    for wanted in [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ] {
        if supported.contains(wanted) {
            return wanted;
        }
    }
    // First set bit, or OPAQUE if the driver reported nothing (invalid but
    // seen in the wild on old Adreno blobs).
    if supported.is_empty() {
        vk::CompositeAlphaFlagsKHR::OPAQUE
    } else {
        vk::CompositeAlphaFlagsKHR::from_raw(supported.as_raw() & supported.as_raw().wrapping_neg())
    }
}

/// Negotiate swapchain image usage. COLOR_ATTACHMENT is mandatory (the images
/// are render targets / blit destinations); TRANSFER_DST is required for the
/// lane 29 server blit path (vkCmdBlitImage/vkCmdCopyImage into the acquired
/// image). Intersect with what the surface supports and report the result —
/// if COLOR_ATTACHMENT itself is unsupported the surface is unusable and the
/// caller must error out.
pub(crate) fn pick_image_usage(supported: vk::ImageUsageFlags) -> vk::ImageUsageFlags {
    const WANTED: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
        vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
            | vk::ImageUsageFlags::TRANSFER_DST.as_raw(),
    );
    let usable = supported & WANTED;
    if usable.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT) {
        usable
    } else {
        // Caller checks for COLOR_ATTACHMENT and errors; keep TRANSFER_DST if
        // that's somehow the only thing offered so the log line is truthful.
        usable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(format: vk::Format) -> vk::SurfaceFormatKHR {
        vk::SurfaceFormatKHR {
            format,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }
    }

    #[test]
    fn format_prefers_bgra_over_rgba() {
        let formats = [fmt(vk::Format::R8G8B8A8_UNORM), fmt(vk::Format::B8G8R8A8_UNORM)];
        assert_eq!(pick_format(&formats).format, vk::Format::B8G8R8A8_UNORM);
    }

    #[test]
    fn format_falls_back_to_rgba() {
        let formats = [fmt(vk::Format::R8G8B8A8_UNORM), fmt(vk::Format::R5G6B5_UNORM_PACK16)];
        assert_eq!(pick_format(&formats).format, vk::Format::R8G8B8A8_UNORM);
    }

    #[test]
    fn format_takes_first_when_neither_preferred() {
        let formats = [fmt(vk::Format::R5G6B5_UNORM_PACK16), fmt(vk::Format::A2B10G10R10_UNORM_PACK32)];
        assert_eq!(pick_format(&formats).format, vk::Format::R5G6B5_UNORM_PACK16);
    }

    #[test]
    fn format_undefined_list_means_unconstrained() {
        let formats = [vk::SurfaceFormatKHR {
            format: vk::Format::UNDEFINED,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }];
        assert_eq!(pick_format(&formats).format, vk::Format::B8G8R8A8_UNORM);
    }

    #[test]
    fn format_empty_list_means_unconstrained() {
        assert_eq!(pick_format(&[]).format, vk::Format::B8G8R8A8_UNORM);
    }

    #[test]
    fn present_mode_prefers_mailbox() {
        let modes = [vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX];
        assert_eq!(pick_present_mode(&modes), vk::PresentModeKHR::MAILBOX);
    }

    #[test]
    fn present_mode_falls_back_to_fifo() {
        let modes = [vk::PresentModeKHR::FIFO, vk::PresentModeKHR::FIFO_RELAXED];
        assert_eq!(pick_present_mode(&modes), vk::PresentModeKHR::FIFO);
    }

    #[test]
    fn extent_uses_current_when_valid() {
        let mut caps = vk::SurfaceCapabilitiesKHR::default();
        caps.current_extent = vk::Extent2D { width: 1080, height: 2400 };
        caps.min_image_extent = vk::Extent2D { width: 1, height: 1 };
        caps.max_image_extent = vk::Extent2D { width: 4096, height: 4096 };
        assert_eq!(clamp_extent(&caps), vk::Extent2D { width: 1080, height: 2400 });
    }

    #[test]
    fn extent_clamps_when_sentinel() {
        // u32::MAX sentinel: driver lets the app choose. Our copy clamps the
        // sentinel into [min,max] — the app picks the max available.
        let mut caps = vk::SurfaceCapabilitiesKHR::default();
        caps.current_extent = vk::Extent2D { width: u32::MAX, height: u32::MAX };
        caps.min_image_extent = vk::Extent2D { width: 64, height: 64 };
        caps.max_image_extent = vk::Extent2D { width: 2560, height: 1440 };
        let e = clamp_extent(&caps);
        assert!(e.width >= 64 && e.width <= 2560);
        assert!(e.height >= 64 && e.height <= 1440);
        assert_ne!(e.width, u32::MAX);
    }

    #[test]
    fn image_count_triple_buffered_and_clamped() {
        let mut caps = vk::SurfaceCapabilitiesKHR::default();
        caps.min_image_count = 2;
        caps.max_image_count = 0; // unlimited
        assert_eq!(pick_image_count(&caps), 3);
        caps.max_image_count = 2;
        assert_eq!(pick_image_count(&caps), 2);
        caps.min_image_count = 4;
        caps.max_image_count = 8;
        assert_eq!(pick_image_count(&caps), 4);
    }

    #[test]
    fn composite_alpha_prefers_opaque() {
        let supported = vk::CompositeAlphaFlagsKHR::INHERIT | vk::CompositeAlphaFlagsKHR::OPAQUE;
        assert_eq!(pick_composite_alpha(supported), vk::CompositeAlphaFlagsKHR::OPAQUE);
        let inherit_only = vk::CompositeAlphaFlagsKHR::INHERIT;
        assert_eq!(pick_composite_alpha(inherit_only), vk::CompositeAlphaFlagsKHR::INHERIT);
        let empty = vk::CompositeAlphaFlagsKHR::empty();
        assert_eq!(pick_composite_alpha(empty), vk::CompositeAlphaFlagsKHR::OPAQUE);
    }

    #[test]
    fn usage_masks_unsupported_bits() {
        let supported = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST;
        let got = pick_image_usage(supported);
        assert!(got.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
        assert!(got.contains(vk::ImageUsageFlags::TRANSFER_DST));
        let no_transfer = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
        let got = pick_image_usage(no_transfer);
        assert!(got.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
        assert!(!got.contains(vk::ImageUsageFlags::TRANSFER_DST));
        assert!(!got.contains(vk::ImageUsageFlags::SAMPLED));
    }
}
