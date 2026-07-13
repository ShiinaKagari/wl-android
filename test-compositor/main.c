/**
 * wl-android-compositor: 无头嵌套 wlroots 合成器
 *
 * 类似 Gamescope 的嵌套合成器，但不渲染到本地屏幕。
 * 将每个 surface commit 的 DMA-BUF fd 通过 libland_wlroots.so 转发到 Android。
 *
 * 使用方式:
 *   WAYLAND_DISPLAY=wl-android-0 <app>   # 在该 socket 下运行应用
 *
 * 构建:
 *   gcc main.c -o wl-android-compositor \
 *       $(pkg-config --cflags --libs wlroots wayland-server libdrm) \
 *       -ldl -lpthread -lm
 */

#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>

#include <wayland-server-core.h>
#include <wlr/backend.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_output_layout.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_subcompositor.h>
#include <wlr/types/wlr_xdg_shell.h>
#include <wlr/util/log.h>

/* ================================================================
 * land 插件
 * ================================================================ */
static void *(*land_create)(struct wlr_renderer *, struct wl_display *) = NULL;
static void  (*land_destroy)(void *)                                     = NULL;
static bool  (*land_buffer_submit)(void *, struct wlr_buffer *)          = NULL;
static void *land_backend = NULL;

static bool load_land_plugin(void) {
    void *h = dlopen("libland_wlroots.so", RTLD_NOW | RTLD_GLOBAL);
    if (!h) {
        fprintf(stderr, "[land] dlopen failed: %s\n", dlerror());
        return false;
    }
    land_create       = dlsym(h, "land_wlroots_renderer_create");
    land_destroy      = dlsym(h, "land_wlroots_renderer_destroy");
    land_buffer_submit = dlsym(h, "land_wlroots_buffer_submit");
    if (!land_create || !land_destroy || !land_buffer_submit) {
        fprintf(stderr, "[land] missing symbols\n");
        return false;
    }
    fprintf(stderr, "[land] plugin loaded\n");
    return true;
}

/* ================================================================
 * Surface 跟踪
 * ================================================================ */
struct tracked_surface {
    struct wlr_surface *wlr_surface;
    struct wl_listener commit;
    struct wl_listener destroy;
    struct wl_list link;        // compositor.surfaces
};

static void surface_commit_handler(struct wl_listener *listener, void *data) {
    struct tracked_surface *ts = wl_container_of(listener, ts, commit);
    (void)data;

    if (!land_backend || !land_buffer_submit)
        return;

    struct wlr_buffer *buffer = wlr_surface_get_buffer(ts->wlr_surface);
    if (!buffer)
        return;

    bool ok = land_buffer_submit(land_backend, buffer);
    if (!ok)
        fprintf(stderr, "[land] buffer submit failed\n");

    wlr_buffer_unlock(buffer);
}

static void surface_destroy_handler(struct wl_listener *listener, void *data) {
    struct tracked_surface *ts = wl_container_of(listener, ts, destroy);
    (void)data;

    wl_list_remove(&ts->commit.link);
    wl_list_remove(&ts->destroy.link);
    wl_list_remove(&ts->link);
    free(ts);
}

static struct tracked_surface *track_surface(struct wlr_surface *surface) {
    struct tracked_surface *ts = calloc(1, sizeof(*ts));
    if (!ts) return NULL;

    ts->wlr_surface = surface;
    ts->commit.notify = surface_commit_handler;
    ts->destroy.notify = surface_destroy_handler;
    wl_signal_add(&surface->events.commit, &ts->commit);
    wl_signal_add(&surface->events.destroy, &ts->destroy);
    return ts;
}

/* ================================================================
 * xdg-shell: 每个新 surface → 自动跟踪
 * ================================================================ */
static void handle_xdg_surface_map(struct wl_listener *listener, void *data);
static void handle_xdg_surface_unmap(struct wl_listener *listener, void *data);
static void handle_xdg_surface_destroy(struct wl_listener *listener, void *data);

struct xdg_surface_data {
    struct wlr_xdg_surface *xdg_surface;
    struct wl_listener map;
    struct wl_listener unmap;
    struct wl_listener destroy_map;
};

static void handle_new_xdg_surface(struct wl_listener *listener, void *data) {
    struct wlr_xdg_surface *xdg_surface = data;

    if (xdg_surface->role != WLR_XDG_SURFACE_ROLE_TOPLEVEL &&
        xdg_surface->role != WLR_XDG_SURFACE_ROLE_POPUP)
        return;

    struct xdg_surface_data *sd = calloc(1, sizeof(*sd));
    if (!sd) return;
    sd->xdg_surface = xdg_surface;
    sd->map.notify = handle_xdg_surface_map;
    sd->unmap.notify = handle_xdg_surface_unmap;
    sd->destroy_map.notify = handle_xdg_surface_destroy;
    wl_signal_add(&xdg_surface->events.map, &sd->map);
    wl_signal_add(&xdg_surface->events.unmap, &sd->unmap);
    wl_signal_add(&xdg_surface->events.destroy, &sd->destroy_map);
}

static void handle_xdg_surface_map(struct wl_listener *listener, void *data) {
    struct xdg_surface_data *sd = wl_container_of(listener, sd, map);
    (void)data;

    struct wlr_xdg_surface *xdg = sd->xdg_surface;
    struct wlr_surface *surface = xdg->surface;

    fprintf(stderr, "[xdg] map: title=\"%s\" %dx%d\n",
            xdg->toplevel ? xdg->toplevel->title : "(popup)",
            surface->current.width, surface->current.height);

    // 为该 surface 建立 commit 跟踪
    track_surface(surface);
}

static void handle_xdg_surface_unmap(struct wl_listener *listener, void *data) {
    struct xdg_surface_data *sd = wl_container_of(listener, sd, unmap);
    (void)data;
    fprintf(stderr, "[xdg] unmap\n");
}

static void handle_xdg_surface_destroy(struct wl_listener *listener, void *data) {
    struct xdg_surface_data *sd = wl_container_of(listener, sd, destroy_map);
    (void)data;
    wl_list_remove(&sd->map.link);
    wl_list_remove(&sd->unmap.link);
    wl_list_remove(&sd->destroy_map.link);
    free(sd);
}

/* ================================================================
 * 合成器状态
 * ================================================================ */
struct compositor_state {
    struct wl_display *display;
    struct wlr_backend *backend;
    struct wlr_renderer *renderer;
    struct wlr_compositor *compositor;
    struct wlr_xdg_shell *xdg_shell;

    struct wl_listener new_xdg_surface;
    struct wl_list surfaces;     // tracked_surface.link

    struct wl_listener new_output;
};

static struct compositor_state state;
static int running = 1;

static void handle_signal(int signo) {
    running = 0;
}

/* ================================================================
 * Output 创建 (仅日志，无渲染)
 * ================================================================ */
static void handle_new_output(struct wl_listener *listener, void *data) {
    struct wlr_output *output = data;
    fprintf(stderr, "[output] %s (headless)\n", output->name);
    wlr_output_enable(output, true);
    wlr_output_set_custom_mode(output, 1920, 1080, 0); // 虚拟分辨率
    wlr_output_commit(output);
}

/* ================================================================
 * 初始化
 * ================================================================ */
static bool init_compositor(void) {
    state.display = wl_display_create();
    if (!state.display) {
        fprintf(stderr, "wl_display_create failed\n");
        return false;
    }

    // 无头后端 (headless) — 不连接任何物理输出或嵌套 Wayland socket
    state.backend = wlr_headless_backend_create(state.display);
    if (!state.backend) {
        fprintf(stderr, "wlr_headless_backend_create failed\n");
        return false;
    }

    // 渲染器 — 仅 wlr_compositor 内部管理 buffer 用。
    // 不参与 DMA-BUF 转发路径，不影响像素数据。
    state.renderer = wlr_renderer_autocreate(state.display);
    if (!state.renderer) {
        fprintf(stderr, "wlr_renderer_autocreate failed\n");
        return false;
    }
    wlr_renderer_init_wl_display(state.renderer, state.display);

    // 核心协议
    state.compositor = wlr_compositor_create(state.display, state.renderer);
    wlr_subcompositor_create(state.display);
    wlr_data_device_manager_create(state.display);

    // xdg-shell (窗口管理)
    state.xdg_shell = wlr_xdg_shell_create(state.display, 5);
    state.new_xdg_surface.notify = handle_new_xdg_surface;
    wl_signal_add(&state.xdg_shell->events.new_surface, &state.new_xdg_surface);

    // Output hook
    state.new_output.notify = handle_new_output;
    wl_signal_add(&state.backend->events.new_output, &state.new_output);

    // 加载 land 插件
    if (load_land_plugin()) {
        land_backend = land_create(state.renderer, state.display);
        if (land_backend)
            fprintf(stderr, "[land] backend ready\n");
        else
            fprintf(stderr, "[land] create failed (socket unreachable?)\n");
    }

    wl_list_init(&state.surfaces);

    // 启动后端 (会触发 new_output)
    if (!wlr_backend_start(state.backend)) {
        fprintf(stderr, "wlr_backend_start failed\n");
        return false;
    }

    // Wayland socket
    const char *socket = wl_display_add_socket_auto(state.display);
    if (!socket) {
        fprintf(stderr, "failed to add Wayland socket\n");
        return false;
    }
    setenv("WAYLAND_DISPLAY", socket, 0);
    fprintf(stderr, "[compositor] WAYLAND_DISPLAY=%s\n", socket);

    return true;
}

/* ================================================================
 * main
 * ================================================================ */
int main(int argc, char *argv[]) {
    wlr_log_init(WLR_DEBUG, NULL);

    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);

    if (!init_compositor()) {
        fprintf(stderr, "init failed\n");
        return 1;
    }

    fprintf(stderr, "[compositor] running\n");

    while (running) {
        // Dispatch Wayland events (client requests, etc.)
        if (wl_event_loop_dispatch(wl_display_get_event_loop(state.display), 100) < 0)
            break;
        wl_display_flush_clients(state.display);
    }

    fprintf(stderr, "[compositor] shutting down\n");

    // cleanup
    if (land_backend && land_destroy)
        land_destroy(land_backend);

    wl_display_destroy_clients(state.display);
    wlr_backend_destroy(state.backend);
    wlr_renderer_destroy(state.renderer);
    wl_display_destroy(state.display);

    return 0;
}
