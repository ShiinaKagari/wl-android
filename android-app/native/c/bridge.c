#include <android/native_window_jni.h>

ANativeWindow* wl_get_native_window(JNIEnv* env, jobject surface) {
    return ANativeWindow_fromSurface(env, surface);
}

void wl_lock_window(ANativeWindow* window, ANativeWindow_Buffer* outBuf) {
    ANativeWindow_lock(window, outBuf, NULL);
}

void wl_unlock_and_post(ANativeWindow* window) {
    ANativeWindow_unlockAndPost(window);
}
