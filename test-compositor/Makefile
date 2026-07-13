CC      ?= gcc
PKGCFG  ?= pkg-config
# Debian 将 wlroots 的 .pc 命名为 wlroots-0.20.pc (版本后缀)
WLR_PC  ?= wlroots

CFLAGS  += -g -O2 -Wall -Wextra
CFLAGS  += $(shell PKG_CONFIG_PATH="/usr/lib/aarch64-linux-gnu/pkgconfig:$(PKG_CONFIG_PATH)" \
            $(PKGCFG) --cflags $(WLR_PC) wayland-server 2>/dev/null || \
            $(PKGCFG) --cflags wlroots-0.20 wayland-server)
LDLIBS  += $(shell PKG_CONFIG_PATH="/usr/lib/aarch64-linux-gnu/pkgconfig:$(PKG_CONFIG_PATH)" \
             $(PKGCFG) --libs $(WLR_PC) wayland-server 2>/dev/null || \
             $(PKGCFG) --libs wlroots-0.20 wayland-server) -lm

TARGET  = wl-android-compositor

.PHONY: all clean run

all: $(TARGET)

$(TARGET): main.c
	$(CC) $(CFLAGS) $< -o $@ $(LDLIBS)

run: $(TARGET)
	./$(TARGET)

clean:
	rm -f $(TARGET)
