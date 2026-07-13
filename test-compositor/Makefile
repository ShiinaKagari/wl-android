CC      ?= gcc
PKGCFG  ?= pkg-config

CFLAGS  += -g -O2 -Wall -Wextra
CFLAGS  += $(shell $(PKGCFG) --cflags wlroots wayland-server)
LDLIBS  += $(shell $(PKGCFG) --libs wlroots wayland-server) -lm

TARGET  = wl-android-compositor

.PHONY: all clean run

all: $(TARGET)

$(TARGET): main.c
	$(CC) $(CFLAGS) $< -o $@ $(LDLIBS)

run: $(TARGET)
	./$(TARGET)

clean:
	rm -f $(TARGET)
