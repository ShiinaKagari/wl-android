/* GPU blit renderer — PBO-pipelined fullscreen texture blit to ANativeWindow.
 *
 * Replaces the CPU path (ANativeWindow_lock + memcpy) with:
 *   SnapshotPool pixels → PBO map+memcpy (CPU ~1.6ms) → glTexImage2D from
 *   PBO → fullscreen quad → eglSwapBuffers.
 *
 * Pipelining: two PBOs alternate per frame. Submitting frame N+1 into PBO[N%2]
 * waits (glClientWaitSync) only until the GPU finished reading PBO[N%2]'s
 * previous contents — while the GPU transfers the 33MB in the background the
 * CPU already returned. Benchmarked: submit+wait ≈ 2.9ms vs 11.7ms memcpy.
 *
 * All EGL/GLES symbols resolved via dlopen/dlsym (no link-time EGL deps).
 * EGL constants inlined (no EGL headers pulled in).
 */

#include <android/log.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, "GPU-BLIT", __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, "GPU-BLIT", __VA_ARGS__)

/* EGL constants */
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

/* GLES2/3 constants */
#define GL_TEXTURE_2D 0x0DE1
#define GL_RGBA 0x1908
#define GL_UNSIGNED_BYTE 0x1401
#define GL_TEXTURE_MIN_FILTER 0x2801
#define GL_TEXTURE_MAG_FILTER 0x2800
#define GL_NEAREST 0x2600
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_VERTEX_SHADER 0x8B31
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_ARRAY_BUFFER 0x8892
#define GL_STATIC_DRAW 0x88E4
#define GL_STREAM_DRAW 0x88E0
#define GL_PIXEL_UNPACK_BUFFER 0x88EC
#define GL_MAP_WRITE_BIT 0x0001
#define GL_SYNC_GPU_COMMANDS_COMPLETE 0x9117
#define GL_TIMEOUT_IGNORED 0xFFFFFFFFFFFFFFFFull
#define GL_FALSE 0
#define GL_TRIANGLE_STRIP 0x0005
#define GL_FLOAT 0x1406
#define GL_COLOR_BUFFER_BIT 0x00004000
#define GL_TEXTURE0 0x84C0
#define GL_ACTIVE_TEXTURE 0x84E0
#define GL_BLEND 0x0BE2
#define GL_ONE 1
#define GL_ONE_MINUS_SRC_ALPHA 0x0303
#define GL_SRC_ALPHA 0x0302

/* --- EGL function pointers --- */
typedef void* (*eglGetDisplay_t)(void*);
typedef int (*eglInitialize_t)(void*, uint32_t*, uint32_t*);
typedef int (*eglChooseConfig_t)(const void*, const int32_t*, void*, int32_t, int32_t*, int32_t*);
typedef void* (*eglCreateContext_t)(void*, const void*, void*, const int32_t*);
typedef void* (*eglCreateWindowSurface_t)(void*, const void*, void*, const int32_t*);
typedef int (*eglMakeCurrent_t)(void*, void*, void*, void*);
typedef int (*eglSwapBuffers_t)(void*, void*);
typedef int (*eglSwapInterval_t)(void*, int32_t);
typedef void (*eglDestroySurface_t)(void*, void*);

/* --- GLES2 function pointers --- */
typedef uint32_t (*glCreateShader_t)(uint32_t);
typedef void (*glShaderSource_t)(uint32_t, int32_t, const char* const*, const int32_t*);
typedef void (*glCompileShader_t)(uint32_t);
typedef void (*glGetShaderiv_t)(uint32_t, uint32_t, int32_t*);
typedef void (*glGetShaderInfoLog_t)(uint32_t, int32_t, int32_t*, char*);
typedef uint32_t (*glCreateProgram_t)(void);
typedef void (*glAttachShader_t)(uint32_t, uint32_t);
typedef void (*glLinkProgram_t)(uint32_t);
typedef void (*glGetProgramiv_t)(uint32_t, uint32_t, int32_t*);
typedef void (*glUseProgram_t)(uint32_t);
typedef int32_t (*glGetUniformLocation_t)(uint32_t, const char*);
typedef void (*glUniform1i_t)(int32_t, int32_t);
typedef void (*glGenBuffers_t)(int32_t, uint32_t*);
typedef void (*glBindBuffer_t)(uint32_t, uint32_t);
typedef void (*glBufferData_t)(uint32_t, int32_t, const void*, uint32_t);
typedef void (*glGenTextures_t)(int32_t, uint32_t*);
typedef void (*glBindTexture_t)(uint32_t, uint32_t);
typedef void (*glTexImage2D_t)(uint32_t, int32_t, int32_t, int32_t, int32_t, int32_t,
                               uint32_t, uint32_t, const void*);
typedef void (*glTexParameteri_t)(uint32_t, uint32_t, int32_t);
typedef void (*glViewport_t)(int32_t, int32_t, int32_t, int32_t);
typedef void (*glClearColor_t)(float, float, float, float);
typedef void (*glClear_t)(uint32_t);
typedef void (*glDrawArrays_t)(uint32_t, int32_t, int32_t);
typedef void (*glEnableVertexAttribArray_t)(uint32_t);
typedef void (*glDisableVertexAttribArray_t)(uint32_t);
typedef void (*glVertexAttribPointer_t)(uint32_t, int32_t, uint32_t, uint8_t, int32_t, const void*);
typedef void (*glActiveTexture_t)(uint32_t);
typedef void* (*glMapBufferRange_t)(uint32_t, int32_t, int32_t, uint32_t);
typedef int (*glUnmapBuffer_t)(uint32_t);
typedef void* (*glFenceSync_t)(uint32_t, uint32_t);
typedef int (*glClientWaitSync_t)(void*, uint32_t, uint64_t);
typedef void (*glDeleteSync_t)(void*);

typedef struct {
    void* egl_display;
    void* egl_surface;
    void* egl_context;
    uint32_t program;
    uint32_t tex;
    int32_t loc_tex;
    uint32_t vbo;
    uint32_t pbos[2];
    int pbo_idx;
    void* fence;
    int width;
    int height;
    int frame_size;
    int ready;

    /* resolved fn pointers */
    eglSwapBuffers_t eglSwapBuffers;
    glUseProgram_t glUseProgram;
    glUniform1i_t glUniform1i;
    glActiveTexture_t glActiveTexture;
    glBindTexture_t glBindTexture;
    glTexImage2D_t glTexImage2D;
    glViewport_t glViewport;
    glClear_t glClear;
    glClearColor_t glClearColor;
    glDrawArrays_t glDrawArrays;
    glBindBuffer_t glBindBuffer;
    glBufferData_t glBufferData;
    glMapBufferRange_t glMapBufferRange;
    glUnmapBuffer_t glUnmapBuffer;
    glFenceSync_t glFenceSync;
    glClientWaitSync_t glClientWaitSync;
    glDeleteSync_t glDeleteSync;
    glEnableVertexAttribArray_t glEnableVertexAttribArray;
    glVertexAttribPointer_t glVertexAttribPointer;
} gpu_blit;

static gpu_blit g_blit;

/* ── shaders ────────────────────────────────────────────────────────── */

static const char* VERT_SRC =
    "attribute vec2 a_pos;\n"
    "varying vec2 v_uv;\n"
    "void main() {\n"
    "  v_uv = vec2(a_pos.x * 0.5 + 0.5, 1.0 - (a_pos.y * 0.5 + 0.5));\n"
    "  gl_Position = vec4(a_pos, 0.0, 1.0);\n"
    "}\n";

static const char* FRAG_SRC =
    "precision mediump float;\n"
    "varying vec2 v_uv;\n"
    "uniform sampler2D u_tex;\n"
    "void main() {\n"
    "  gl_FragColor = texture2D(u_tex, v_uv);\n"
    "}\n";

/* ── helpers ────────────────────────────────────────────────────────── */

static void* dl(void* lib, const char* sym) {
    void* p = dlsym(lib, sym);
    return p;
}

static uint32_t compile_shader(uint32_t type, const char* src,
                               glCreateShader_t glCreateShader,
                               glShaderSource_t glShaderSource,
                               glCompileShader_t glCompileShader,
                               glGetShaderiv_t glGetShaderiv,
                               glGetShaderInfoLog_t glGetShaderInfoLog) {
    uint32_t s = glCreateShader(type);
    if (!s) return 0;
    const char* srcs[] = { src };
    glShaderSource(s, 1, srcs, NULL);
    glCompileShader(s);
    int32_t ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[512];
        glGetShaderInfoLog(s, sizeof(log), NULL, log);
        LOGE("shader compile failed: %s", log);
        return 0;
    }
    return s;
}

static uint32_t build_program(glCreateShader_t cS, glShaderSource_t sS,
                              glCompileShader_t cC, glGetShaderiv_t gI,
                              glGetShaderInfoLog_t gL,
                              glCreateProgram_t cP, glAttachShader_t aS,
                              glLinkProgram_t lP, glGetProgramiv_t gP) {
    uint32_t vs = compile_shader(GL_VERTEX_SHADER, VERT_SRC, cS, sS, cC, gI, gL);
    uint32_t fs = compile_shader(GL_FRAGMENT_SHADER, FRAG_SRC, cS, sS, cC, gI, gL);
    if (!vs || !fs) return 0;
    uint32_t prog = cP();
    aS(prog, vs);
    aS(prog, fs);
    lP(prog);
    int32_t ok = 0;
    gP(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        LOGE("program link failed");
        return 0;
    }
    return prog;
}

/* ── public API ─────────────────────────────────────────────────────── */

/* Initialize EGL + GLES for the given ANativeWindow. Returns 0 on success. */
int wl_gpu_init(void* anativewindow) {
    void* egl_lib = dlopen("libEGL.so", RTLD_NOW);
    void* gles_lib = dlopen("libGLESv2.so", RTLD_NOW);
    if (!egl_lib || !gles_lib) { LOGE("dlopen EGL failed"); return -1; }

    eglGetDisplay_t eglGetDisplay = (eglGetDisplay_t)dl(egl_lib, "eglGetDisplay");
    eglInitialize_t eglInitialize = (eglInitialize_t)dl(egl_lib, "eglInitialize");
    eglChooseConfig_t eglChooseConfig = (eglChooseConfig_t)dl(egl_lib, "eglChooseConfig");
    eglCreateContext_t eglCreateContext = (eglCreateContext_t)dl(egl_lib, "eglCreateContext");
    eglCreateWindowSurface_t eglCreateWindowSurface =
        (eglCreateWindowSurface_t)dl(egl_lib, "eglCreateWindowSurface");
    eglMakeCurrent_t eglMakeCurrent = (eglMakeCurrent_t)dl(egl_lib, "eglMakeCurrent");
    g_blit.eglSwapBuffers = (eglSwapBuffers_t)dl(egl_lib, "eglSwapBuffers");
    eglSwapInterval_t eglSwapInterval = (eglSwapInterval_t)dl(egl_lib, "eglSwapInterval");

    glCreateShader_t glCreateShader = (glCreateShader_t)dl(gles_lib, "glCreateShader");
    glShaderSource_t glShaderSource = (glShaderSource_t)dl(gles_lib, "glShaderSource");
    glCompileShader_t glCompileShader = (glCompileShader_t)dl(gles_lib, "glCompileShader");
    glGetShaderiv_t glGetShaderiv = (glGetShaderiv_t)dl(gles_lib, "glGetShaderiv");
    glGetShaderInfoLog_t glGetShaderInfoLog = (glGetShaderInfoLog_t)dl(gles_lib, "glGetShaderInfoLog");
    glCreateProgram_t glCreateProgram = (glCreateProgram_t)dl(gles_lib, "glCreateProgram");
    glAttachShader_t glAttachShader = (glAttachShader_t)dl(gles_lib, "glAttachShader");
    glLinkProgram_t glLinkProgram = (glLinkProgram_t)dl(gles_lib, "glLinkProgram");
    glGetProgramiv_t glGetProgramiv = (glGetProgramiv_t)dl(gles_lib, "glGetProgramiv");
    glUseProgram_t glUseProgram = (glUseProgram_t)dl(gles_lib, "glUseProgram");
    glGetUniformLocation_t glGetUniformLocation = (glGetUniformLocation_t)dl(gles_lib, "glGetUniformLocation");
    glUniform1i_t glUniform1i = (glUniform1i_t)dl(gles_lib, "glUniform1i");
    glActiveTexture_t glActiveTexture = (glActiveTexture_t)dl(gles_lib, "glActiveTexture");
    glGenBuffers_t glGenBuffers = (glGenBuffers_t)dl(gles_lib, "glGenBuffers");
    glBindBuffer_t glBindBuffer = (glBindBuffer_t)dl(gles_lib, "glBindBuffer");
    glBufferData_t glBufferData = (glBufferData_t)dl(gles_lib, "glBufferData");
    glGenTextures_t glGenTextures = (glGenTextures_t)dl(gles_lib, "glGenTextures");
    glBindTexture_t glBindTexture = (glBindTexture_t)dl(gles_lib, "glBindTexture");
    glTexImage2D_t glTexImage2D = (glTexImage2D_t)dl(gles_lib, "glTexImage2D");
    glTexParameteri_t glTexParameteri = (glTexParameteri_t)dl(gles_lib, "glTexParameteri");
    glViewport_t glViewport = (glViewport_t)dl(gles_lib, "glViewport");
    glClearColor_t glClearColor = (glClearColor_t)dl(gles_lib, "glClearColor");
    glClear_t glClear = (glClear_t)dl(gles_lib, "glClear");
    glDrawArrays_t glDrawArrays = (glDrawArrays_t)dl(gles_lib, "glDrawArrays");
    glEnableVertexAttribArray_t glEnableVertexAttribArray = (glEnableVertexAttribArray_t)dl(gles_lib, "glEnableVertexAttribArray");
    glVertexAttribPointer_t glVertexAttribPointer = (glVertexAttribPointer_t)dl(gles_lib, "glVertexAttribPointer");
    glMapBufferRange_t glMapBufferRange = (glMapBufferRange_t)dl(gles_lib, "glMapBufferRange");
    glUnmapBuffer_t glUnmapBuffer = (glUnmapBuffer_t)dl(gles_lib, "glUnmapBuffer");
    glFenceSync_t glFenceSync = (glFenceSync_t)dl(gles_lib, "glFenceSync");
    glClientWaitSync_t glClientWaitSync = (glClientWaitSync_t)dl(gles_lib, "glClientWaitSync");
    glDeleteSync_t glDeleteSync = (glDeleteSync_t)dl(gles_lib, "glDeleteSync");

    if (!eglGetDisplay || !eglInitialize || !eglChooseConfig || !eglCreateContext ||
        !eglCreateWindowSurface || !eglMakeCurrent || !g_blit.eglSwapBuffers ||
        !glCreateShader || !glShaderSource || !glCompileShader || !glGetShaderiv ||
        !glCreateProgram || !glAttachShader || !glLinkProgram || !glGetProgramiv ||
        !glUseProgram || !glGetUniformLocation || !glUniform1i || !glActiveTexture ||
        !glGenBuffers || !glBindBuffer || !glBufferData || !glGenTextures || !glBindTexture ||
        !glTexImage2D || !glTexParameteri || !glViewport || !glClearColor || !glClear ||
        !glDrawArrays || !glEnableVertexAttribArray || !glVertexAttribPointer ||
        !glMapBufferRange || !glUnmapBuffer || !glFenceSync || !glClientWaitSync ||
        !glDeleteSync) {
        LOGE("dlsym failed: %s", dlerror());
        return -1;
    }

    g_blit.egl_display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (g_blit.egl_display == EGL_NO_DISPLAY) return -1;
    uint32_t maj, min;
    if (!eglInitialize(g_blit.egl_display, &maj, &min)) return -1;

    const int32_t config_attr[] = {
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE
    };
    void* config = NULL;
    int32_t n = 0;
    if (!eglChooseConfig(g_blit.egl_display, config_attr, &config, 1, &n, NULL) || n == 0) {
        LOGE("eglChooseConfig failed");
        return -1;
    }
    const int32_t ctx_attr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    g_blit.egl_context = eglCreateContext(g_blit.egl_display, config, EGL_NO_CONTEXT, ctx_attr);
    g_blit.egl_surface = eglCreateWindowSurface(g_blit.egl_display, config, anativewindow, NULL);
    if (g_blit.egl_context == EGL_NO_CONTEXT || g_blit.egl_surface == EGL_NO_SURFACE) {
        LOGE("egl context/surface failed");
        return -1;
    }
    if (!eglMakeCurrent(g_blit.egl_display, g_blit.egl_surface, g_blit.egl_surface,
                        g_blit.egl_context)) {
        LOGE("eglMakeCurrent failed");
        return -1;
    }
    if (eglSwapInterval) eglSwapInterval(g_blit.egl_display, 0); /* no vsync block */

    /* program + quad */
    g_blit.program = build_program(glCreateShader, glShaderSource, glCompileShader,
                                   glGetShaderiv, glGetShaderInfoLog,
                                   glCreateProgram, glAttachShader, glLinkProgram,
                                   glGetProgramiv);
    if (!g_blit.program) return -1;
    g_blit.loc_tex = glGetUniformLocation(g_blit.program, "u_tex");

    static const float quad[] = {
        -1.0f, -1.0f,
         1.0f, -1.0f,
        -1.0f,  1.0f,
         1.0f,  1.0f,
    };
    glGenBuffers(1, &g_blit.vbo);
    glBindBuffer(GL_ARRAY_BUFFER, g_blit.vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);

    glGenTextures(1, &g_blit.tex);
    glBindTexture(GL_TEXTURE_2D, g_blit.tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    /* double PBO */
    glGenBuffers(2, g_blit.pbos);
    g_blit.pbo_idx = 0;
    g_blit.fence = NULL;
    g_blit.ready = 0;

    g_blit.glUseProgram = glUseProgram;
    g_blit.glUniform1i = glUniform1i;
    g_blit.glActiveTexture = glActiveTexture;
    g_blit.glBindTexture = glBindTexture;
    g_blit.glTexImage2D = glTexImage2D;
    g_blit.glViewport = glViewport;
    g_blit.glClear = glClear;
    g_blit.glClearColor = glClearColor;
    g_blit.glDrawArrays = glDrawArrays;
    g_blit.glBindBuffer = glBindBuffer;
    g_blit.glBufferData = glBufferData;
    g_blit.glActiveTexture = glActiveTexture;
    g_blit.glMapBufferRange = glMapBufferRange;
    g_blit.glUnmapBuffer = glUnmapBuffer;
    g_blit.glFenceSync = glFenceSync;
    g_blit.glClientWaitSync = glClientWaitSync;
    g_blit.glDeleteSync = glDeleteSync;
    g_blit.glEnableVertexAttribArray = glEnableVertexAttribArray;
    g_blit.glVertexAttribPointer = glVertexAttribPointer;

    LOGI("GPU blit initialized (EGL + GLES2 + double PBO)");
    return 0;
}

/* Present a full frame via PBO-pipelined blit. Returns 0 on success. */
int wl_gpu_present(const void* pixels, int w, int h) {
    if (!g_blit.ready || g_blit.egl_display == NULL) return -1;
    int size = w * h * 4;
    if (size != g_blit.frame_size) return -1;

    int idx = g_blit.pbo_idx;
    /* Wait for the GPU to finish reading this PBO's previous contents. */
    if (g_blit.fence) {
        g_blit.glClientWaitSync(g_blit.fence, 0, GL_TIMEOUT_IGNORED);
        g_blit.glDeleteSync(g_blit.fence);
        g_blit.fence = NULL;
    }

    /* Upload: map PBO (orphaned), copy pixels, unmap. */
    g_blit.glBindBuffer(GL_PIXEL_UNPACK_BUFFER, g_blit.pbos[idx]);
    g_blit.glBindBuffer(GL_PIXEL_UNPACK_BUFFER, g_blit.pbos[idx]);
    void* mapped = g_blit.glMapBufferRange(GL_PIXEL_UNPACK_BUFFER, 0, size, GL_MAP_WRITE_BIT);
    if (mapped) {
        memcpy(mapped, pixels, size);
        g_blit.glUnmapBuffer(GL_PIXEL_UNPACK_BUFFER);
    }

    /* Texture from PBO, draw fullscreen quad. */
    g_blit.glActiveTexture(GL_TEXTURE0);
    g_blit.glBindTexture(GL_TEXTURE_2D, g_blit.tex);
    g_blit.glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    g_blit.glUseProgram(g_blit.program);
    g_blit.glUniform1i(g_blit.loc_tex, 0);
    g_blit.glBindBuffer(GL_ARRAY_BUFFER, g_blit.vbo);
    g_blit.glEnableVertexAttribArray(0);
    g_blit.glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    g_blit.glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

    /* Fence: marks the PBO transfer complete; next frame waits on it. */
    g_blit.fence = g_blit.glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    g_blit.eglSwapBuffers(g_blit.egl_display, g_blit.egl_surface);

    g_blit.pbo_idx = 1 - idx;
    return 0;
}

/* Resize the texture/PBO allocation. Call on render-size change. */
int wl_gpu_resize(int w, int h) {
    if (!g_blit.ready) return -1;
    g_blit.frame_size = w * h * 4;
    /* Orphan both PBOs at the new size. */
    for (int i = 0; i < 2; i++) {
        g_blit.glBindBuffer(GL_PIXEL_UNPACK_BUFFER, g_blit.pbos[i]);
        g_blit.glBufferData(GL_PIXEL_UNPACK_BUFFER, g_blit.frame_size, NULL, GL_STREAM_DRAW);
    }
    return 0;
}

/* Clear to black (used when the session disconnects). */
int wl_gpu_blank(void) {
    if (!g_blit.ready || g_blit.egl_display == NULL) return -1;
    g_blit.glClearColor(0, 0, 0, 1);
    g_blit.glClear(GL_COLOR_BUFFER_BIT);
    g_blit.eglSwapBuffers(g_blit.egl_display, g_blit.egl_surface);
    return 0;
}

/* Called once the render thread has a window + dimensions. */
int wl_gpu_setup(void* anativewindow, int w, int h) {
    if (g_blit.ready) return 0;
    if (wl_gpu_init(anativewindow) != 0) return -1;
    g_blit.width = w;
    g_blit.height = h;
    g_blit.frame_size = w * h * 4;
    for (int i = 0; i < 2; i++) {
        g_blit.glBindBuffer(GL_PIXEL_UNPACK_BUFFER, g_blit.pbos[i]);
        g_blit.glBufferData(GL_PIXEL_UNPACK_BUFFER, g_blit.frame_size, NULL, GL_STREAM_DRAW);
    }
    g_blit.ready = 1;
    return 0;
}

int wl_gpu_is_ready(void) {
    return g_blit.ready ? 1 : 0;
}
