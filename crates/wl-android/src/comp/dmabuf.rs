use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::wayland::buffer::BufferHandler;
use tracing::{debug, warn};

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
        let num_planes = dmabuf.num_planes();
        debug!(num_planes, "dmabuf imported");

        // TODO 25: the eager import here is redundant — the KWin frame fd is
        // dup'd and stashed at commit time (extract_from_buffer →
        // pending_frames) and lazily imported in blit_and_send_frame via the
        // PERF-11 frame_images cache (state.rs). This handler only accepts the
        // dmabuf so KWin's wl_dmabuf negotiation completes; notifier.successful
        // is the acceptance signal — dropping it would reject KWin's buffer.
        let handles: Vec<_> = dmabuf.handles().collect();
        if handles.is_empty() {
            warn!("dmabuf with no planes");
            notifier.invalid_format();
            return;
        }

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
