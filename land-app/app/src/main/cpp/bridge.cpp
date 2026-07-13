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
static void* land_native_handle = NULL;

static void ensure_land_native_loaded() {
    if (land_set_surface) return;
    // 确保 libland-native.so 的符号在全局命名空间可查
    if (!land_native_handle) {
        land_native_handle = dlopen("libland-native.so", RTLD_NOW | RTLD_GLOBAL);
        if (!land_native_handle) {
            LOGE("dlopen libland-native.so failed: %s", dlerror());
            return;
        }
    }
    land_set_surface = (land_set_surface_fn)dlsym(land_native_handle, "land_native_set_surface");
    if (!land_set_surface)
        LOGE("land_native_set_surface not found: %s", dlerror());
}

// Android Surface → ANativeWindow* → Rust
extern "C" JNIEXPORT void JNICALL
Java_com_land_MainActivity_nativeSetSurface(
    JNIEnv* env, jclass cls, jobject surface, jint width, jint height) {

    ensure_land_native_loaded();
    if (!land_set_surface) return;

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
