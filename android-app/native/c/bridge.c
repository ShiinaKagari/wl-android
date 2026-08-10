#include <android/native_window_jni.h>
#include <android/log.h>
#include <dlfcn.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>

/* NDK 27's native_window.h does not define WINDOW_FORMAT_BGRA_8888.
 * Value 5 matches android.graphics.PixelFormat.BGRA_8888. */
#ifndef WINDOW_FORMAT_BGRA_8888
#define WINDOW_FORMAT_BGRA_8888 5
#endif

/* AHB-PROBE: verify whether an external (KWin-allocated) dmabuf fd can be
 * imported into an AHardwareBuffer via the gralloc path. AHardwareBuffer_
 * createFromHandle is a VNDK symbol not exposed by the NDK headers, so it is
 * resolved dynamically (same pattern as ANativeWindow_setBufferCount below).
 * The native_handle wraps the dmabuf fd exactly like a buffer_handle_t. */

/* Minimal native_handle_t layout (cutils/native_handle.h) — we only need the
 * fixed header + one fd, no fds/ints payload. */
typedef struct {
    int version;
    int numFds;
    int numInts;
    int reserved;
    int data[1]; /* fd[0] */
} probe_native_handle;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t layers;
    uint32_t format;
    uint64_t usage;
    uint32_t stride;
    uint32_t rfu0;
    uint64_t rfu1;
} probe_ahb_desc;

typedef struct probe_ahb { int unused; } probe_ahb;

typedef int (*create_from_handle_fn)(const probe_ahb_desc*, const void*, int, probe_ahb**);
typedef int (*lock_fn)(probe_ahb*, uint64_t, int, void*, const void**);
typedef int (*unlock_fn)(probe_ahb*, void**);
typedef void (*release_fn)(probe_ahb*);
typedef void (*get_desc_fn)(const probe_ahb*, probe_ahb_desc*);

/* Returns: 0 = createFromHandle absent, 1 = import OK, -errno = import failed */
int wl_probe_ahardwarebuffer_import(int dmabuf_fd, int width, int height, int stride,
                                    void* pixel_probe_out /* 16 bytes */) {
    /* RTLD_DEFAULT does not see VNDK-private symbols from an app namespace;
     * dlopen the library explicitly so dlsym resolves createFromHandle. */
    void* lib = dlopen("libnativewindow.so", RTLD_NOW);
    if (lib == NULL) {
        lib = dlopen("/system/lib64/libnativewindow.so", RTLD_NOW);
    }
    if (lib == NULL) {
        return 0; /* library not loadable */
    }
    create_from_handle_fn create = (create_from_handle_fn)dlsym(lib, "AHardwareBuffer_createFromHandle");
    if (create == NULL) {
        dlclose(lib);
        return 0; /* symbol not present */
    }
    lock_fn do_lock = (lock_fn)dlsym(lib, "AHardwareBuffer_lock");
    unlock_fn do_unlock = (unlock_fn)dlsym(lib, "AHardwareBuffer_unlock");
    release_fn do_release = (release_fn)dlsym(lib, "AHardwareBuffer_release");
    if (do_lock == NULL || do_release == NULL) {
        dlclose(lib);
        return 0;
    }

    /* X8R8G8B8_UNORM = 1 (AHARDWAREBUFFER_FORMAT_R8G8B8X8_UNORM), matching
     * KWin's XRGB8888. usage: GPU sampled image + CPU read for verification. */
    probe_ahb_desc desc;
    memset(&desc, 0, sizeof(desc));
    desc.width = (uint32_t)width;
    desc.height = (uint32_t)height;
    desc.layers = 1;
    /* KWin XRGB8888 (memory B,G,R,X) maps to AHARDWAREBUFFER_FORMAT_R8G8B8X8_UNORM = 2. */
    desc.format = 2;
    desc.usage = (1ULL << 10) | (1ULL << 2) | (1ULL << 0); /* GPU_SAMPLED_IMAGE|GPU_COLOR_OUTPUT|CPU_READ_OFTEN */
    desc.stride = (uint32_t)stride;

    probe_native_handle h;
    memset(&h, 0, sizeof(h));
    h.version = 1;
    h.numFds = 1;
    h.numInts = 0;
    h.data[0] = dmabuf_fd;

    /* Cross-check: is the format/size combination even supported by this
     * gralloc? Separates "format unsupported" from "import of external
     * buffer rejected". AHardwareBuffer_isSupported is a public NDK API. */
    int (*is_supported_fn)(const probe_ahb_desc*) =
        (int (*)(const probe_ahb_desc*))dlsym(lib, "AHardwareBuffer_isSupported");
    if (is_supported_fn != NULL) {
        int sup = is_supported_fn(&desc);
        __android_log_print(ANDROID_LOG_INFO, "AHB-PROBE", "isSupported(fmt=%u,%dx%d)=%d",
                            desc.format, desc.width, desc.height, sup);
    }

    probe_ahb* buf = NULL;
    int rc = create(&desc, &h, 2 /* AHARDWAREBUFFER_CREATE_FROM_HANDLE_METHOD_REGISTER */, &buf);
    if (rc != 0) {
        dlclose(lib);
        return -rc;
    }
    if (buf == NULL) {
        return -EINVAL;
    }

    /* Lock and read the first 16 bytes to prove the pixels are reachable. */
    if (pixel_probe_out != NULL) {
        const void* vaddr = NULL;
        int lrc = do_lock(buf, desc.usage, -1, NULL, &vaddr);
        if (lrc == 0 && vaddr != NULL) {
            memcpy(pixel_probe_out, vaddr, 16);
            void* unused = NULL;
            do_unlock(buf, &unused);
        }
    }
    do_release(buf);
    return 1;
}

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
