#!/usr/bin/env bash
# Lay the newest reader build into /home/ubuntu/web, which narrator-rs mounts
# read-only over the copy baked into the server image.
#
# The reader is published as its own image - ghcr.io/fernandobandeira/hearsay-web,
# a `FROM scratch` image whose entire filesystem is /dist - so a UI change is a
# pull and a file copy rather than an hour of recompiling whisper.cpp for two
# architectures. This script is the whole deploy: pull, compare the digest to a
# stamp, and if it moved, copy the files out of the image and into place.
#
# Three things it must not do:
#
#   * Fail when the box is offline. A registry it cannot reach is not an error
#     worth a red unit in `systemctl status`; the box keeps serving what it has.
#   * Install an empty reader. /home/ubuntu/web shadows the image's copy
#     unconditionally - the server does not fall back - so an empty directory is
#     a blank page, not a default. Nothing is copied unless index.html is there.
#   * Replace the *directory*. It is a bind mount source: the running container
#     holds the inode it was started with, so swapping in a new directory would
#     leave the server serving files nobody can see any more. The contents are
#     synced in place instead, and rsync renames each file into position, so a
#     request landing mid-update gets a whole file either way.
set -euo pipefail

IMAGE="${HEARSAY_WEB_IMAGE:-ghcr.io/fernandobandeira/hearsay-web:latest}"
DEST="${HEARSAY_WEB_DIR:-/home/ubuntu/web}"
OWNER="${HEARSAY_WEB_OWNER:-ubuntu:ubuntu}"
STATE=/var/lib/hearsay
STAMP="$STATE/web.digest"

log() { logger -t hearsay-web-update -s -- "$*"; }

mkdir -p "$STATE" "$DEST"

# A registry hiccup, a rebooting box, a dropped tunnel: all the same answer.
if ! docker pull --quiet "$IMAGE" >/dev/null 2>&1; then
  if docker image inspect "$IMAGE" >/dev/null 2>&1; then
    log "pull failed; using the image already on the box"
  else
    log "pull failed and no local copy - nothing to do"
    exit 0
  fi
fi

# The local image id: always present, and it changes exactly when the contents
# do. (RepoDigests would name the manifest, but it is empty for an image that
# was loaded rather than pulled.)
digest="$(docker image inspect "$IMAGE" --format '{{.Id}}' 2>/dev/null || true)"
[ -n "$digest" ] || { log "cannot read the image id - nothing to do"; exit 0; }

if [ "$digest" = "$(cat "$STAMP" 2>/dev/null || true)" ]; then
  exit 0
fi

stage="$(mktemp -d "$STATE/stage.XXXXXX")"
cid=""
cleanup() {
  [ -n "$cid" ] && docker rm -f "$cid" >/dev/null 2>&1 || true
  rm -rf "$stage"
}
trap cleanup EXIT

# Never started, so `scratch` having no entrypoint does not matter: `docker cp`
# reads the container's filesystem, not a running process.
cid="$(docker create "$IMAGE" /nonexistent)"
docker cp "$cid:/dist/." "$stage/"

if [ ! -f "$stage/index.html" ]; then
  log "the image has no index.html - refusing to install it"
  exit 1
fi

chown -R "$OWNER" "$stage"
chmod -R a+rX "$stage"
# Two passes: everything new lands first, and only then is what is no longer
# referenced removed. A reader that fetches an asset during the update finds it.
rsync -a "$stage/" "$DEST/"
rsync -a --delete "$stage/" "$DEST/"

printf '%s\n' "$digest" > "$STAMP"
log "installed $(find "$DEST" -type f | wc -l) files from $IMAGE ($digest)"
