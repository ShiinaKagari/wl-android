#include <android/native_window_jni.h>

ANativeWindow* wl_get_native_window(JNIEnv* env, jobject surface) {
    return ANativeWindow_fromSurface(env, surface);
}

int wl_lock_window(ANativeWindow* window, ANativeWindow_Buffer* outBuf) {
    return ANativeWindow_lock(window, outBuf, NULL);
}

int wl_unlock_and_post(ANativeWindow* window) {
    return ANativeWindow_unlockAndPost(window);
}

int wl_acquire_window(ANativeWindow* window) {
    return ANativeWindow_acquire(window);
}
