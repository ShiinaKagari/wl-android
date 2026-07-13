#include <jni.h>
#include <android/log.h>
#include <android/native_window.h>
#include <android/native_window_jni.h>
#include <dlfcn.h>

#define LOG_TAG "land-bridge"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, LOG_TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)

// 从 libland-native.so 中动态加载 land_native_set_surface
typedef jboolean (*land_set_surface_fn)(void*, jint, jint);
static land_set_surface_fn land_set_surface = NULL;

// Android Surface → ANativeWindow* → Rust
extern "C" JNIEXPORT void JNICALL
Java_com_land_MainActivity_nativeSetSurface(
    JNIEnv* env, jclass cls, jobject surface, jint width, jint height) {

    if (!land_set_surface) {
        land_set_surface = (land_set_surface_fn)dlsym(RTLD_DEFAULT, "land_native_set_surface");
        if (!land_set_surface) {
            LOGE("land_native_set_surface not found in loaded libraries");
            return;
        }
    }

    ANativeWindow* window = ANativeWindow_fromSurface(env, surface);
    if (window == nullptr) {
        LOGE("ANativeWindow_fromSurface failed");
        return;
    }
    land_set_surface((void*)window, width, height);
    ANativeWindow_release(window);
}

extern "C" jint JNI_OnLoad(JavaVM* vm, void* reserved) {
    LOGI("land-bridge loaded");
    return JNI_VERSION_1_6;
}
