# narrator — Kokoro TTS on CPU, in Rust, with the Vite/React reader.
#
# Three stages. `web` builds the reader with node; `build` compiles the server
# with cargo; the runtime has heard of neither and carries a single static-ish
# binary plus ffmpeg and espeak-ng. No GPU anywhere. Builds for amd64 and arm64
# (the Oracle A1 VPS):
#
#   docker build -t narrator-rs:latest .
#   docker buildx build --platform linux/amd64,linux/arm64 -t narrator-rs:latest .
#
# The build needs network for crates.io *and* for `ort`, which downloads a
# prebuilt ONNX Runtime for the target triple. The voice-memo transcriber is
# the vendored Phonon-2 engine (vendor/phonon-2-c), a few thousand lines of C
# that build.rs compiles with the system cc in seconds.

# ---------------------------------------------------------------- web build
FROM node:22-slim AS web
WORKDIR /build
# Three ways this stage can get a reader, in order of preference:
#
#   1. `web/package.json` - the Vite app itself lives here, so build it. This is
#      the arrangement now: the reader's source is in this repo, so the server
#      image is self-contained and a checkout is all a build needs.
#   2. `web/dist/` - a vendored build, which is what this was before the reader
#      moved in. Nothing produces one today; the branch stays because a tarball
#      of a dist is still a legitimate way to hand this image a reader.
#   3. neither - the placeholder page, so the server still answers on /.
#
# The reader that actually runs on the box comes from the *other* image,
# ghcr.io/fernandobandeira/hearsay-web (see web/Dockerfile), laid into
# /home/ubuntu/web and mounted over this one. What is baked in here is the
# fallback, and the reason `docker run` of this image alone is a whole reader.
COPY web/ ./
RUN set -eux; \
    if [ -f package.json ]; then \
        npm ci --no-audit --no-fund || npm install --no-audit --no-fund; \
        npm run build; \
    elif [ -f dist/index.html ]; then \
        echo "using the vendored reader build"; \
    else \
        mkdir -p dist && cp placeholder/index.html dist/; \
    fi; \
    test -f dist/index.html; \
    ls -la dist

# ------------------------------------------------------------------- build
FROM rust:1-slim-bookworm AS build

# build-essential: the C compiler build.rs drives for the Phonon-2 engine (and
# for rusqlite's bundled SQLite). pkg-config and libssl for the crates that
# link openssl. On aarch64 these are the packages that most often differ - keep
# them explicit.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# The instruction set the engine is compiled for is named in build.rs, never
# -march=native: this image is built on GitHub's runners and run on the A1, and
# the arm64 runners are newer cores than the A1's Neoverse N1. arm64 gets
# armv8.2-a+dotprod (the int8 SDOT kernels, which are the whole speed of the
# thing; `lscpu` on the A1 lists `asimddp`), amd64 gets x86-64-v3. The same
# baseline this image already required for whisper.cpp, so nothing that ran it
# before stops running it now. `--build-arg PHONON_MARCH=native` for a build on
# the machine that will run it.
ARG PHONON_MARCH=
ENV PHONON_MARCH=${PHONON_MARCH}

WORKDIR /src
# Dependency layer: a manifest-only build so a source edit does not re-download
# and re-compile 300 crates. No build.rs here, so the stub links nothing of the
# engine; the C is compiled in the layer below, in seconds.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > src/bin/listen_test.rs \
    && (cargo build --release --locked || true) \
    && rm -rf src

COPY build.rs ./
COPY vendor/ ./vendor/
COPY native/ ./native/
COPY src/ ./src/
# tests/ is deliberately *not* copied. `cargo build --bin narrator` never
# compiles a test target, and every fixture that lands in this stage is another
# way an edit to a test invalidates the final compile layer for nothing. The
# tests run in CI on the runner, against the same source.
# Touch so cargo does not reuse the stub's fingerprint.
RUN set -eux; \
    touch src/main.rs src/lib.rs; \
    cargo build --release --locked --bin narrator; \
    strip target/release/narrator || true

# ----------------------------------------------------------------- runtime
FROM debian:bookworm-slim

# ffmpeg: chapter packing, HLS segmentation, .m4b export and decoding voice
#   memos for the transcriber.
# espeak-ng: Kokoro's grapheme-to-phoneme front end. narrator-rs runs it as a
#   subprocess rather than linking libespeak-ng, so none of the python image's
#   espeakng_loader surgery is needed here - the apt binary and its data are
#   simply what `espeak-ng` on PATH resolves to, and they always match.
# libgomp1: ONNX Runtime's CPU execution provider is OpenMP-threaded.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ffmpeg espeak-ng espeak-ng-data libgomp1 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Fail the image here rather than at the first render if the g2p is missing.
RUN espeak-ng -q --ipa=2 -v en-us "narrator renders books on a cpu." \
    && ffmpeg -version > /dev/null

# Weights and rendered audio live in bind mounts outside the image, so they
# survive rebuilds. NARRATOR_MODELS holds kokoro/ and phonon-2/; see
# scripts/fetch-models.sh for what goes in them.
ENV NARRATOR_MODELS=/models \
    NARRATOR_WORK=/work \
    NARRATOR_BOOKS=/books \
    NARRATOR_WEB=/web \
    KOKORO_VOICE=af_heart \
    RUST_LOG=info,tower_http=warn,ort=warn

COPY --from=web /build/dist /web
COPY --from=build /src/target/release/narrator /usr/local/bin/narrator

EXPOSE 7870
# A liveness probe that only proves the HTTP server is answering is worth
# nothing - /healthz exercises the render thread, forward progress, the packer
# and a writable work dir.
HEALTHCHECK --interval=60s --timeout=10s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/narrator", "--healthcheck"]
CMD ["narrator"]
