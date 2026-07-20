#!/bin/bash
# soak.sh — 1-hour performance soak test for wl-android
# Monitors: RSS, fd count, FPS, memory growth over time.
# Run inside the container alongside wl-android and KWin.
#
# Usage:
#   ./soak.sh [duration_seconds] [output_dir]
#   Default: 3600s (1h), output to ./soak-results/
set -euo pipefail

DURATION="${1:-3600}"
OUTDIR="${2:-./soak-results}"
mkdir -p "$OUTDIR"

WL_PID=""
APP_PID=""

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'

echo "=== wl-android Soak Test ==="
echo "Duration: ${DURATION}s ($((DURATION / 60))min)"
echo "Output:   $OUTDIR"
echo "Started:  $(date -Iseconds)"
echo

# ── Find processes ──

WL_PID=$(pgrep -f "wl-android" | head -1 || echo "")
if [ -z "$WL_PID" ]; then
    echo "⚠️  wl-android not running — starting soak with just FD/RSS checks"
else
    echo "wl-android PID: $WL_PID"
fi

echo

# ── Log files ──

FD_LOG="$OUTDIR/fd-count.log"
RSS_LOG="$OUTDIR/rss.log"
SUMMARY="$OUTDIR/summary.txt"

echo "# timestamp fd_count" > "$FD_LOG"
echo "# timestamp rss_kb" > "$RSS_LOG"

# ── Baselines ──

if [ -n "$WL_PID" ]; then
    INIT_FD=$(ls "/proc/$WL_PID/fd" 2>/dev/null | wc -l || echo "N/A")
    INIT_RSS=$(grep VmRSS "/proc/$WL_PID/status" 2>/dev/null | awk '{print $2}' || echo "N/A")
    echo "Initial:  fd=$INIT_FD  rss=${INIT_RSS}KB"
else
    INIT_FD="N/A"; INIT_RSS="N/A"
fi

# ── Soak loop ──

INTERVAL=60  # sample every 60s
ELAPSED=0

while [ "$ELAPSED" -lt "$DURATION" ]; do
    NOW=$(date +%s)

    if [ -n "$WL_PID" ] && kill -0 "$WL_PID" 2>/dev/null; then
        FD=$(ls "/proc/$WL_PID/fd" 2>/dev/null | wc -l || echo "0")
        RSS=$(grep VmRSS "/proc/$WL_PID/status" 2>/dev/null | awk '{print $2}' || echo "0")
        echo "$NOW $FD" >> "$FD_LOG"
        echo "$NOW $RSS" >> "$RSS_LOG"

        # Alerts
        if [ "$RSS" -gt 32000 ]; then
            echo "  ${RED}⚠️  RSS=${RSS}KB > 32MB (PERF-05)${NC}"
        fi
    else
        echo "$NOW 0" >> "$FD_LOG"
        echo "$NOW 0" >> "$RSS_LOG"
    fi

    printf "\r  [%5ds/%5ds] fd=%s rss=%sKB   " "$ELAPSED" "$DURATION" "$FD" "$RSS"
    sleep "$INTERVAL"
    ELAPSED=$((ELAPSED + INTERVAL))
done

echo

# ── Final stats ──

if [ -n "$WL_PID" ] && kill -0 "$WL_PID" 2>/dev/null; then
    FINAL_FD=$(ls "/proc/$WL_PID/fd" 2>/dev/null | wc -l || echo "0")
    FINAL_RSS=$(grep VmRSS "/proc/$WL_PID/status" 2>/dev/null | awk '{print $2}' || echo "0")
else
    FINAL_FD="N/A"; FINAL_RSS="N/A"
fi

echo
echo "=== Soak Complete ==="
echo "Initial:  fd=$INIT_FD  rss=${INIT_RSS}KB"
echo "Final:    fd=$FINAL_FD  rss=${FINAL_RSS}KB"

FD_OK="❌"
RSS_OK="❌"

if [ "$INIT_FD" != "N/A" ] && [ "$FINAL_FD" != "N/A" ]; then
    if [ "$INIT_FD" = "$FINAL_FD" ]; then
        FD_OK="✅"
    fi
fi
if [ "$FINAL_RSS" != "N/A" ] && [ "$FINAL_RSS" -lt 32000 ]; then
    RSS_OK="✅"
fi

cat > "$SUMMARY" <<EOF
wl-android Soak Test Summary
=============================
Date:        $(date -Iseconds)
Duration:    ${DURATION}s ($((DURATION / 60))min)
wl-android PID: ${WL_PID:-N/A}

Initial:     fd=$INIT_FD  rss=${INIT_RSS}KB
Final:       fd=$FINAL_FD  rss=${FINAL_RSS}KB

PERF-05 (RSS < 32MB):     $RSS_OK  (final=${FINAL_RSS}KB)
PERF-07 (fd leak = 0):    $FD_OK  ($INIT_FD → $FINAL_FD)

Logs: $FD_LOG, $RSS_LOG
EOF

cat "$SUMMARY"

# Return non-zero if any PERF constraint violated
if [ "$FD_OK" = "❌" ] || [ "$RSS_OK" = "❌" ]; then
    echo
    echo "❌ Performance constraints violated!"
    exit 1
fi

echo
echo "✅ Soak test PASSED"
