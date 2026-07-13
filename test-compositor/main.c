/**
 * wl-android-compositor: 无头嵌套 wlroots 合成器
 *
 * 仅有的容器侧组件。不依赖插件，直接通过 wlroots API 提取 DMA-BUF fd。
 * 所有 Wayland 客户端连到它的 socket，每帧 DMA-BUF fd 通过 SCM_RIGHTS
 * 转发到 landd → land-app → Android 屏幕。
 *
 * 构建:
 *   gcc main.c -o wl-android-compositor \
 *       $(pkg-config --cflags --libs wlroots wayland-server) \
 *       -ldl -lpthread -lm
 *
 * 运行:
 *   wl-android-compositor
 *   WAYLAND_DISPLAY=wl-android-0 your-app
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <errno.h>
#include <sys/socket.h>
#include <sys/un.h>

#include <wayland-server-core.h>
#include <wlr/backend.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/types/wlr_subcompositor.h>
#include <wlr/types/wlr_xdg_shell.h>
#include <wlr/util/log.h>

/* ================================================================
 * landd 通信
 * ================================================================ */

static int landd_fd = -1;

static int connect_landd(void) {
    const char *path = getenv("LAND_SOCKET");
    if (!path) path = "/dev/socket/land.sock";

    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;

    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

struct __attribute__((packed)) land_header {
    uint32_t magic;
    uint32_t msg_type;
    uint32_t length;
};

struct __attribute__((packed)) land_frame {
    uint32_t width;
    uint32_t height;
    uint32_t format;
    uint32_t stride;
    uint64_t serial;
};

#define LAND_MAGIC   0x4C414E00
#define MSG_FRAME    0x4C414E01
#define MSG_TOUCH    0x4C414E02

static int send_frame(int fd, struct wlr_dmabuf_attributes *attr, int dmabuf_fd, uint64_t serial) {
    struct land_header hdr = {
        .magic = LAND_MAGIC,
        .msg_type = MSG_FRAME,
        .length = sizeof(struct land_frame),
    };
    struct land_frame frame = {
        .width = (uint32_t)attr->width,
        .height = (uint32_t)attr->height,
        .format = attr->format,
        .stride = (uint32_t)attr->stride[0],
        .serial = serial,
    };

    struct iovec iov[] = {
        { .iov_base = &hdr, .iov_len = sizeof(hdr) },
        { .iov_base = &frame, .iov_len = sizeof(frame) },
    };

    char cmsg_buf[CMSG_SPACE(sizeof(int))];
    struct msghdr msg = { .msg_iov = iov, .msg_iovlen = 2 };
    msg.msg_control = cmsg_buf;
    msg.msg_controllen = sizeof(cmsg_buf);

    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &dmabuf_fd, sizeof(int));

    return sendmsg(fd, &msg, MSG_NOSIGNAL);
}

/* ================================================================
 * Surface 跟踪 + DMA-BUF 转发
 * ================================================================ */

static uint64_t frame_serial = 1;

static void handle_surface_commit(struct wl_listener *listener, void *data);

struct tracked_surface {
    struct wlr_surface *wlr_surface;
    struct wl_listener commit;
    struct wl_listener destroy;
    struct wl_list link;
};

static void surface_destroy_handler(struct wl_listener *listener, void *data) {
    struct tracked_surface *ts = wl_container_of(listener, ts, destroy);
    (void)data;
    wl_list_remove(&ts->commit.link);
    wl_list_remove(&ts->destroy.link);
    wl_list_remove(&ts->link);
    free(ts);
}

static void handle_surface_commit(struct wl_listener *listener, void *data) {
    struct tracked_surface *ts = wl_container_of(listener, ts, commit);
    (void)data;

    if (landd_fd < 0) return;

    struct wlr_buffer *buffer = wlr_surface_get_buffer(ts->wlr_surface);
    if (!buffer) return;

    struct wlr_dmabuf_attributes dmabuf = {0};
    if (!wlr_buffer_get_dmabuf(buffer, &dmabuf)) {
        wlr_buffer_unlock(buffer);
        return;  // SHM buffer, skip
    }

    // dup fd 后转发
    int dup_fd = fcntl(dmabuf.fd[0], F_DUPFD_CLOEXEC, 0);
    if (dup_fd < 0) {
        wlr_buffer_unlock(buffer);
        return;
    }

    uint64_t serial = __atomic_fetch_add(&frame_serial, 1, __ATOMIC_RELAXED);
    send_frame(landd_fd, &dmabuf, dup_fd, serial);
    close(dup_fd);
    wlr_buffer_unlock(buffer);
}

static struct tracked_surface *track_surface(struct wlr_surface *surface) {
    struct tracked_surface *ts = calloc(1, sizeof(*ts));
    if (!ts) return NULL;
    ts->wlr_surface = surface;
    ts->commit.notify = handle_surface_commit;
    ts->destroy.notify = surface_destroy_handler;
    wl_signal_add(&surface->events.commit, &ts->commit);
    wl_signal_add(&surface->events.destroy, &ts->destroy);
    return ts;
}

/* ================================================================
 * xdg-shell
 * ================================================================ */

static void handle_xdg_surface_map(struct wl_listener *listener, void *data);
static void handle_xdg_surface_destroy(struct wl_listener *listener, void *data);

struct xdg_data {
    struct wlr_xdg_surface *xdg_surface;
    struct wl_listener map;
    struct wl_listener destroy;
};

static void handle_new_xdg_surface(struct wl_listener *listener, void *data) {
    struct wlr_xdg_surface *xdg = data;
    if (xdg->role != WLR_XDG_SURFACE_ROLE_TOPLEVEL &&
        xdg->role != WLR_XDG_SURFACE_ROLE_POPUP)
        return;

    struct xdg_data *sd = calloc(1, sizeof(*sd));
    if (!sd) return;
    sd->xdg_surface = xdg;
    sd->map.notify = handle_xdg_surface_map;
    sd->destroy.notify = handle_xdg_surface_destroy;
    wl_signal_add(&xdg->events.map, &sd->map);
    wl_signal_add(&xdg->events.destroy, &sd->destroy);
}

static void handle_xdg_surface_map(struct wl_listener *listener, void *data) {
    struct xdg_data *sd = wl_container_of(listener, sd, map);
    (void)data;
    track_surface(sd->xdg_surface->surface);
}

static void handle_xdg_surface_destroy(struct wl_listener *listener, void *data) {
    struct xdg_data *sd = wl_container_of(listener, sd, destroy);
    (void)data;
    wl_list_remove(&sd->map.link);
    wl_list_remove(&sd->destroy.link);
    free(sd);
}

/* ================================================================
 * 合成器
 * ================================================================ */

struct compositor_state {
    struct wl_display *display;
    struct wlr_backend *backend;
    struct wlr_renderer *renderer;
    struct wlr_compositor *compositor;
    struct wlr_xdg_shell *xdg_shell;
    struct wl_listener new_xdg_surface;
    struct wl_listener new_output;
};

static struct compositor_state state;
static int running = 1;
static void handle_signal(int signo) { running = 0; }

static void handle_new_output(struct wl_listener *listener, void *data) {
    struct wlr_output *output = data;
    wlr_output_enable(output, true);
    wlr_output_set_custom_mode(output, 1920, 1080, 0);
    wlr_output_commit(output);
}

static bool init_compositor(void) {
    state.display = wl_display_create();
    if (!state.display) return false;

    state.backend = wlr_headless_backend_create(state.display);
    if (!state.backend) return false;

    state.renderer = wlr_renderer_autocreate(state.display);
    if (!state.renderer) return false;
    wlr_renderer_init_wl_display(state.renderer, state.display);

    state.compositor = wlr_compositor_create(state.display, state.renderer);
    wlr_subcompositor_create(state.display);
    wlr_data_device_manager_create(state.display);

    state.xdg_shell = wlr_xdg_shell_create(state.display, 5);
    state.new_xdg_surface.notify = handle_new_xdg_surface;
    wl_signal_add(&state.xdg_shell->events.new_surface, &state.new_xdg_surface);

    state.new_output.notify = handle_new_output;
    wl_signal_add(&state.backend->events.new_output, &state.new_output);

    if (!wlr_backend_start(state.backend)) return false;

    const char *socket = wl_display_add_socket_auto(state.display);
    if (!socket) return false;
    setenv("WAYLAND_DISPLAY", socket, 0);
    fprintf(stderr, "[compositor] WAYLAND_DISPLAY=%s\n", socket);

    // 连接 landd
    landd_fd = connect_landd();
    if (landd_fd < 0)
        fprintf(stderr, "[compositor] landd not reachable (deferring)\n");
    else
        fprintf(stderr, "[compositor] connected to landd\n");

    return true;
}

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
        if (wl_event_loop_dispatch(wl_display_get_event_loop(state.display), 100) < 0)
            break;
        wl_display_flush_clients(state.display);
    }

    if (landd_fd >= 0) close(landd_fd);
    wl_display_destroy_clients(state.display);
    wlr_backend_destroy(state.backend);
    wlr_renderer_destroy(state.renderer);
    wl_display_destroy(state.display);
    return 0;
}
