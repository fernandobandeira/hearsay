# phonon-2-c, vendored

The library half of <https://github.com/eschmidbauer/phonon-2-c> at commit
`c4b860f6ad745369d6f21599d6e23d381c1fcc7a` (2026-10-09, version 0.1.0): a pure-C
inference engine for [Phonon-2](https://huggingface.co/FermionResearch/Phonon-2),
Fermion Research's five-value re-training of NVIDIA's Parakeet-TDT 0.6B v3.

`csrc/` is copied **unchanged**, minus what this server does not link: the CLI
(`main.c`), the GEMM benchmark, the Python bindings, the converter and the test
data. `build.rs` compiles it with the `cc` crate and `native/phonon_shim.c` is
the only thing that calls it. To update, copy the same files from a newer commit
and change the hash above; never edit them here.

Licences: the engine is MIT (`LICENSE`); the weights it runs are CC-BY-4.0 and
are not in this repository (see `NOTICE`, and `scripts/fetch-models.sh`, which
downloads them).
