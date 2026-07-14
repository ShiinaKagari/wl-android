/**
 * wl-android-compositor — 无头嵌套 wlroots 合成器 (wlroots 0.20)
 */
#define _GNU_SOURCE

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

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

/* ===== socket ===== */

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

/* ===== 测试帧生成 (DRM dumb buffer) ===== */

static int drm_fd = -1;
static int open_drm(void) {
	if (drm_fd >= 0) return drm_fd;
	const char *dev = getenv("ANLAND_DRM_DEVICE");
	if (!dev) dev = "/dev/dri/renderD128";
	drm_fd = open(dev, O_RDWR);
	return drm_fd;
}

static void fill_gradient(void *map, int width, int height, int stride, int frame_count) {
	for (int y = 0; y < height; y++) {
		unsigned char *row = (unsigned char *)map + y * stride;
		for (int x = 0; x < width; x++) {
			int i = x * 4;
			row[i+0] = (x * 255 / width) ^ (frame_count & 0xFF);        // B
			row[i+1] = (y * 255 / height) ^ ((frame_count >> 4) & 0xFF); // G
			row[i+2] = ((x+y) * 255 / (width+height)) ^ ((frame_count >> 8) & 0xFF); // R
			row[i+3] = 255; // A
		}
	}
}

static int generate_test_frame(uint32_t *out_width, uint32_t *out_height, uint32_t *out_stride, uint32_t *out_format) {
	int fd = open_drm();
	if (fd < 0) return -1;

	struct drm_mode_create_dumb create = {
		.width = 640,
		.height = 480,
		.bpp = 32,
	};
	int ret = ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create);
	if (ret < 0) return -1;

	struct drm_prime_handle prime = {
		.handle = create.handle,
		.flags = DRM_CLOEXEC | O_RDWR,
	};
	ret = ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &prime);
	if (ret < 0) { close(create.handle); return -1; }

	// Map and fill
	struct drm_mode_map_dumb map_req = { .handle = create.handle };
	ret = ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map_req);
	if (ret < 0) { close(prime.fd); close(create.handle); return -1; }

	void *map = mmap(NULL, create.size, PROT_WRITE, MAP_SHARED, fd, map_req.offset);
	if (map == MAP_FAILED) { close(prime.fd); close(create.handle); return -1; }

	static int frame_count = 0;
	fill_gradient(map, create.width, create.height, create.pitch, frame_count++);
	munmap(map, create.size);

	// Cleanup the handle (prime fd keeps the buffer alive)
	close(create.handle);

	*out_width = create.width;
	*out_height = create.height;
	*out_stride = create.pitch;
	*out_format = 0x34325258; // DRM_FORMAT_XRGB8888
	return prime.fd;
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
	if (!wlr_buffer_get_dmabuf(buf, &dmabuf)) { wlr_buffer_unlock(buf); return; }
	int fd = fcntl(dmabuf.fd[0], F_DUPFD_CLOEXEC, 0);
	if (fd < 0) { wlr_buffer_unlock(buf); return; }
	send_frame(&dmabuf, fd, __atomic_fetch_add(&g_serial, 1, __ATOMIC_RELAXED));
	close(fd);
	wlr_buffer_unlock(buf);
}

/* ===== xdg-shell ===== */

struct wl_listener commit_listeners[64];
static int listener_count = 0;

static void on_xdg_map(struct wl_listener *l, void *data) {
	struct wlr_xdg_surface *xdg = data;
	(void)l;
	if (listener_count >= 64) return;
	commit_listeners[listener_count].notify = on_commit;
	wl_signal_add(&xdg->surface->events.commit, &commit_listeners[listener_count]);
	listener_count++;
}

static void on_new_xdg(struct wl_listener *l, void *data) {
	struct wlr_xdg_surface *xdg = data;
	(void)l;
	if (xdg->role != WLR_XDG_SURFACE_ROLE_TOPLEVEL) return;
	struct wl_listener *map_l = calloc(1, sizeof(*map_l));
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

	struct timespec last_test = {0};

	while (running) {
		wl_event_loop_dispatch(loop, 100);
		wl_display_flush_clients(disp);

		// Send a test frame every 2 seconds (to verify pipeline)
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		if (now.tv_sec - last_test.tv_sec >= 2) {
			last_test = now;
			uint32_t w, h, stride, fmt;
			int dmafd = generate_test_frame(&w, &h, &stride, &fmt);
			if (dmafd >= 0) {
				struct wlr_dmabuf_attributes dmabuf = {
					.width = (int)w, .height = (int)h,
					.format = fmt, .n_planes = 1,
					.stride = { (uint32_t)stride, 0, 0, 0 },
				};
				int dup_fd = fcntl(dmafd, F_DUPFD_CLOEXEC, 0);
				if (dup_fd >= 0) {
					send_frame(&dmabuf, dup_fd, __atomic_fetch_add(&g_serial, 1, __ATOMIC_RELAXED));
					close(dup_fd);
				}
				close(dmafd);
			}
		}
	}

	if (sock_fd >= 0) close(sock_fd);
	wl_display_destroy(disp);
	return 0;
}
