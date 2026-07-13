# wlroots Compositor Integration Guide

## Quick Start

```c
#include <wlr/render/wlr_renderer.h>

// 声明 wl-android 入口
extern void* land_wlroots_renderer_create(struct wlr_renderer* renderer,
                                          struct wl_display* display);
extern void  land_wlroots_renderer_destroy(void* backend);
extern bool  land_wlroots_buffer_submit(void* backend, struct wlr_buffer* buffer);
```

## Integration Pattern 1: Renderer Wrapper

Wrap the native wlr_renderer at compositor startup:

```c
// In your compositor's main.c or init
struct wlr_renderer* renderer = wlr_renderer_autocreate(display, NULL);
void* land_backend = land_wlroots_renderer_create(renderer, display);

// On each surface buffer commit, extract and forward:
// (hook into wlr_surface's commit signal)
static void handle_surface_commit(struct wl_listener* listener, void* data) {
    struct wlr_surface* surface = wlr_surface_from_resource(data);
    struct wlr_buffer* buffer = surface->buffer;
    if (buffer) {
        land_wlroots_buffer_submit(land_backend, buffer);
    }
}
```

### Surface Commit Signal Registration

```c
// Some compositor
static struct wl_listener surface_commit_listener;

static void setup_land_hook(struct wlr_surface* surface) {
    surface_commit_listener.notify = handle_surface_commit;
    wl_signal_add(&surface->events.commit, &surface_commit_listener);
}
```

## Integration Pattern 2: Custom wlr_backend

Alternatively, create a virtual `wlr_backend` that produces DMA-BUF outputs:

```c
static const struct wlr_backend_impl land_backend_impl = {
    .start = land_backend_start,
    .destroy = land_backend_destroy,
};

struct wlr_backend* land_backend_create(struct wl_display* display) {
    struct wlr_backend* backend = wlr_backend_init(
        &land_backend_impl, sizeof(struct wlr_backend));
    // ... init socket, landd connection
    return backend;
}
```

## Building

### As system plugin (recommended)

```bash
# Install libland_wlroots.so
sudo cp target/x86_64-unknown-linux-gnu/release/libland_wlroots.so \
      /usr/lib/wlroots/

# Set environment variable
export LAND_SOCKET=/dev/socket/land.sock
```

### Statically linked

Link `libland_wlroots.a` into your compositor binary:

```makefile
LDFLAGS += -l:libland_wlroots.a -ldl -lpthread
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LAND_SOCKET` | `/dev/socket/land.sock` | Unix socket path for landd |

## Debugging

```bash
# Check landd is running
ls -la /dev/socket/land.sock

# Monitor log
logcat -s landd land-native land-bridge  # Android
journalctl -f -t landd                    # Linux

# Verify socket connection
socat - UNIX-CONNECT:/dev/socket/land.sock
```

## Troubleshooting

| Problem | Likely Cause | Fix |
|---------|-------------|-----|
| `land_wlroots_buffer_get_dmabuf` returns false | Buffer is SHM not DMA-BUF | Modify compositor to use DMA-BUF allocator |
| Socket connection refused | landd not running | Start landd or check /dev/socket permissions |
| Vulkan import fails | Device lacks VK_KHR_external_memory_fd | Check `vulkaninfo --summary` |
| No frame on screen | App not connected to socket | Verify land-app is running and socket path matches |
