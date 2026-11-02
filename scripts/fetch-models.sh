#!/usr/bin/env bash
# Download the weights narrator needs into $NARRATOR_MODELS (default ./models).
#
# Two models, both CPU:
#   kokoro/model.onnx + voices/*.bin  — Kokoro-82M v1.0, fp32 ONNX export
#   phonon-2/weights.{bin,json} + tokens.txt — Phonon-2 (Parakeet-TDT 0.6B v3,
#                                       five-value), converted for the vendored
#                                       C engine; the voice-memo transcriber
#
# Nothing here is in git: the weights are ~600 MB and the repo is not a CDN.
set -euo pipefail

root="${NARRATOR_MODELS:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/models}"
voice="${KOKORO_VOICE:-af_heart}"
kokoro_repo="https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main"
# The converted weights vendor/phonon-2-c's engine reads, pinned by sha256 to
# the archive its upstream pins (scripts/get_weights.sh there). The same three
# files can be rebuilt from FermionResearch/Phonon-2's own release with that
# repo's convert_weights.py; this is the download, not the only source.
phonon_url="https://huggingface.co/eschmidbauer/phonon-2-c/resolve/main/phonon-2-c-weights-v1.tar.gz"
phonon_sha256="48137a58011c4b0bc0a97087ea7e868daabf1525ca8447414be230b21faf99e6"

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

# Phonon-2: an archive, checked before anything is unpacked, and unpacked
# beside the destination then renamed into place, so an interrupted run never
# leaves a directory that looks like weights.
phonon="$root/phonon-2"
if [ -s "$phonon/weights.bin" ]; then
  echo "have phonon-2"
else
  echo "fetching phonon-2"
  mkdir -p "$root"
  stage="$(mktemp -d "$root/.phonon-2.XXXXXX")"
  trap 'rm -rf "$stage"' EXIT
  curl -fL --progress-bar -o "$stage/w.tar.gz" "$phonon_url"
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$stage/w.tar.gz" | cut -d' ' -f1)"
  else
    got="$(shasum -a 256 "$stage/w.tar.gz" | cut -d' ' -f1)"
  fi
  if [ "$got" != "$phonon_sha256" ]; then
    echo "phonon-2: sha256 $got, expected $phonon_sha256; nothing unpacked" >&2
    exit 1
  fi
  tar xzf "$stage/w.tar.gz" -C "$stage"
  rm -rf "$phonon"
  mv "$stage/c_weights" "$phonon"
fi

echo
du -sh "$root"/* 2>/dev/null || true
