# hearsay-web — the reader's build, and nothing else.
#
# A data image: no base, no shell, no package manager. It exists so a UI change
# can reach the box without rebuilding the Rust server (which compiles
# whisper.cpp and takes a good part of an hour across two architectures). The box
# pulls this, `docker create`s it, `docker cp`s /dist out and lays the files into
# /home/ubuntu/web, which narrator-rs.service mounts over the reader baked into
# the server image. See deploy/hearsay-web-update.service.
#
#   docker build -t hearsay-web:latest web/
#
# Built for amd64 *and* arm64 even though the payload is architecture-free: the
# node stage is pinned to $BUILDPLATFORM, so it runs once, natively, and the two
# manifests differ only in metadata. That costs nothing and spares whoever runs
# this on the arm64 box a --platform flag and a "requested image's platform does
# not match" warning on every pull.

FROM --platform=$BUILDPLATFORM node:22-slim AS build
WORKDIR /build
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY . .
RUN set -eux; npm run build; test -f dist/index.html; ls -la dist

FROM scratch
LABEL org.opencontainers.image.title="hearsay-web" \
      org.opencontainers.image.description="The HearSay reader, built. Contents only: /dist." \
      org.opencontainers.image.source="https://github.com/fernandobandeira/hearsay" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /build/dist /dist
