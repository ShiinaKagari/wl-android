use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::wayland::buffer::BufferHandler;
use tracing::debug;

use crate::state::WlState;

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
        debug!(planes = dmabuf.num_planes(), "dmabuf imported");
        let _ = notifier.successful::<Self>();
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
