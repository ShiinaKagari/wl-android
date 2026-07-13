mod backend;
mod dmabuf;
mod socket;

use backend::LandBackend;
use socket::FrameSender;

/// 入口：创建 wl-android 渲染器包装。
///
/// 合成器调用此函数获取包装的渲染器指针，后续所有 buffer commit
/// 将触发 DMA-BUF fd 提取与转发。
///
/// # SAFETY
/// - `renderer` 必须指向有效的 `wlr_renderer`
/// - `wl_display` 必须指向有效的 `wl_display`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn land_wlroots_renderer_create(
    renderer: *mut std::ffi::c_void,
    _wl_display: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void {
    let socket_path = land_common::types::default_socket_path();

    let ret = (|| -> Result<*mut std::ffi::c_void, Box<dyn std::error::Error>> {
        let sender = FrameSender::connect(&socket_path)?;
        // SAFETY: `renderer` is guaranteed valid by the caller per the `land_wlroots_renderer_create` contract
        let backend = unsafe { LandBackend::wrap(renderer, sender)? };
        let boxed = Box::into_raw(Box::new(backend));
        Ok(boxed as *mut std::ffi::c_void)
    })();

    match ret {
        Ok(ptr) => ptr,
        Err(e) => {
            eprintln!("[land] failed to create backend: {}", e);
            std::ptr::null_mut()
        }
    }
}

/// 销毁后端。
///
/// # SAFETY
/// - `backend` 必须由 `land_wlroots_renderer_create` 返回且尚未销毁
#[unsafe(no_mangle)]
pub unsafe extern "C" fn land_wlroots_renderer_destroy(backend: *mut std::ffi::c_void) {
    if !backend.is_null() {
        // SAFETY: `backend` was created by `Box::into_raw` in `land_wlroots_renderer_create`, so it is safe to reconstruct the Box and drop it
        unsafe {
            drop(Box::from_raw(backend as *mut LandBackend));
        }
    }
}

/// 当 buffer 被提交到 wlroots 时调用。
///
/// # SAFETY
/// - `backend` 必须由 `land_wlroots_renderer_create` 返回且尚未销毁
/// - `buffer` 必须指向有效的 `wlr_buffer`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn land_wlroots_buffer_submit(
    backend: *mut std::ffi::c_void,
    buffer: *mut std::ffi::c_void,
) -> bool {
    // SAFETY: `backend` is guaranteed valid per the `land_wlroots_renderer_destroy` contract and non-null at this point
    let backend = unsafe { &*(backend as *mut LandBackend) };
    // SAFETY: `buffer` is guaranteed valid by the caller per the `land_wlroots_buffer_submit` contract
    match unsafe { backend.on_buffer_submit(buffer) } {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[land] buffer submit failed: {}", e);
            false
        }
    }
}
