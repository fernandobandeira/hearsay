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
#   1. `web/package.json` - the Vite app itself lives here, so build it.
#   2. `web/dist/` - the build is vendored (scripts/sync-web.sh copies it out of
#      ~/git/narrator/web/dist). This is the current arrangement: the reader is
#      still developed in the python repo, and the VPS must be able to build
#      this image without that repo or a node_modules tree.
#   3. neither - the placeholder page, so the server still answers on /.
#
# Node is only actually needed for (1); the stage is cheap in the other two and
# keeps one place where "where does /web come from" is answered.
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
# image is built for two architectures, sometimes under emulation. GGML_NATIVE=OFF
# makes whisper.cpp a portable baseline build instead - the right trade when the
# thing it renders is a thirty-second voice memo, and the wrong instruction on an
# Ampere Altra is a SIGILL rather than a slow transcript. whisper-rs-sys passes
# every GGML_* variable straight through to cmake.
ARG GGML_NATIVE=OFF
ENV GGML_NATIVE=${GGML_NATIVE}

WORKDIR /src
# Dependency layer: a manifest-only build so a source edit does not re-download
# and re-compile 300 crates (and, more to the point, does not recompile
# whisper.cpp, which is most of the build).
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > src/bin/listen_test.rs \
    && cargo build --release --locked || true \
    && rm -rf src

COPY src/ ./src/
COPY tests/ ./tests/
# Touch so cargo does not reuse the stub's fingerprint.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked --bin narrator \
    && strip target/release/narrator || true

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
