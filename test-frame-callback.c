// test-frame-callback.c — Minimal Wayland client to verify the compositor
// dispatches wl_callback.done after surface commit.
//
// Build: gcc -o test-frame-callback test-frame-callback.c -lwayland-client
// Run:   WAYLAND_DISPLAY=wayland-1 ./test-frame-callback

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/poll.h>
#include <wayland-client.h>

static int got_callback = 0;

static void
callback_done(void *data, struct wl_callback *callback, uint32_t time)
{
    (void)data;
    (void)time;
    got_callback = 1;
    fprintf(stderr, "wl_callback.done(time=%u) — PASS\n", time);
    wl_callback_destroy(callback);
}

static const struct wl_callback_listener callback_listener = {
    .done = callback_done,
};

static void
registry_global(void *data, struct wl_registry *registry,
                uint32_t name, const char *interface, uint32_t version)
{
    static struct wl_compositor **compositor = NULL;
    compositor = data;
    if (strcmp(interface, "wl_compositor") == 0) {
        *compositor = wl_registry_bind(registry, name,
                                       &wl_compositor_interface, 1);
        fprintf(stderr, "wl_compositor v%u bound\n", version);
    }
}

static void
registry_global_remove(void *data, struct wl_registry *registry, uint32_t name)
{
    (void)data; (void)registry; (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

int main(int argc, char **argv)
{
    const char *socket_name = NULL;
    if (argc > 1) {
        socket_name = argv[1];
    } else {
        const char *xdg = getenv("XDG_RUNTIME_DIR");
        if (!xdg) xdg = "/tmp";
        const char *display = getenv("WAYLAND_DISPLAY");
        if (!display) display = "wayland-0";
        static char buf[256];
        snprintf(buf, sizeof(buf), "%s/%s", xdg, display);
        socket_name = buf;
    }

    fprintf(stderr, "Connecting to %s\n", socket_name);
    struct wl_display *display = wl_display_connect(socket_name);
    if (!display) {
        fprintf(stderr, "FAIL: cannot connect to %s\n", socket_name);
        return 1;
    }

    struct wl_compositor *compositor = NULL;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, &compositor);

    wl_display_roundtrip(display);

    if (!compositor) {
        fprintf(stderr, "FAIL: no wl_compositor global\n");
        wl_display_disconnect(display);
        return 1;
    }

    struct wl_surface *surface = wl_compositor_create_surface(compositor);
    struct wl_callback *callback = wl_surface_frame(surface);
    wl_callback_add_listener(callback, &callback_listener, NULL);

    wl_surface_commit(surface);
    wl_display_flush(display);

    fprintf(stderr, "Surface committed with frame callback, waiting for done...\n");

    struct pollfd fds[1];
    fds[0].fd = wl_display_get_fd(display);
    fds[0].events = POLLIN;

    int timeout_ms = 5000;
    int elapsed = 0;
    int step = 50;

    while (!got_callback && elapsed < timeout_ms) {
        if (wl_display_prepare_read(display) != 0) {
            // Already have events to dispatch
        } else {
            poll(fds, 1, step);
            wl_display_cancel_read(display);
        }
        wl_display_dispatch(display);
        wl_display_flush(display);
        elapsed += step;
    }

    if (got_callback) {
        printf("TEST PASSED: frame callback dispatched\n");
    } else {
        printf("TEST FAILED: no callback within %dms\n", timeout_ms);
        wl_surface_destroy(surface);
        wl_display_disconnect(display);
        return 1;
    }

    wl_surface_destroy(surface);
    wl_display_disconnect(display);
    return 0;
}
