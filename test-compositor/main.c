/**
 * wl-android-compositor — 无头嵌套 wlroots 合成器
 * wlroots 0.20 API
 */
#define _GNU_SOURCE
#define WLR_USE_UNSTABLE

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <errno.h>
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

/* socket */
static int sock_fd = -1;

static int connect_sock(void) {
    const char *path = getenv("LAND_SOCKET");
    if (!path) path = "/run/land.sock";
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0)
        { close(fd); return -1; }
    return fd;
}

struct land_hdr { uint32_t magic, type, len; };
struct land_frm { uint32_t w, h, fmt, stride; uint64_t serial; };

static void send_frame(struct wlr_dmabuf_attributes *a, int dmafd, uint64_t serial) {
    if (sock_fd < 0) {
        sock_fd = connect_sock();
        if (sock_fd < 0) return;
    }
    struct land_hdr h = { .magic = 0x4C414E00, .type = 0x4C414E01, .len = sizeof(struct land_frm) };
    struct land_frm f = { .w = a->width, .h = a->height, .fmt = a->format,
                          .stride = a->stride[0], .serial = serial };
    struct iovec iov[] = { {&h,sizeof(h)}, {&f,sizeof(f)} };
    char cmsg[CMSG_SPACE(sizeof(int))];
    struct msghdr msg = { .msg_iov = iov, .msg_iovlen = 2, .msg_control = cmsg, .msg_controllen = sizeof(cmsg) };
    struct cmsghdr *cp = CMSG_FIRSTHDR(&msg);
    cp->cmsg_level = SOL_SOCKET; cp->cmsg_type = SCM_RIGHTS; cp->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cp), &dmafd, sizeof(int));
    if (sendmsg(sock_fd, &msg, MSG_NOSIGNAL) < 0) { close(sock_fd); sock_fd = -1; }
}

/* surface tracking */
static uint64_t g_serial = 1;

static void on_commit(struct wl_listener *l, void *data) {
    struct wlr_surface *s = l->data;  /* listener.data = surface ptr */
    (void)data;
    struct wlr_buffer *buf = s->buffer;
    if (!buf) return;
    if (!wlr_buffer_lock(buf)) return;
    struct wlr_dmabuf_attributes dmabuf = {0};
    if (!wlr_buffer_get_dmabuf(buf, &dmabuf)) { wlr_buffer_unlock(buf); return; }
    int fd = fcntl(dmabuf.fd[0], F_DUPFD_CLOEXEC, 0);
    if (fd < 0) { wlr_buffer_unlock(buf); return; }
    send_frame(&dmabuf, fd, __atomic_fetch_add(&g_serial, 1, __ATOMIC_RELAXED));
    close(fd);
    wlr_buffer_unlock(buf);
}

/* xdg-shell */
static struct wl_listener commit_listeners[64];
static int num_xdg = 0;

static void on_xdg_map(struct wl_listener *l, void *data) {
    struct wlr_xdg_surface *xdg = wl_container_of(l, xdg, map);
    (void)data;
    if (num_xdg >= 64) return;
    commit_listeners[num_xdg].notify = on_commit;
    commit_listeners[num_xdg].data = xdg->surface;
    wl_signal_add(&xdg->surface->events.commit, &commit_listeners[num_xdg]);
    num_xdg++;
}

static void on_new_xdg(struct wl_listener *l, void *data) {
    struct wlr_xdg_surface *xdg = data;
    (void)l;
    if (xdg->role != WLR_XDG_SURFACE_ROLE_TOPLEVEL) return;
    wl_signal_add(&xdg->events.map, &(struct wl_listener){.notify=on_xdg_map});
}

/* main */
static int running = 1;
static void sig_h(int s) { running = 0; }

int main(void) {
    wlr_log_init(WLR_DEBUG, NULL);
    signal(SIGINT, sig_h); signal(SIGTERM, sig_h); signal(SIGPIPE, SIG_IGN);

    struct wl_display *disp = wl_display_create();
    struct wlr_backend *be = wlr_headless_backend_create(disp);
    struct wlr_renderer *rend = wlr_renderer_autocreate(be);
    wlr_renderer_init_wl_display(rend, disp);

    wlr_compositor_create(disp, 6, rend);
    wlr_subcompositor_create(disp);
    wlr_data_device_manager_create(disp);

    struct wlr_xdg_shell *xdg = wlr_xdg_shell_create(disp, 6);
    struct wl_listener xdg_l = { .notify = on_new_xdg };
    wl_signal_add(&xdg->events.new_surface, &xdg_l);

    /* headless output */
    wlr_headless_add_output(be, 1920, 1080);

    wlr_backend_start(be);
    const char *sock_name = wl_display_add_socket_auto(disp);
    setenv("WAYLAND_DISPLAY", sock_name, 1);
    fprintf(stderr, "[wl-android] WAYLAND_DISPLAY=%s\n", sock_name);

    sock_fd = connect_sock();
    if (sock_fd < 0)
        fprintf(stderr, "[wl-android] waiting for land-app...\n");
    else
        fprintf(stderr, "[wl-android] connected\n");

    while (running && wl_display_dispatch(disp) != -1) {
        wl_display_flush_clients(disp);
    }

    if (sock_fd >= 0) close(sock_fd);
    wl_display_destroy(disp);
    return 0;
}
