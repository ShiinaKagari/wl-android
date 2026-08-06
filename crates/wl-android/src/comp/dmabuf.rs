use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::dmabuf::{get_dmabuf, DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use tracing::{debug, warn};

use crate::state::WlState;

/// Default dmabuf feedback: advertise the container's render node plus the
/// formats KWin's EGL stack may allocate. ONLY LINEAR is advertised — the
/// CPU read-back path (extract_from_dmabuf) cannot handle QCOM_COMPRESSED
/// (UBWC) buffers, and advertising them invites gbm to pick the compressed
/// modifier, which would make every frame unreadable. KWin rendering into
/// linear buffers is still GPU-accelerated, just without UBWC compression.
pub fn build_default_feedback() -> DmabufFeedback {
    use drm_fourcc::DrmFourcc;
    use drm_fourcc::DrmModifier;
    use smithay::backend::allocator::Format;

    let modifiers = &[DrmModifier::Linear];
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

    // Advertise the container's render node dev_t. KWin (nested client)
    // resolves its GPU via drmGetDeviceFromDevId on this value — dev_t 0
    // made it fail with "drmGetDeviceFromDevId() failed" and fall back to
    // no rendering backend. Discover the real device dynamically so the
    // value matches what the kernel exposes on this machine.
    let device_id = discover_render_node_dev_t().unwrap_or(0);
    DmabufFeedbackBuilder::new(device_id, formats)
        .build()
        .expect("build dmabuf feedback")
}

/// stat(2) the first render node under /dev/dri and return its st_rdev
/// (dev_t), or None when no render node exists.
fn discover_render_node_dev_t() -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let dir = std::fs::read_dir("/dev/dri").ok()?;
    for entry in dir.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("renderD") {
            if let Ok(meta) = std::fs::metadata(&path) {
                return Some(meta.rdev());
            }
        }
    }
    None
}

impl DmabufHandler for WlState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let num_planes = dmabuf.num_planes();
        debug!(num_planes, "dmabuf imported");

        // Accept the buffer so KWin's wl_dmabuf negotiation completes; the
        // actual read-back happens at commit time (extract_from_buffer →
        // FrameCache). Rejecting here would make KWin's EGL surface fail.
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
        if get_dmabuf(buffer).is_ok() {
            debug!("dmabuf buffer destroyed");
        }
    }
}

use smithay::delegate_dmabuf;
delegate_dmabuf!(WlState);
