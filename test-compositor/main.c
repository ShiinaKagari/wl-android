/**
 * wl-android-compositor: 无头嵌套 wlroots 合成器
 *
 * 直连 land-app，无中间守护进程。App 不在线时丢帧。
 * 唯一容器侧组件。
 *
 * 构建:
 *   gcc main.c -o wl-android-compositor \
 *       $(pkg-config --cflags --libs wlroots wayland-server) -lm
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <fcntl.h>

#include <wayland-server-core.h>
#include <wlr/backend.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_subcompositor.h>
#include <wlr/types/wlr_xdg_shell.h>
#include <wlr/util/log.h>

/* ================================================================
 * App 直连 (无 landd)
 * ================================================================ */

static int app_fd = -1;

static int connect_app(void) {
    const char *path = getenv("LAND_SOCKET");
    if (!path) path = "/run/land.sock";

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;

    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);

    int ret = connect(fd, (struct sockaddr *)&addr, sizeof(addr));
    if (ret < 0) { close(fd); return -1; }

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

static int send_frame(int fd, struct wlr_dmabuf_attributes *attr, int dmabuf_fd, uint64_t serial) {
    struct land_header hdr = {
        .magic = LAND_MAGIC, .msg_type = MSG_FRAME,
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

static void surface_commit_handler(struct wl_listener *listener, void *data);
static void surface_destroy_handler(struct wl_listener *listener, void *data);

struct tracked_surface {
    struct wlr_surface *surface;
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

static void surface_commit_handler(struct wl_listener *listener, void *data) {
    struct tracked_surface *ts = wl_container_of(listener, ts, commit);
    (void)data;

    struct wlr_buffer *buffer = wlr_surface_get_buffer(ts->surface);
    if (!buffer) return;

    struct wlr_dmabuf_attributes dmabuf = {0};
    if (!wlr_buffer_get_dmabuf(buffer, &dmabuf)) {
        wlr_buffer_unlock(buffer);
        return;
    }

    // 直连 App。App 不在线时丢帧。
    if (app_fd < 0) {
        app_fd = connect_app();
        if (app_fd < 0) { wlr_buffer_unlock(buffer); return; }
    }

    int dup_fd = fcntl(dmabuf.fd[0], F_DUPFD_CLOEXEC, 0);
    if (dup_fd < 0) { wlr_buffer_unlock(buffer); return; }

    uint64_t serial = __atomic_fetch_add(&frame_serial, 1, __ATOMIC_RELAXED);
    int ret = send_frame(app_fd, &dmabuf, dup_fd, serial);
    close(dup_fd);

    if (ret < 0) { close(app_fd); app_fd = -1; }

    wlr_buffer_unlock(buffer);
}

static struct tracked_surface *track_surface(struct wlr_surface *surface) {
    struct tracked_surface *ts = calloc(1, sizeof(*ts));
    if (!ts) return NULL;
    ts->surface = surface;
    ts->commit.notify = surface_commit_handler;
    ts->destroy.notify = surface_destroy_handler;
    wl_signal_add(&surface->events.commit, &ts->commit);
    wl_signal_add(&surface->events.destroy, &ts->destroy);
    return ts;
}

/* ================================================================
 * xdg-shell
 * ================================================================ */

static void xdg_map(struct wl_listener *l, void *d);
static void xdg_destroy(struct wl_listener *l, void *d);

struct xdg_data {
    struct wlr_xdg_surface *xdg;
    struct wl_listener map;
    struct wl_listener destroy;
};

static void new_xdg_surface(struct wl_listener *l, void *d) {
    struct wlr_xdg_surface *xdg = d;
    if (xdg->role != WLR_XDG_SURFACE_ROLE_TOPLEVEL &&
        xdg->role != WLR_XDG_SURFACE_ROLE_POPUP) return;
    struct xdg_data *sd = calloc(1, sizeof(*sd));
    if (!sd) return;
    sd->xdg = xdg;
    sd->map.notify = xdg_map;
    sd->destroy.notify = xdg_destroy;
    wl_signal_add(&xdg->events.map, &sd->map);
    wl_signal_add(&xdg->events.destroy, &sd->destroy);
}

static void xdg_map(struct wl_listener *l, void *d) {
    struct xdg_data *sd = wl_container_of(l, sd, map);
    (void)d;
    track_surface(sd->xdg->surface);
}

static void xdg_destroy(struct wl_listener *l, void *d) {
    struct xdg_data *sd = wl_container_of(l, sd, destroy);
    (void)d;
    wl_list_remove(&sd->map.link);
    wl_list_remove(&sd->destroy.link);
    free(sd);
}

/* ================================================================
 * 合成器
 * ================================================================ */

static struct wl_display *display;
static int running = 1;
static void sig_handler(int s) { running = 0; }
static void output_create(struct wl_listener *l, void *d) {
    struct wlr_output *o = d;
    wlr_output_enable(o, 1);
    wlr_output_set_custom_mode(o, 1920, 1080, 0);
    wlr_output_commit(o);
}

int main(void) {
    wlr_log_init(WLR_DEBUG, NULL);
    signal(SIGINT, sig_handler);
    signal(SIGTERM, sig_handler);

    display = wl_display_create();

    struct wlr_backend *backend = wlr_headless_backend_create(display);
    struct wlr_renderer *renderer = wlr_renderer_autocreate(display);
    wlr_renderer_init_wl_display(renderer, display);

    wlr_compositor_create(display, renderer);
    wlr_subcompositor_create(display);
    wlr_data_device_manager_create(display);

    struct wlr_xdg_shell *xdg = wlr_xdg_shell_create(display, 5);
    struct wl_listener xdg_listener = { .notify = new_xdg_surface };
    wl_signal_add(&xdg->events.new_surface, &xdg_listener);

    struct wl_listener out_listener = { .notify = output_create };
    wl_signal_add(&backend->events.new_output, &out_listener);

    wlr_backend_start(backend);

    const char *socket = wl_display_add_socket_auto(display);
    setenv("WAYLAND_DISPLAY", socket, 1);
    fprintf(stderr, "[wl-android] WAYLAND_DISPLAY=%s\n", socket);
    fprintf(stderr, "[wl-android] connecting to land-app...\n");

    // 尝试连接 App (非阻塞，连不上不影响)
    app_fd = connect_app();
    if (app_fd < 0)
        fprintf(stderr, "[wl-android] app not running, frames will be dropped\n");
    else
        fprintf(stderr, "[wl-android] connected to land-app\n");

    while (running) {
        wl_event_loop_dispatch(wl_display_get_event_loop(display), 100);
        wl_display_flush_clients(display);
    }

    if (app_fd >= 0) close(app_fd);
    wl_display_destroy_clients(display);
    wlr_backend_destroy(backend);
    wlr_renderer_destroy(renderer);
    wl_display_destroy(display);
    return 0;
}
