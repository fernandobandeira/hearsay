#!/usr/bin/env bash
# Download the weights narrator needs into $NARRATOR_MODELS (default ./models).
#
# Two models, both CPU:
#   kokoro/model.onnx + voices/*.bin  — Kokoro-82M v1.0, fp32 ONNX export
#   whisper/ggml-*.bin                — whisper.cpp, quantized, plus a VAD model
#
# Nothing here is in git: the weights are ~900 MB and the repo is not a CDN.
set -euo pipefail

root="${NARRATOR_MODELS:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/models}"
voice="${KOKORO_VOICE:-af_heart}"
whisper="${WHISPER_MODEL:-large-v3-turbo-q5_0}"
kokoro_repo="https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main"
whisper_repo="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"
vad_repo="https://huggingface.co/ggml-org/whisper-vad/resolve/main"

get() { # url dest
  if [ -s "$2" ]; then echo "have $(basename "$2")"; return; fi
  echo "fetching $(basename "$2")"
  mkdir -p "$(dirname "$2")"
  curl -fL --progress-bar -o "$2.part" "$1"
  mv "$2.part" "$2"
}

get "$kokoro_repo/onnx/model.onnx"        "$root/kokoro/model.onnx"
get "$kokoro_repo/tokenizer.json"         "$root/kokoro/tokenizer.json"
get "$kokoro_repo/voices/$voice.bin"      "$root/kokoro/voices/$voice.bin"
# A second voice, so KOKORO_VOICE can be changed without another download.
get "$kokoro_repo/voices/am_michael.bin"  "$root/kokoro/voices/am_michael.bin"
get "$whisper_repo/ggml-$whisper.bin"     "$root/whisper/ggml-$whisper.bin"
get "$vad_repo/ggml-silero-v5.1.2.bin"    "$root/whisper/ggml-silero-v5.1.2.bin"

echo
du -sh "$root"/* 2>/dev/null || true
