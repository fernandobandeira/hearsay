# GATE 0 — the ONNX engine listening test

`kokoro_onnx_af_heart.wav` is chapter index 1 of *Lord of Mysteries* ("1: Crimson"),
first 33 chunks straight out of `work/audio/01 - Lord of Mysteries/plan.json`,
rendered through this repo's Rust path — espeak-ng phonemes → misaki's
`EspeakFallback` mapping → Kokoro-82M v1.0 fp32 ONNX via `ort` — at 24 kHz mono
s16 with 0.35 s between chunks.

Compare against `~/git/narrator/work/kokoro-test/kokoro_af_heart.wav`, the
PyTorch Kokoro render Fernando accepted when the engine was chosen.

Reproduce: `cargo run --release --bin listen_test`.
