/* EGL/GLES2 upload benchmark — device-only diagnostic.
 *
 * Answers: is GPU blit (glTexImage2D upload + draw) faster than the current
 * CPU path (memcpy into ANativeWindow)? Measures three costs on a 32MB frame:
 *   1. memcpy (CPU path baseline, current render_frame copy)
 *   2. glTexImage2D upload (CPU -> GPU texture, the blit path's dominant cost)
 *   3. upload + swap (full EGL present)
 *
 * All EGL/GLES symbols are resolved via dlopen("libEGL.so") + dlsym so the
 * probe builds against the NDK without a hard EGL dependency. EGL/GLES
 * constants are defined inline (no EGL/egl.h include — zero link-time
 * dependency on the EGL headers).
 */

#include <android/log.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, "EGL-BENCH", __VA_ARGS__)

/* EGL constants (from EGL/egl.h) */
#define EGL_DEFAULT_DISPLAY ((void*)0)
#define EGL_NO_DISPLAY ((void*)0)
#define EGL_NO_CONTEXT ((void*)0)
#define EGL_NO_SURFACE ((void*)0)
#define EGL_OPENGL_ES2_BIT 0x0004
#define EGL_WINDOW_BIT 0x0004
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_SURFACE_TYPE 0x3033
#define EGL_RED_SIZE 0x3024
#define EGL_GREEN_SIZE 0x3023
#define EGL_BLUE_SIZE 0x3022
#define EGL_ALPHA_SIZE 0x3021
#define EGL_NONE 0x3038
#define EGL_CONTEXT_CLIENT_VERSION 0x3098

/* GLES2 constants (from GLES2/gl2.h) */
#define GL_TEXTURE_2D 0x0DE1
#define GL_RGBA 0x1908
#define GL_UNSIGNED_BYTE 0x1401
#define GL_TEXTURE_MIN_FILTER 0x2801
#define GL_TEXTURE_MAG_FILTER 0x2800
#define GL_NEAREST 0x2600

/* --- dynamic function pointer types --- */
typedef void* (*eglGetDisplay_t)(void*);
typedef int (*eglInitialize_t)(void*, uint32_t*, uint32_t*);
typedef int (*eglChooseConfig_t)(const void*, const int32_t*, void*, int32_t, int32_t*, int32_t*);
typedef void* (*eglCreateContext_t)(void*, const void*, void*, const int32_t*);
typedef void* (*eglCreateWindowSurface_t)(void*, const void*, void*, const int32_t*);
typedef int (*eglMakeCurrent_t)(void*, void*, void*, void*);
typedef int (*eglSwapBuffers_t)(void*, void*);
typedef void (*glGenTextures_t)(int32_t, uint32_t*);
typedef void (*glBindTexture_t)(uint32_t, uint32_t);
typedef void (*glTexImage2D_t)(uint32_t, int32_t, int32_t, int32_t, int32_t, int32_t,
                               uint32_t, uint32_t, const void*);
typedef void (*glTexParameteri_t)(uint32_t, uint32_t, int32_t);
typedef void (*glReadPixels_t)(int32_t, int32_t, int32_t, int32_t, uint32_t, uint32_t, void*);

typedef struct {
    void* egl_display;
    void* egl_surface;
    void* egl_context;
    uint32_t tex;
    glTexImage2D_t glTexImage2D;
    glReadPixels_t glReadPixels;
    eglSwapBuffers_t eglSwapBuffers;
} bench_egl;

static bench_egl g_bench;

/* dlopen libEGL.so + libGLESv2.so and resolve everything. */
static int bench_egl_init(void* anativewindow) {
    void* egl_lib = dlopen("libEGL.so", RTLD_NOW);
    void* gles_lib = dlopen("libGLESv2.so", RTLD_NOW);
    if (egl_lib == NULL || gles_lib == NULL) {
        LOGI("dlopen EGL/GLES failed: %s", dlerror());
        return -1;
    }
    eglGetDisplay_t eglGetDisplay = (eglGetDisplay_t)dlsym(egl_lib, "eglGetDisplay");
    eglInitialize_t eglInitialize = (eglInitialize_t)dlsym(egl_lib, "eglInitialize");
    eglChooseConfig_t eglChooseConfig = (eglChooseConfig_t)dlsym(egl_lib, "eglChooseConfig");
    eglCreateContext_t eglCreateContext = (eglCreateContext_t)dlsym(egl_lib, "eglCreateContext");
    eglCreateWindowSurface_t eglCreateWindowSurface =
        (eglCreateWindowSurface_t)dlsym(egl_lib, "eglCreateWindowSurface");
    eglMakeCurrent_t eglMakeCurrent = (eglMakeCurrent_t)dlsym(egl_lib, "eglMakeCurrent");
    g_bench.eglSwapBuffers = (eglSwapBuffers_t)dlsym(egl_lib, "eglSwapBuffers");
    glGenTextures_t glGenTextures = (glGenTextures_t)dlsym(gles_lib, "glGenTextures");
    glBindTexture_t glBindTexture = (glBindTexture_t)dlsym(gles_lib, "glBindTexture");
    g_bench.glTexImage2D = (glTexImage2D_t)dlsym(gles_lib, "glTexImage2D");
    glTexParameteri_t glTexParameteri = (glTexParameteri_t)dlsym(gles_lib, "glTexParameteri");
    g_bench.glReadPixels = (glReadPixels_t)dlsym(gles_lib, "glReadPixels");

    if (!eglGetDisplay || !eglInitialize || !eglChooseConfig || !eglCreateContext ||
        !eglCreateWindowSurface || !eglMakeCurrent || !g_bench.eglSwapBuffers ||
        !g_bench.glTexImage2D || !glGenTextures || !glBindTexture || !glTexParameteri) {
        LOGI("dlsym failed: %s", dlerror());
        return -1;
    }

    g_bench.egl_display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (g_bench.egl_display == EGL_NO_DISPLAY) return -1;
    uint32_t maj, min;
    if (!eglInitialize(g_bench.egl_display, &maj, &min)) return -1;

    const int32_t config_attr[] = {
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE
    };
    void* config = NULL;
    int32_t n = 0;
    if (!eglChooseConfig(g_bench.egl_display, config_attr, &config, 1, &n, NULL) || n == 0) {
        LOGI("eglChooseConfig failed");
        return -1;
    }
    const int32_t ctx_attr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    g_bench.egl_context = eglCreateContext(g_bench.egl_display, config, EGL_NO_CONTEXT, ctx_attr);
    g_bench.egl_surface = eglCreateWindowSurface(g_bench.egl_display, config, anativewindow, NULL);
    if (g_bench.egl_context == EGL_NO_CONTEXT || g_bench.egl_surface == EGL_NO_SURFACE) {
        LOGI("egl context/surface failed");
        return -1;
    }
    if (!eglMakeCurrent(g_bench.egl_display, g_bench.egl_surface, g_bench.egl_surface,
                        g_bench.egl_context)) {
        LOGI("eglMakeCurrent failed");
        return -1;
    }
    glGenTextures(1, &g_bench.tex);
    glBindTexture(GL_TEXTURE_2D, g_bench.tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    LOGI("EGL initialized");
    return 0;
}

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

/* Bench 1: plain memcpy of `size` bytes. Returns ms. */
static double bench_memcpy(const void* src, void* dst, size_t size) {
    double t0 = now_ms();
    memcpy(dst, src, size);
    return now_ms() - t0;
}

/* Bench 2: glTexImage2D upload of a full frame (CPU -> GPU texture). The
 * glReadPixels afterwards forces the transfer to complete (GL is pipelined;
 * a swap alone would measure submission, not bandwidth). */
static double bench_upload(const void* src, int w, int h) {
    uint32_t pix = 0;
    double t0 = now_ms();
    g_bench.glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, src);
    if (g_bench.glReadPixels) g_bench.glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, &pix);
    return now_ms() - t0;
}

/* Bench 3: upload + eglSwapBuffers (full frame present, includes vsync). */
static double bench_swap(const void* src, int w, int h) {
    double t0 = now_ms();
    g_bench.glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, src);
    g_bench.eglSwapBuffers(g_bench.egl_display, g_bench.egl_surface);
    return now_ms() - t0;
}

/* ── PBO async upload benchmark (GLES3) ───────────────────────────────
 * Measures whether a PBO-mediated upload lets the CPU hand the frame to the
 * GPU and return immediately (pipelining), vs the synchronous 9ms
 * glTexImage2D. Sequence per iteration:
 *   1. map the PBO (or buffer-data it), copy 33MB in
 *   2. glTexImage2D from the PBO (GPU-internal copy)
 *   3. fence, then wait — the wait time is what a real pipeline would hide
 *      by having the NEXT frame ready while the GPU still transfers.
 * Both the "submit" time (CPU side, pipelineable) and "sync wait" (the
 * true per-frame cost if the pipeline stalls) are reported.
 */

/* GLES3 constants */
#define GL_PIXEL_UNPACK_BUFFER 0x88EC
#define GL_STREAM_DRAW 0x88E0
#define GL_DYNAMIC_DRAW 0x88E8
#define GL_MAP_WRITE_BIT 0x0001
#define GL_SYNC_GPU_COMMANDS_COMPLETE 0x9117
#define GL_ALREADY_SIGNALED 0x911A
#define GL_CONDITION_SATISFIED 0x911C
#define GL_TIMEOUT_IGNORED 0xFFFFFFFFFFFFFFFFull

typedef void (*glGenBuffers_t)(int32_t, uint32_t*);
typedef void (*glBindBuffer_t)(uint32_t, uint32_t);
typedef void (*glBufferData_t)(uint32_t, int32_t, const void*, uint32_t);
typedef void (*glBufferSubData_t)(uint32_t, int32_t, int32_t, const void*);
typedef void* (*glMapBufferRange_t)(uint32_t, int32_t, int32_t, uint32_t);
typedef int (*glUnmapBuffer_t)(uint32_t);
typedef void* (*glFenceSync_t)(uint32_t, uint32_t);
typedef int (*glClientWaitSync_t)(void*, uint32_t, uint64_t);
typedef void (*glDeleteSync_t)(void*);

static glGenBuffers_t fn_glGenBuffers;
static glBindBuffer_t fn_glBindBuffer;
static glBufferData_t fn_glBufferData;
static glBufferSubData_t fn_glBufferSubData;
static glMapBufferRange_t fn_glMapBufferRange;
static glUnmapBuffer_t fn_glUnmapBuffer;
static glFenceSync_t fn_glFenceSync;
static glClientWaitSync_t fn_glClientWaitSync;
static glDeleteSync_t fn_glDeleteSync;
static uint32_t pbo = 0;
static int pbo_ready = 0;

static int resolve_pbo(void) {
    if (pbo_ready) return pbo_ready > 0 ? 0 : -1;
    void* gles_lib = dlopen("libGLESv2.so", RTLD_NOW);
    if (gles_lib == NULL) { pbo_ready = -1; return -1; }
    fn_glGenBuffers = (glGenBuffers_t)dlsym(gles_lib, "glGenBuffers");
    fn_glBindBuffer = (glBindBuffer_t)dlsym(gles_lib, "glBindBuffer");
    fn_glBufferData = (glBufferData_t)dlsym(gles_lib, "glBufferData");
    fn_glBufferSubData = (glBufferSubData_t)dlsym(gles_lib, "glBufferSubData");
    fn_glMapBufferRange = (glMapBufferRange_t)dlsym(gles_lib, "glMapBufferRange");
    fn_glUnmapBuffer = (glUnmapBuffer_t)dlsym(gles_lib, "glUnmapBuffer");
    fn_glFenceSync = (glFenceSync_t)dlsym(gles_lib, "glFenceSync");
    fn_glClientWaitSync = (glClientWaitSync_t)dlsym(gles_lib, "glClientWaitSync");
    fn_glDeleteSync = (glDeleteSync_t)dlsym(gles_lib, "glDeleteSync");
    if (!fn_glGenBuffers || !fn_glBindBuffer || !fn_glBufferData || !fn_glBufferSubData ||
        !fn_glMapBufferRange || !fn_glUnmapBuffer || !fn_glFenceSync || !fn_glClientWaitSync) {
        pbo_ready = -1;
        LOGI("PBO dlsym failed — GLES3 PBO not available");
        return -1;
    }
    fn_glGenBuffers(1, &pbo);
    fn_glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
    pbo_ready = 1;
    return 0;
}

/* Returns (submit_ms, wait_ms) — submit is pipelineable CPU time, wait is the
 * sync cost that a stall would expose. */
static void bench_pbo(const void* src, int w, int h, double* submit_ms, double* wait_ms) {
    int size = w * h * 4;
    /* Map-write the PBO (double-buffered via orphaning: re-alloc each frame). */
    double t0 = now_ms();
    fn_glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
    fn_glBufferData(GL_PIXEL_UNPACK_BUFFER, size, NULL, GL_STREAM_DRAW); /* orphan */
    void* mapped = fn_glMapBufferRange(GL_PIXEL_UNPACK_BUFFER, 0, size, GL_MAP_WRITE_BIT);
    memcpy(mapped, src, size);
    fn_glUnmapBuffer(GL_PIXEL_UNPACK_BUFFER);
    g_bench.glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    double t1 = now_ms();
    /* Fence + wait: this is the stall a real pipeline would hide. */
    void* sync = fn_glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    fn_glClientWaitSync(sync, 0, GL_TIMEOUT_IGNORED);
    fn_glDeleteSync(sync);
    double t2 = now_ms();
    *submit_ms = t1 - t0;
    *wait_ms = t2 - t1;
}

/* Called from Rust with a real frame buffer + the ANativeWindow. */
int wl_bench_egl(void* anativewindow, const void* pixels, int w, int h) {
    static int inited = 0;
    if (!inited) {
        if (bench_egl_init(anativewindow) != 0) {
            LOGI("EGL init failed — GPU blit not measurable");
            return -1;
        }
        inited = 1;
    }
    const size_t size = (size_t)w * h * 4;
    static uint8_t* dst = NULL;
    if (dst == NULL) dst = malloc(size);
    if (!dst) return -1;

    double mc = bench_memcpy(pixels, dst, size);
    double up = bench_upload(pixels, w, h);
    double sw = bench_swap(pixels, w, h);
    double mb = size / 1048576.0;
    LOGI("frame %ux%u (%.0fMB): memcpy=%.2fms (%.1fGB/s) upload=%.2fms (%.1fGB/s) swap=%.2fms",
         w, h, mb, mc, mb / mc, up, mb / up, sw);

    /* PBO async path — only if GLES3 symbols resolve. */
    double submit_ms = 0, wait_ms = 0;
    if (resolve_pbo() == 0) {
        /* Warm up once (first buffer-data may lazily allocate). */
        bench_pbo(pixels, w, h, &submit_ms, &wait_ms);
        bench_pbo(pixels, w, h, &submit_ms, &wait_ms);
        LOGI("PBO: submit=%.2fms (pipelineable CPU) wait=%.2fms (sync stall) total=%.2fms",
             submit_ms, wait_ms, submit_ms + wait_ms);
    }
    return 0;
}
