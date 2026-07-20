#!/system/bin/sh
# wl-android Magisk module — service.sh
#
# Creates the socket directory for wl-android communication.
# The container (Droidspaces) bind-mounts /data/local/tmp/wl-android/
# to /run/wl-android/ inside the container.
# wl-android (container) binds land.sock; wl-android-app (Android) connects.

SOCKET_DIR=/data/local/tmp/wl-android

# Wait for /data partition to be ready
until [ -d /data ]; do
    sleep 1
done

mkdir -p "$SOCKET_DIR"
chmod 0777 "$SOCKET_DIR"
chown system:system "$SOCKET_DIR"

# Restore SELinux context for the directory
# The sepolicy.rule grants untrusted_app access to this path
restorecon -R "$SOCKET_DIR" 2>/dev/null || true
