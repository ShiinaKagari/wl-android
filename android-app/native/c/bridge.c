#include <android/native_window_jni.h>
#include <dlfcn.h>

/* NDK 27's native_window.h does not define WINDOW_FORMAT_BGRA_8888.
 * Value 5 matches android.graphics.PixelFormat.BGRA_8888. */
#ifndef WINDOW_FORMAT_BGRA_8888
#define WINDOW_FORMAT_BGRA_8888 5
#endif

ANativeWindow* wl_get_native_window(JNIEnv* env, jobject surface) {
    return ANativeWindow_fromSurface(env, surface);
}

int wl_lock_window(ANativeWindow* window, ANativeWindow_Buffer* outBuf) {
    return ANativeWindow_lock(window, outBuf, NULL);
}

int wl_unlock_and_post(ANativeWindow* window) {
    return ANativeWindow_unlockAndPost(window);
}

void wl_acquire_window(ANativeWindow* window) {
    ANativeWindow_acquire(window);
}

int wl_set_format(ANativeWindow* window) {
    // SOM-BUF: three-buffer pool (like Sommelier's fixed shm pool) so the
    // render thread never overwrites a buffer SurfaceFlinger is still
    // displaying — the "jumping between frames" artifact. Default is 2.
    //
    // ANativeWindow_setBufferCount is API 35+ and absent from the device's
    // libandroid — resolve dynamically and skip when unavailable (a hard
    // reference would fail dlopen with UnsatisfiedLinkError).
    typedef int32_t (*set_buf_count_fn)(ANativeWindow*, size_t);
    set_buf_count_fn set_count =
        (set_buf_count_fn)dlsym(RTLD_DEFAULT, "ANativeWindow_setBufferCount");
    if (set_count != NULL) {
        set_count(window, 3);
    }
    return ANativeWindow_setBuffersGeometry(window, 0, 0, WINDOW_FORMAT_BGRA_8888);
}

/* SCALE: set the buffer geometry to the render target resolution (the scaled
 * size KWin renders at). The SurfaceView stays fullscreen, so SurfaceFlinger
 * stretches the smaller buffer to fill the panel. width/height of 0 keeps the
 * surface's own size (fullscreen, no scaling). */
int wl_set_dimensions(ANativeWindow* window, int width, int height) {
    return ANativeWindow_setBuffersGeometry(window, width, height, WINDOW_FORMAT_BGRA_8888);
}
