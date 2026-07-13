CC      ?= gcc
PKGCFG  ?= pkg-config

CFLAGS  += -g -O2 -Wall -Wextra
CFLAGS  += $(shell $(PKGCFG) --cflags wlroots wayland-server)
LDLIBS  += $(shell $(PKGCFG) --libs wlroots wayland-server) -ldl -lm

TARGET  = wl-android-compositor

.PHONY: all clean run

all: $(TARGET)

$(TARGET): main.c
	$(CC) $(CFLAGS) $< -o $@ $(LDLIBS)

run: $(TARGET)
	WAYLAND_DISPLAY=wl-android-0 ./$(TARGET)

clean:
	rm -f $(TARGET)
