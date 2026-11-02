//! Compiles the vendored Phonon-2 engine (`vendor/phonon-2-c`) and its shim
//! (`native/phonon_shim.c`) into the binary, when the `stt` feature is on.
//!
//! **The instruction set is named, never `-march=native`.** Upstream builds for
//! the machine it is built on, and this image is built on GitHub's runners and
//! run on the A1. Those are different CPUs: the arm64 runners are newer Neoverse
//! cores than the A1's Ampere Altra (Neoverse N1), so "native" there can mean
//! instructions the box does not have, and that is a SIGILL in the middle of a
//! voice memo rather than a slow one.
//!
//! - aarch64: `armv8.2-a+dotprod`. The engine's fast path is int8 `SDOT`
//!   (`vdotq_laneq_s32`), compiled in only under `__ARM_FEATURE_DOTPROD`, so
//!   without `+dotprod` it silently falls back to fp32 at a quarter of the speed.
//!   The A1 lists `asimddp` in `lscpu`; so does every Apple silicon Mac. The
//!   server image already required exactly this baseline for whisper.cpp.
//! - x86_64: `x86-64-v3` (AVX2 + FMA), which the engine's x86 kernels test for.
//!   Every Intel/AMD part since ~2013, and the same bet the image made before.
//!
//! `PHONON_MARCH` overrides it (`PHONON_MARCH=native` for a build on the machine
//! that will run it).
//!
//! `-ffast-math` is upstream's flag and is kept: the engine is verified token for
//! token against the transformers reference with it. It only reaches these
//! translation units — the cc crate compiles, rustc links, and nothing passes
//! `-ffast-math` to the link that would install crtfastmath's FTZ/DAZ mode for
//! the whole process.

use std::path::Path;

const SOURCES: &[&str] = &[
    "blas.c",
    "threadpool.c",
    "tensor_ops.c",
    "encoder.c",
    "decoder.c",
    "mel.c",
    "weights.c",
    "tokens.c",
    "wav.c",
    "resample.c",
    "segment.c",
    "engine.c",
    "json.c",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native/phonon_shim.c");
    println!("cargo:rerun-if-changed=vendor/phonon-2-c/csrc");
    println!("cargo:rerun-if-env-changed=PHONON_MARCH");
    if std::env::var_os("CARGO_FEATURE_STT").is_none() {
        return;
    }

    let csrc = Path::new("vendor/phonon-2-c/csrc");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let march = std::env::var("PHONON_MARCH")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .or(match arch.as_str() {
            "aarch64" => Some("armv8.2-a+dotprod".to_string()),
            "x86_64" => Some("x86-64-v3".to_string()),
            _ => None,
        });

    let mut b = cc::Build::new();
    b.include(csrc)
        .files(SOURCES.iter().map(|f| csrc.join(f)))
        .file("native/phonon_shim.c")
        .std("c11")
        .opt_level(3)
        .flag("-ffast-math")
        .flag("-funroll-loops")
        .flag("-pthread")
        // Upstream builds with -Wall -Wextra and is clean there; its warnings are
        // its own business, and a vendored copy is never edited to silence them.
        .warnings(false);
    if let Some(m) = march {
        b.flag(format!("-march={m}"));
    }
    // glibc hides clock_gettime, sysconf and mmap under -std=c11 unless asked;
    // macOS shows everything (upstream's Makefile does the same).
    if os != "macos" {
        b.define("_GNU_SOURCE", None);
    }
    b.compile("phonon2");

    println!("cargo:rustc-link-lib=m");
    if os != "macos" {
        println!("cargo:rustc-link-lib=pthread");
    }
}
