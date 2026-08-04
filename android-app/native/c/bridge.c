#include <android/native_window_jni.h>

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
    return ANativeWindow_setBuffersGeometry(window, 0, 0, WINDOW_FORMAT_BGRA_8888);
}
