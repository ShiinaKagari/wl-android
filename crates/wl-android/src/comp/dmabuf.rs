use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::wayland::buffer::BufferHandler;
use smithay::backend::allocator::Buffer;
use tracing::{debug, error, warn};
use drm_fourcc::DrmFourcc;
use std::os::fd::OwnedFd;
use std::os::fd::AsRawFd as _;

use crate::state::WlState;

fn fourcc_to_vk(fourcc: DrmFourcc) -> ash::vk::Format {
    match fourcc {
        DrmFourcc::Argb8888 | DrmFourcc::Xrgb8888 => ash::vk::Format::B8G8R8A8_UNORM,
        DrmFourcc::Abgr8888 | DrmFourcc::Xbgr8888 => ash::vk::Format::R8G8B8A8_UNORM,
        _ => ash::vk::Format::B8G8R8A8_UNORM, // fallback
    }
}

pub fn build_default_feedback() -> smithay::wayland::dmabuf::DmabufFeedback {
    use drm_fourcc::DrmFourcc;
    use drm_fourcc::DrmModifier;
    use smithay::backend::allocator::Format;

    let modifiers = &[
        DrmModifier::Linear,
        DrmModifier::from(0x0800_0000_0000_0005u64), // QCOM_COMPRESSED
    ];
    let fourccs = &[
        DrmFourcc::Xrgb8888,
        DrmFourcc::Argb8888,
        DrmFourcc::Xbgr8888,
        DrmFourcc::Abgr8888,
    ];

    let formats: Vec<Format> = fourccs
        .iter()
        .flat_map(|&fourcc| {
            modifiers.iter().map(move |&modifier| Format {
                code: fourcc,
                modifier,
            })
        })
        .collect();

    DmabufFeedbackBuilder::new(0, formats)
        .build()
        .expect("build dmabuf feedback")
}

impl DmabufHandler for WlState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let num_planes = dmabuf.num_planes();
        debug!(num_planes, "dmabuf imported");

        let handles: Vec<_> = dmabuf.handles().collect();
        if handles.is_empty() {
            warn!("dmabuf with no planes");
            notifier.invalid_format();
            return;
        }

        // Dup the first plane's fd (Dmabuf retains ownership)
        let raw_fd = handles[0].as_raw_fd();
        let fd = unsafe { OwnedFd::from_raw_fd(nix::unistd::dup(raw_fd).unwrap()) };

        // Import into blit engine
        let format = dmabuf.format();
        let modifier: u64 = u64::from(format.modifier);
        let vk_format = fourcc_to_vk(format.code);

        match self.blit_engine.import_dmabuf(fd, 0, 0, vk_format, modifier) {
            Ok(handle) => {
                self.blit_image_handles.push(handle);
                let _ = notifier.successful::<Self>();
            }
            Err(e) => {
                error!(err = %e, "blit import failed");
                notifier.failed();
            }
        }
    }
}

impl BufferHandler for WlState {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        if let Ok(_dmabuf) = get_dmabuf(buffer) {
            debug!("dmabuf buffer destroyed");
        }
    }
}

use smithay::delegate_dmabuf;
delegate_dmabuf!(WlState);
