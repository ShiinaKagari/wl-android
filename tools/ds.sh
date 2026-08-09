#!/usr/bin/env bash
# Droidspaces container helper for wl-android debugging.
#
# Standardized access to the Android-side droidspaces container (arch).
# Authorized method per user directive:
#   1. adb shell
#   2. su
#   3. /data/local/Droidspaces/bin/droidspaces help
#   4. droidspaces enter kagari --name=arch   (interactive shell)
#   5. droidspaces run <cmd> [args] --name=arch  (one-shot)
#
# NOTE (verified 2026-08-09): with a single running container, `run`
# without --name works and does NOT leak the flag into the command.
# Container name: arch (pid of init = systemd).

DS="/data/local/Droidspaces/bin/droidspaces"
CONTAINER="arch"

# One-shot: run a command inside the container, print output.
# Usage: dsrun <cmd> [args...]
dsrun() {
    adb shell "su -c '$DS run $(printf '%q ' "$@")'" 2>&1
}

# Interactive: attach to the container with a login shell as `kagari`.
# Usage: dsenter
dsenter() {
    adb shell "su -c '$DS enter kagari --name=$CONTAINER'" 2>&1
}

# Container process snapshot (grep-able).
# Usage: dsps [pattern]
dsps() {
    dsrun ps aux | grep -E "${1:-.}"
}

# Last N lines of the plasma/plasmashell log.
# Usage: dslog [n]
dslog() {
    dsrun tail -n "${1:-80}" /tmp/plasma.log
}

# Container memory picture.
# Usage: dsmem
dsmem() {
    dsrun sh -c 'free -m; echo ---; cat /sys/fs/cgroup/memory.max 2>/dev/null; cat /sys/fs/cgroup/memory.current 2>/dev/null; echo ---; ps aux --sort=-rss | head -8'
}

"$@"
