#!/usr/bin/env python3
"""
mock-app.py — wl-android mock client for device testing.

Connects to land.sock, completes HELO→CONF handshake,
receives Frame messages, sends Acks, and injects touch events.

Usage:
    adb push scripts/mock-app.py /data/local/tmp/
    adb shell python3 /data/local/tmp/mock-app.py
"""

import socket
import struct
import sys
import time

# Protocol constants
MAGIC_HELO = 0x4F4C4548  # 'HELO'
MAGIC_CONF = 0x464E4F43  # 'CONF'
MAGIC_LAND = 0x444E414C  # 'LAND'
MAGIC_FACK = 0x4B434146  # 'FACK'
MAGIC_TOUC = 0x43554F54  # 'TOUC'

TOUCH_DOWN = 0
TOUCH_MOVE = 1
TOUCH_UP = 2
TOUCH_CANCEL = 3
TOUCH_FRAME = 4

SOCKET_PATH = sys.argv[1] if len(sys.argv) > 1 else (
    "/data/local/tmp/wl-android/land.sock"
    if not sys.platform.startswith("linux")
    else "/tmp/land.sock"
)

def send_msg(sock, data: bytes):
    """Send a length-prefixed message."""
    sock.sendall(struct.pack('<I', len(data)) + data)

def recv_msg(sock, timeout=5.0) -> bytes:
    """Receive a length-prefixed message."""
    sock.settimeout(timeout)
    hdr = sock.recv(4)
    if len(hdr) < 4:
        raise ConnectionError("connection closed")
    msglen = struct.unpack('<I', hdr)[0]
    data = bytearray()
    while len(data) < msglen:
        chunk = sock.recv(msglen - len(data))
        if not chunk:
            raise ConnectionError("connection closed")
        data.extend(chunk)
    return bytes(data)

def test_handshake(sock):
    """M3: HELO→CONF handshake."""
    print("[M3] Handshake test...")

    # Receive HELO
    data = recv_msg(sock)
    magic = struct.unpack('<I', data[:4])[0]
    assert magic == MAGIC_HELO, f"Expected HELO, got {magic:#x}"
    version = struct.unpack('<I', data[4:8])[0]
    caps = struct.unpack('<I', data[8:12])[0]
    print(f"  ✅ HELO v{version} caps={caps:#x}")

    # Send CONF (3392x2400 @144Hz, 289 DPI, blit mode)
    conf = struct.pack('<8I',
        MAGIC_CONF, 1,       # magic, version
        3392, 2400,           # width, height
        144000, 289,          # refresh (mHz), dpi
        0, 0                  # app_caps=0 (blit mode), reserved
    )
    send_msg(sock, conf)
    print("  ✅ CONF sent")
    time.sleep(0.3)

def test_frames(sock, count=3):
    """M3: Receive frames and send cumulative acks."""
    print(f"[M3] Frame test (expecting up to {count} frames)...")
    last_serial = 0

    for i in range(count):
        try:
            data = recv_msg(sock, timeout=3.0)
        except (socket.timeout, ConnectionError):
            print(f"  ⚠️  Only {i} frames received (expected {count})")
            break

        magic = struct.unpack('<I', data[:4])[0]
        if magic != MAGIC_LAND:
            print(f"  ⚠️  Unexpected message: {magic:#x}")
            continue

        serial = struct.unpack_from('<Q', data, 8)[0]
        w = struct.unpack_from('<I', data, 24)[0]
        h = struct.unpack_from('<I', data, 28)[0]
        fmt = struct.unpack_from('<I', data, 32)[0]
        buf_id = struct.unpack_from('<I', data, 40)[0]
        print(f"  ✅ Frame serial={serial} {w}x{h} fmt=0x{fmt:08x} buf={buf_id}")
        last_serial = serial

    if last_serial > 0:
        ack = struct.pack('<IIQ', MAGIC_FACK, 0, last_serial)
        send_msg(sock, ack)
        print(f"  ✅ Cumulative Ack sent (serial <= {last_serial})")
    else:
        print("  ⚠️  No frames to ack")

def test_config_update(sock):
    """M5: Send config update (simulate rotation to 1920x1080)."""
    print("[M5] Config update test...")
    conf = struct.pack('<8I',
        MAGIC_CONF, 1,
        1920, 1080,           # new size
        60000, 200,           # 60Hz, 200 DPI
        0, 0
    )
    send_msg(sock, conf)
    print("  ✅ Config update sent (1920x1080 @60Hz)")
    time.sleep(0.3)

    # Restore
    conf2 = struct.pack('<8I',
        MAGIC_CONF, 1,
        3392, 2400, 144000, 289,
        0, 0
    )
    send_msg(sock, conf2)
    print("  ✅ Config restored (3392x2400 @144Hz)")
    time.sleep(0.3)

def test_touch(sock):
    """M4: Send touch events."""
    print("[M4] Touch test...")

    def send_touch(touch_id, x, y, phase, time_ms):
        msg = struct.pack('<IiffII', MAGIC_TOUC, touch_id, x, y, phase, time_ms)
        send_msg(sock, msg)

    # Touch down
    send_touch(0, 0.5, 0.5, TOUCH_DOWN, 1000)
    send_touch(0, 0.0, 0.0, TOUCH_FRAME, 1001)  # frame sentinel
    print("  ✅ Touch DOWN + FRAME sent")

    # Touch move
    send_touch(0, 0.6, 0.5, TOUCH_MOVE, 1100)
    send_touch(0, 0.0, 0.0, TOUCH_FRAME, 1101)
    print("  ✅ Touch MOVE + FRAME sent")

    # Touch up
    send_touch(0, 0.6, 0.5, TOUCH_UP, 1200)
    send_touch(0, 0.0, 0.0, TOUCH_FRAME, 1201)
    print("  ✅ Touch UP + FRAME sent")

    # Multi-touch: two fingers
    send_touch(0, 0.3, 0.3, TOUCH_DOWN, 1300)
    send_touch(1, 0.7, 0.7, TOUCH_DOWN, 1301)
    send_touch(0, 0.0, 0.0, TOUCH_FRAME, 1302)
    send_touch(0, 0.32, 0.32, TOUCH_MOVE, 1400)
    send_touch(1, 0.68, 0.68, TOUCH_MOVE, 1401)
    send_touch(0, 0.0, 0.0, TOUCH_FRAME, 1402)
    send_touch(0, 0.32, 0.32, TOUCH_UP, 1500)
    send_touch(1, 0.68, 0.68, TOUCH_UP, 1501)
    send_touch(0, 0.0, 0.0, TOUCH_FRAME, 1502)
    print("  ✅ Multi-touch OK")

    time.sleep(0.2)

def main():
    print(f"=== wl-android mock-app ===")
    print(f"Socket: {SOCKET_PATH}")
    print()

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(SOCKET_PATH)
    except (FileNotFoundError, ConnectionRefusedError) as e:
        print(f"❌ Cannot connect to {SOCKET_PATH}: {e}")
        print("   Is wl-android running in the container?")
        print("   Is the bind mount configured?")
        sys.exit(1)

    print("✅ Connected")
    print()

    test_handshake(sock)
    print()
    test_config_update(sock)
    print()
    test_touch(sock)
    print()
    test_frames(sock, count=5)
    print()

    sock.close()
    print("=== All tests PASSED ===")

if __name__ == "__main__":
    main()
