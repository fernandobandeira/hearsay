#!/usr/bin/env bash
# Restart narrator-rs onto the newest server image, if there is one.
#
# release.yml publishes ghcr.io/fernandobandeira/hearsay:latest on every green
# push to master, and narrator-rs.service already pulls :latest in its
# ExecStartPre - so a deploy has always been "restart the unit". This script is
# the thing that decides to: pull, compare the image the running container was
# started from with the one :latest names now, and restart only when they differ.
#
# Pull-based on purpose. The alternative is CI reaching in, which on this box
# means a Tailscale auth key and an SSH key in GitHub secrets, a tag in the
# tailnet ACL exempt from Tailscale SSH's browser check, and a public repo's
# workflow holding a way into the machine that has the vault on it. Pulling
# needs none of that: the image is public, the box asks, nothing gets in.
#
# What it must not do:
#
#   * Fail when the box is offline. A registry it cannot reach logs a line and
#     exits 0; the box keeps running what it has.
#   * Start a server that is not running. A stopped unit is somebody's decision
#     (or the watchdog's to make); this only ever *replaces* a running one.
#   * Worry about a memo mid-transcription. A restart is safe for it: every
#     process finishes the memo the last one was killed during
#     (notes::resume_unfiled), and the phone keeps it until a 2xx anyway.
set -euo pipefail

IMAGE="${HEARSAY_IMAGE:-ghcr.io/fernandobandeira/hearsay:latest}"
UNIT="${HEARSAY_UNIT:-narrator-rs}"
CONTAINER="${HEARSAY_CONTAINER:-narrator-rs}"

log() { logger -t hearsay-server-update -s -- "$*"; }

if ! systemctl is-active --quiet "$UNIT"; then
  exit 0
fi

if ! docker pull --quiet "$IMAGE" >/dev/null 2>&1; then
  log "pull failed; leaving $UNIT on the image it has"
  exit 0
fi

want="$(docker image inspect "$IMAGE" --format '{{.Id}}' 2>/dev/null || true)"
have="$(docker inspect "$CONTAINER" --format '{{.Image}}' 2>/dev/null || true)"
# The unit is active but the container is not up yet (ExecStartPre is pulling,
# or it is mid-restart): the start in progress will pick up :latest by itself.
[ -n "$want" ] && [ -n "$have" ] || exit 0
[ "$want" = "$have" ] && exit 0

log "restarting $UNIT: ${have:7:12} -> ${want:7:12}"
systemctl restart "$UNIT"
# Old images are a few hundred MB each on a box with a 50 GB audio cache.
docker image prune -f >/dev/null 2>&1 || true
