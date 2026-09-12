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
# prebuilt ONNX Runtime for the target triple; whisper.cpp is compiled from
# source by whisper-rs-sys, which is why cmake and clang are in the build stage.

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

# cmake + clang: whisper-rs-sys compiles whisper.cpp and binds it with bindgen,
# which needs libclang. pkg-config and libssl for the crates that link openssl.
# On aarch64 these are the packages that most often differ - keep them explicit.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake clang libclang-dev pkg-config \
        libssl-dev ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# ggml compiles for the machine it is *built* on unless told otherwise, and this
# image is built for two architectures, sometimes under emulation - where "the
# machine" is QEMU's idea of a CPU, not the Ampere Altra the result will run on.
# A wrong instruction there is a SIGILL, not a slow transcript, so GGML_NATIVE is
# off and the baseline is named explicitly instead.
#
#   amd64: x86-64-v3 (AVX, AVX2, FMA, F16C) - every Intel/AMD part since ~2013,
#          which both this box and any plausible VPS are. Leaving these off as
#          well costs whisper.cpp several times its speed for nothing.
#   arm64: the ARMv8 baseline, which already mandates NEON. Not armv8.2+dotprod:
#          the A1 has it, but naming it here would make the image A1-only, and a
#          voice memo is not the hot path. Build on the box with
#          `--build-arg GGML_NATIVE=ON` if that transcript ever feels slow.
#
# whisper-rs-sys passes every GGML_* variable straight through to cmake.
ARG TARGETARCH
ARG GGML_NATIVE=OFF
ENV GGML_NATIVE=${GGML_NATIVE}
# One shell snippet, sourced by both cargo invocations below. TARGETARCH comes
# from buildx; dpkg is the fallback for a plain `docker build`.
RUN set -eux; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    if [ "${GGML_NATIVE}" = "OFF" ] && [ "$arch" = "amd64" ]; then \
        echo 'export GGML_AVX=ON GGML_AVX2=ON GGML_FMA=ON GGML_F16C=ON' > /etc/ggml.sh; \
    else \
        echo '# ggml baseline for this arch' > /etc/ggml.sh; \
    fi; \
    cat /etc/ggml.sh

WORKDIR /src
# Dependency layer: a manifest-only build so a source edit does not re-download
# and re-compile 300 crates (and, more to the point, does not recompile
# whisper.cpp, which is most of the build).
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > src/bin/listen_test.rs \
    && sh -c '. /etc/ggml.sh; cargo build --release --locked || true' \
    && rm -rf src

COPY src/ ./src/
# tests/ is deliberately *not* copied. `cargo build --bin narrator` never
# compiles a test target, and every fixture that lands in this stage is another
# way an edit to a test invalidates the final compile layer for nothing. The
# tests run in CI on the runner, against the same source.
# Touch so cargo does not reuse the stub's fingerprint.
RUN set -eux; \
    . /etc/ggml.sh; \
    touch src/main.rs src/lib.rs; \
    cargo build --release --locked --bin narrator; \
    strip target/release/narrator || true

# ----------------------------------------------------------------- runtime
FROM debian:bookworm-slim

# ffmpeg: chapter packing, HLS segmentation, .m4b export and decoding voice
#   memos for whisper.
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
# survive rebuilds. NARRATOR_MODELS holds kokoro/ and whisper/; see
# scripts/fetch-models.sh for what goes in them.
ENV NARRATOR_MODELS=/models \
    NARRATOR_WORK=/work \
    NARRATOR_BOOKS=/books \
    NARRATOR_WEB=/web \
    KOKORO_VOICE=af_heart \
    WHISPER_MODEL=large-v3-turbo-q5_0 \
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
