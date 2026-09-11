#!/usr/bin/env bash
# Restart narrator-rs after three consecutive unhealthy checks, with a floor
# between restarts so a crash loop cannot hide the problem.
#
# /healthz is not a liveness probe: it 503s when the render thread has died or
# has not produced a chunk in HEALTH_STALL_S, which is the failure that is
# otherwise noticed hours later, as silence.
set -euo pipefail
STATE=/var/lib/narrator-rs/watchdog
FLOOR=900           # seconds between restarts
LIMIT=3             # consecutive failures before acting
URL=http://127.0.0.1:7870/healthz

mkdir -p "$STATE"
fails=$(cat "$STATE/fails" 2>/dev/null || echo 0)
last=$(cat "$STATE/last-restart" 2>/dev/null || echo 0)
now=$(date +%s)

if curl -fsS --max-time 10 "$URL" >/dev/null 2>&1; then
  echo 0 > "$STATE/fails"
  exit 0
fi

fails=$((fails + 1))
echo "$fails" > "$STATE/fails"
logger -t narrator-rs-watchdog "healthz failed ($fails/$LIMIT)"
[ "$fails" -ge "$LIMIT" ] || exit 0
if [ $((now - last)) -lt "$FLOOR" ]; then
  logger -t narrator-rs-watchdog "restart suppressed: only $((now - last))s since the last one"
  exit 0
fi
echo "$now" > "$STATE/last-restart"
echo 0 > "$STATE/fails"
logger -t narrator-rs-watchdog "restarting narrator-rs"
systemctl restart narrator-rs
