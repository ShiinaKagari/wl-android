CC      ?= gcc
PKGCFG  ?= pkg-config
PKG_CONFIG_PATH ?= /usr/lib/aarch64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/pkgconfig:/usr/share/pkgconfig

CFLAGS  += -g -O2 -Wall -Wextra -DWLR_USE_UNSTABLE
CFLAGS  += $(shell PKG_CONFIG_PATH="$(PKG_CONFIG_PATH)" $(PKGCFG) --cflags wlroots wayland-server 2>/dev/null || \
            PKG_CONFIG_PATH="$(PKG_CONFIG_PATH)" $(PKGCFG) --cflags wlroots-0.20 wayland-server)
LDLIBS  += $(shell PKG_CONFIG_PATH="$(PKG_CONFIG_PATH)" $(PKGCFG) --libs wlroots wayland-server 2>/dev/null || \
             PKG_CONFIG_PATH="$(PKG_CONFIG_PATH)" $(PKGCFG) --libs wlroots-0.20 wayland-server) -lm

TARGET  = wl-android-compositor

.PHONY: all clean run

all: $(TARGET)

$(TARGET): main.c
	$(CC) $(CFLAGS) $< -o $@ $(LDLIBS)

run: $(TARGET)
	./$(TARGET)

clean:
	rm -f $(TARGET)
