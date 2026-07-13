/**
 * wl-android-compositor — 无头嵌套 wlroots 合成器 (wlroots 0.20)
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
#include <wlr/backend/headless.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_single_pixel_buffer_v1.h>
#include <wlr/types/wlr_subcompositor.h>
#include <wlr/types/wlr_xdg_shell.h>
#include <wlr/util/log.h>

/* ===== socket — 直连 (无 landd) ===== */

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
	if (sock_fd < 0) sock_fd = connect_sock();
	if (sock_fd < 0) return;

	struct land_hdr h = { .magic = 0x4C414E00, .type = 0x4C414E01, .len = sizeof(struct land_frm) };
	struct land_frm f = { .w = a->width, .h = a->height, .fmt = a->format,
	                      .stride = a->stride[0], .serial = serial };
	struct iovec iov[] = { {&h,sizeof(h)}, {&f,sizeof(f)} };
	char cmsg[CMSG_SPACE(sizeof(int))];
	struct msghdr msg = { .msg_iov = iov, .msg_iovlen = 2,
	                      .msg_control = cmsg, .msg_controllen = sizeof(cmsg) };
	struct cmsghdr *cp = CMSG_FIRSTHDR(&msg);
	cp->cmsg_level = SOL_SOCKET; cp->cmsg_type = SCM_RIGHTS;
	cp->cmsg_len = CMSG_LEN(sizeof(int));
	memcpy(CMSG_DATA(cp), &dmafd, sizeof(int));
	if (sendmsg(sock_fd, &msg, MSG_NOSIGNAL) < 0) { close(sock_fd); sock_fd = -1; }
}

/* ===== surface commit → DMA-BUF → socket ===== */

static uint64_t g_serial = 1;

static void on_commit(struct wl_listener *l, void *data) {
	struct wlr_surface *s = data;
	(void)l;
	if (!s->buffer) return;
	struct wlr_buffer *buf = wlr_buffer_lock((struct wlr_buffer *)s->buffer);
	if (!buf) return;

	struct wlr_dmabuf_attributes dmabuf = {0};
	if (!wlr_buffer_get_dmabuf(buf, &dmabuf))
		{ wlr_buffer_unlock(buf); return; }

	int fd = fcntl(dmabuf.fd[0], F_DUPFD_CLOEXEC, 0);
	if (fd < 0) { wlr_buffer_unlock(buf); return; }

	send_frame(&dmabuf, fd, __atomic_fetch_add(&g_serial, 1, __ATOMIC_RELAXED));
	close(fd);
	wlr_buffer_unlock(buf);
}

/* ===== xdg-shell 跟踪 ===== */

#define MAX_SURFACES 64
static struct wl_listener commit_listeners[MAX_SURFACES];
static int listener_count = 0;

static void on_xdg_map(struct wl_listener *l, void *data) {
	struct wlr_xdg_surface *xdg = data;
	(void)l;
	if (listener_count >= MAX_SURFACES) return;
	commit_listeners[listener_count].notify = on_commit;
	wl_signal_add(&xdg->surface->events.commit, &commit_listeners[listener_count]);
	listener_count++;
}

static void on_new_xdg(struct wl_listener *l, void *data) {
	struct wlr_xdg_surface *xdg = data;
	(void)l;
	if (xdg->role != WLR_XDG_SURFACE_ROLE_TOPLEVEL) return;

	struct wl_listener *map_l = calloc(1, sizeof(struct wl_listener));
	if (!map_l) return;
	map_l->notify = on_xdg_map;
	wl_signal_add(&xdg->surface->events.map, map_l);
}

/* ===== main ===== */

static int running = 1;
static void sig_h(int s) { (void)s; running = 0; }

int main(void) {
	wlr_log_init(WLR_DEBUG, NULL);
	signal(SIGINT, sig_h); signal(SIGTERM, sig_h); signal(SIGPIPE, SIG_IGN);

	struct wl_display *disp = wl_display_create();
	struct wl_event_loop *loop = wl_display_get_event_loop(disp);

	struct wlr_backend *be = wlr_headless_backend_create(loop);
	struct wlr_renderer *rend = wlr_renderer_autocreate(be);
	wlr_renderer_init_wl_display(rend, disp);

    wlr_compositor_create(disp, 6, rend);
    wlr_subcompositor_create(disp);
    wlr_data_device_manager_create(disp);
    wlr_single_pixel_buffer_manager_v1_create(disp);

	struct wlr_xdg_shell *xdg = wlr_xdg_shell_create(disp, 6);
	struct wl_listener xdg_l = { .notify = on_new_xdg };
	wl_signal_add(&xdg->events.new_surface, &xdg_l);

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

	while (running) {
		wl_event_loop_dispatch(loop, 100);
		wl_display_flush_clients(disp);
	}

	if (sock_fd >= 0) close(sock_fd);
	wl_display_destroy(disp);
	return 0;
}
