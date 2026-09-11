# narrator-rs — repo instructions

This file is `AGENTS.md`; `CLAUDE.md` is a symlink to it (Fernando's convention — edit this file, never break the symlink).

The **Rust rewrite of narrator** (`~/git/narrator`, Python/FastAPI). Same HTTP contract, same `work/` cache, same writes into the Obsidian vault — so the two are interchangeable in front of the same machine state. Kokoro-82M on CPU through ONNX Runtime, whisper.cpp for voice memos, axum + tokio, one binary.

**Why it exists.** Fernando's motivation is stability, not speed: the reader must never crash and must heal itself. So the type system is doing real work here — every failure path is a typed error with a caller that logs it and degrades, there is no `unwrap()`/`expect()` outside tests and startup, and the render worker derives its bookkeeping from the filesystem rather than from a counter it keeps in its head (see [the disk-truth invariant](#the-disk-truth-invariant), which is a bug the Python version still has).

> The Python repo is the **reference**. Read `~/git/narrator/AGENTS.md` before changing anything here; its "The HTTP API — treat as a frozen contract" section is the spec this crate implements. Never edit that repo from this one.

## Layout

| Path | What it is |
|---|---|
| `src/book.rs` | EPUB → render plan. A **bug-for-bug** port of `app/book.py`: spine order, sentence-aware chunking at `max_chars` 300, `is_speakable()` silent beats. See [chunking parity](#chunking-parity-the-one-that-cannot-move). |
| `src/tts/g2p.rs` | Text → Kokoro phonemes: espeak-ng as a **subprocess**, then misaki's `EspeakFallback` character mapping verbatim. |
| `src/tts/kokoro.rs` | Kokoro-82M v1.0 fp32 ONNX through `ort`. The 178-symbol vocabulary is inlined as model contract; the voice pack is 510 style vectors indexed by phoneme count. |
| `src/tts/mod.rs` | `Engine`: lazy load, never fatal, plus the deterministic fake (`NARRATOR_FAKE_TTS=1`) the whole test suite renders with. |
| `src/stt.rs` | whisper.cpp via `whisper-rs`: `large-v3-turbo-q5_0`, CPU, one transcription at a time, ffmpeg decoding the webm, silero VAD when its model is present. |
| `src/cache.rs` | `work/audio/<key>/chNNN/IIIII.wav` and `gc_audio`. |
| `src/chapters.rs` | Chapter packing (AAC m4a + chunk→second manifest), lazy HLS, the chapter gc. |
| `src/plancache.rs` | The parse cache: `plan.json` plus a `parse.json` stamp, so `/api/load` does not re-parse a book that has not changed. |
| `src/text.rs` | The two-tier offline text bundle: `index.json` + byte-budgeted shards. |
| `src/vault.rs` | `.narrator-positions.json`, `Reading Log.md`, fleeting notes — all byte-identical to the Python output. |
| `src/render.rs` | The render worker and the packer, one OS thread each. |
| `src/events.rs`, `src/api/stream.rs` | The SSE bus and `/api/events`. |
| `src/state.rs` | One global session, exactly like Python's process-wide `S`. |
| `src/api/` | Every endpoint, with typed request/response structs that **generate** the OpenAPI document. |
| `src/watch.rs` | The library watcher (`notify`), which turns "the vault's git sync pulled an epub onto the server" into a `books` event. |
| `tests/` | Five parity suites, the reader-requirement suite, and a harness that runs a whole server in a temp dir. |
| `scripts/golden/` | Generates the Python golden fixtures (uv + ebooklib + bs4) the parity tests assert against. |
| `scripts/gen-client.sh`, `scripts/drift-check.mjs` | OpenAPI → typed TS client, and the drift gate against the reader's hand-written types. |
| `listen-test/` | GATE 0: the ONNX engine rendered against the PyTorch render Fernando accepted. |
| `web/dist/` | **The built reader, vendored.** The Vite/React app is still developed in `~/git/narrator/web`; `./narrator sync-web` copies its `dist/` here so the image can be built on the VPS without that repo or a node tree. `web/placeholder/` is the fallback page when there is no build at all. |
| `deploy/` | systemd templates for the Oracle A1 — **applied by hand, never by a playbook**, like the python repo's. |

## Running it

```bash
./narrator models        # fetch Kokoro + whisper weights into ./models (~900 MB)
./narrator dev           # cargo run, serving ./web and ./work
./narrator build && ./narrator up     # docker, port 7870 on localhost only
./narrator test          # the whole suite: 86 tests, no model, no network
./narrator lint          # rustfmt --check + clippy -D warnings
./narrator client        # regenerate the TS client and run the drift check
./narrator listen-test   # re-render the GATE 0 passage and report RTF
```

`cargo test` needs neither weights nor network: `NARRATOR_FAKE_TTS` swaps in a deterministic tone as long as the real thing would be, which is how a chapter renders and packs in milliseconds.

## Parity guarantees

Everything below is asserted by a test. "Golden" means the fixture was produced by running the Python implementation, not by blessing this one's output.

### Chunking: the one that cannot move

A chunk index *is* a reading position. It names a wav on disk, an entry in a chapter manifest, a line in the vault's `Reading Log.md`, and the passage a voice note points back to. Move one boundary and Fernando's place in a 1433-chapter book silently relocates and every packed chapter in the cache becomes a lie. So `src/book.rs` reproduces the Python exactly, **including two bugs**:

- **The abbreviation guards are inert.** `_ABBR` puts lookbehinds like `(?<!\bDr)` *before* `(?<=[.!?])`, so they test the two characters ending at the split point — which are `r.`, never `Dr`. All ten guards do nothing and "Dr. Smith" splits in two.
- **The split eats a closing quote.** `["'”’)\]]*` sits inside the separator, so `He said "go." Then` loses the `"`.

Fixing either is a migration, not an edit: it invalidates every rendered chunk, every packed chapter and every stored position. Three things Rust does *not* get for free are also handled: `len()` counts characters, Python's `\s` is wider than `char::is_whitespace`, and `str.isalnum()` includes the `Nl`/`No` categories — that last one decides whether a footnote marker like `²` is a silent beat.

Verified against `7 Powers (2016).epub` copied out of the vault: 22 chapters, 1577 chunks, 237 400 characters, identical chapter ids, titles, counts and first/last chunk text, with chapters 0–2 identical chunk for chunk.

And, the check that actually settles it — `tests/parity_chunking.rs`'s opt-in
`a_python_written_plan_matches_chunk_for_chunk` re-chunks whatever books a real
narrator working tree already has a `plan.json` for and compares against the
file **the Python server itself wrote**. Run against `~/git/narrator/work`:

```
01 - Lord of Mysteries: 1433 chapters, 118831 chunks, identical
```

Every chapter id, every title, every chunk's text, paragraph index and silent
flag. That is the whole cache and every stored position in Fernando's largest
book, proven to survive the swap. It is opt-in because it needs the epub still
sitting next to the plan; `NARRATOR_REF_WORK` and `NARRATOR_REF_BOOKS` point it
somewhere else, and it only ever reads (the epub is copied out before parsing).

### Cache layout

`work/audio/<stem[:50]>/chNNN/IIIII.wav`, 24 kHz mono s16le; `plan.json` beside the chapter directories; `chapters/<key>/chNNN.{m4a,json}`; `hls/<key>/chNNN/`; `text/<key>/{index,NNN}.json`. **A Rust deploy adopts the existing cache in place** — a test seeds a Python-shaped cache and asserts nothing is re-rendered.

`gc_audio` trims to 90 % of `MAX_AUDIO_GB`, oldest first, and never touches the current chapter, its prerender span, or any chapter the manager is rendering, flagged for packing, queued to pack, or packing right now — chunk wavs are the *input* to a pack, and evicting them leaves a chapter permanently one hole short of packable.

One deliberate improvement: chunks are written to a `.part` and renamed. The Python server writes in place, so a killed container leaves a truncated wav that looks rendered.

### Vault writes

`.narrator-positions.json` is written the way `json.dumps(pos, indent=1)` writes it — one-space indent, `": "` after keys, and **non-ASCII escaped**, which is `ensure_ascii=True`'s doing and the part `serde_json` would otherwise get wrong. `Reading Log.md` is byte-identical, unescaped table pipes included: a `|` in a book stem genuinely breaks the row, and matching the Python matters more than a prettier table, because anything else is churn in every `vault backup:` commit. Fleeting notes match byte for byte, deep link and all. Positions are throttled to 15 s and forced on open/pause.

### The disk-truth invariant

**The bug this rewrite was asked to fix.** The Python worker advances `render_idx` and treats it as the record of what has been rendered. It is not: `gc_audio` can delete a chunk behind the frontier, a forward `/api/playhead` jump drags `render_idx` past holes, and a container restart resets it. In each case the chunk the reader is *waiting on* never gets rendered, because the bookkeeping has already moved past it, and the reader sits on a 404 forever with a renderer that believes it is up to date.

Here `render_idx` is a **hint** and the filesystem is the truth:

1. If the chunk under the playhead is missing, it is rendered first. Always.
2. Otherwise the worker scans forward from the hint for a chunk that is genuinely absent.
3. `/api/open` sets the playhead, so rule 1 guarantees the opened chunk renders, whatever the cache looks like.

Tested three ways: a cache with holes opened at a missing chunk, a hole punched *behind* a completed frontier, and a forward jump past the frontier.

### The API

Every path, method, field name, type and nullability of the Python contract, asserted against `tests/fixtures/api_contract.json` — a transcription of `AGENTS.md` + `app/server.py` + `web/src/lib/types.ts`, written down rather than generated here, so it can catch *this* server drifting.

25 paths. `/api/events` (SSE) is implemented over a tokio broadcast channel with the same wire format, the same coalescing of `progress` and the same refusal to replay. `/healthz` returns 503 with `problems[]` for the failures that actually happen: a dead render thread, a "rendering" status with no chunk in `HEALTH_STALL_S`, an unwritable work dir.

## What this server does that the Python one does not

`~/git/narrator/web/RUST-NOTES.md` is a list of requirements the person porting
the reader wrote against the Python server, each with the client-side mitigation
standing in for it meanwhile. All six are implemented here, all six are
**additive** — a client that sends none of the new parameters gets exactly the
Python behaviour, which is what keeps the Obsidian plugin working untouched —
and each has a test in `tests/reader_requirements.rs`.

1. **`/api/load` does not re-parse an unchanged book.** Measured at 12.4 s per
   call on the 1433-chapter *Lord of Mysteries*, and the reading position comes
   back in that response, so first paint on a new device waited for all of it.
   The plan was already being written to disk for `narrator export`; what was
   missing was a way to know it is still valid, which is the `parse.json` stamp
   beside it — source path, size, mtime, `max_chars`, format version. All match,
   and the plan is read back instead of rebuilt; anything differs and the book is
   parsed and the text bundle rebuilt exactly as before. Because the stamp is on
   disk, a container restart is fast too, not just a second call.
2. **`/api/chapter/{ci}` honours `?book=`.** The Python one answers for whichever
   book the process last loaded and ignores the query — the one asymmetry that
   coupled the reader's first paint to `/api/load` at all. Here a key that is not
   the loaded book is served straight out of that book's text bundle, one shard,
   no session.
3. **A returned position carries `updated_ms`.** `updated` stays the naive local
   stamp, byte-identical, because that is what goes in the vault; the instant
   rides along beside it, resolved here where the server's zone is actually
   known.
4. **`/api/open` and `/api/playhead` take an optional `book`.** Supplied and
   mismatched, they answer **409** instead of dragging another book's render
   frontier around and filing a position under the wrong name.
5. **An unwritable positions directory is a health problem.** `/healthz` reports
   `vault` and `positions_dir` and probes the latter. The failure this prevents
   is the nastiest one in the list: with `NARRATOR_VAULT` pointing at a path that
   does not exist, every write fails, every read returns `{}`, and every book
   quietly opens at chapter one.
6. **`/api/chapters` takes `?from=&to=`.** Inclusive, clamped, with `total`
   alongside so a windowed response still says how big the book is.

## OpenAPI is the contract

Handlers carry `#[utoipa::path]` and every request/response is a typed struct, so the document is **generated from the code**, not maintained beside it.

```bash
./target/release/narrator --openapi > openapi.json   # no server, no work dir
./scripts/gen-client.sh                              # spec → client/ → drift check
```

`scripts/gen-client.sh` runs `@hey-api/openapi-ts` (pinned in `scripts/client/package.json`) into `client/`, then `scripts/drift-check.mjs` compares the generated types field by field against `~/git/narrator/web/src/lib/types.ts` using the TypeScript compiler's own assignability. It exits non-zero on a **MISMATCH** — something that would break the reader — and merely reports additive fields.

That check earned its keep on its first run: 29 mismatches, all real. utoipa renders `Option<T>` as both nullable *and* absent from `required`, while serde without `skip_serializing_if` always sends the key — so the spec was lying about eleven `Status` fields. Fixed with `#[schema(required = true)]` and `value_type` overrides on the fields the server genuinely always sends. **The rule: if serde will always serialize it, say so in the schema.** Current state: 0 mismatches, 0 missing endpoints, 3 additive fields.

Five paths carry a file extension after their parameter (`/api/chunk/{ci}/{i}.wav` and friends). matchit matches whole segments only, so those are routed by hand against `{file}` and split their own suffix; the *document* still carries the real contract URL, because that is what the client is generated from.

## Engine notes

**Kokoro through ONNX.** `onnx-community/Kokoro-82M-v1.0-ONNX`, fp32 `model.onnx`, driven by `ort`. Three inputs: phoneme token ids wrapped in the boundary token `$`, a 256-float style vector picked out of the voice pack **by phoneme count** (that is how Kokoro gets its pacing right), and a speed scalar. Output is f32 mono at 24 kHz — the same rate and layout `app/tts.py` produces, so nothing downstream knows which engine rendered a chunk.

**espeak-ng is a subprocess, not a library.** libespeak-ng keeps process-global state, is not thread-safe, and a wedge or a segfault inside it would take the server with it. A fork costs a few milliseconds against a chunk that takes a second or two to synthesize, and it can be killed on a timeout. It also deletes the Python image's entire `espeakng_loader` symlink surgery: the apt binary and its data are simply what `espeak-ng` on PATH resolves to and can never be a mismatched pair.

**G2P is the fallback half of misaki, not all of it.** Kokoro's real front end looks English words up in a lexicon first and only falls back to espeak-ng; this implements the fallback for every word, with misaki's `EspeakFallback.E2M` mapping table verbatim. Measured cost on the GATE 0 passage: 269.25 s against the PyTorch render's 271.57 s (0.9 %), with per-chunk boundaries lining up. Adding the lexicon would close the rest; it is a lookup table plus POS tagging, and it is the obvious next improvement if any word sounds wrong.

**Known delta, needs Fernando's ear:** the ONNX render is uniformly ~1.4× louder (≈ +3 dB) than `work/kokoro-test/kokoro_af_heart.wav` — same timing, same prosody, a flat gain. One sample of 6.5 million clipped. If that reference was written with headroom applied, nothing is wrong; if not, expose a gain.

**Whisper.** `large-v3-turbo-q5_0` ggml, CPU, serialized behind one mutex like the Python's lock. Language auto-detect, `WHISPER_PROMPT` + book title + chapter title as the initial prompt, silero VAD when `models/whisper/ggml-silero-v5.1.2.bin` is present. `WHISPER_MODEL` takes either a bare name (resolved to `<models>/whisper/ggml-<name>.bin`) or a path.

## Performance

Measured on this machine (16 x86 cores), chapter 1 of *Lord of Mysteries*, 33 chunks, af_heart:

| | RTF |
|---|---|
| Python + PyTorch Kokoro | ~4.5× realtime (AGENTS.md) |
| Rust + ONNX Kokoro, `listen_test` | **4.31×** |
| Rust + ONNX Kokoro, in the container, 7 Powers ch. 2 | **4.87×** |
| Rust + ONNX Kokoro, in the container, *Lord of Mysteries* ch. 1 | **4.67×** |

So: parity on synthesis, within noise. The Oracle A1 is the number that actually matters and it has not been measured.

Everything *around* the synthesis is a different story, and it is where the
reader's waiting actually was. On the 1433-chapter *Lord of Mysteries*
(7.8 MB epub, 16.8 MB of text, 12 shards), in the container:

| | Python | Rust |
|---|---|---|
| `POST /api/load`, cold | 12.4 s (measured by the reader) | **0.73 s** |
| `POST /api/load`, unchanged book | 12.4 s again | **0.36 s** (no parse at all) |
| `GET /api/chapters`, 1433 rows | a few thousand `stat`s per poll | **9 ms**, 244 kB |
| `GET /api/chapters?from=0&to=30` | not available | **12 ms**, 5.3 kB |

The cold parse being seventeen times faster is just Rust against ebooklib + BeautifulSoup + lxml; the second one is [the parse cache](#what-this-server-does-that-the-python-one-does-not).

## Config surface

Every name the Python `AGENTS.md` documents, with the same default: `NARRATOR_PORT` (7870), `NARRATOR_VAULT`, `NARRATOR_WORK`, `NARRATOR_BOOKS`, `NARRATOR_WEB`, `BOOKS_SUBDIR`, `POSITIONS_SUBDIR` (`02 - Studies`), `NOTES_SUBDIR` (`05 - Fleeting`), `KOKORO_VOICE` (`af_heart`), `KOKORO_SPEED`, `LOOKAHEAD` (80), `PRERENDER_CHAPTERS` (2), `PREFETCH_WHILE_PAUSED`, `MAX_AUDIO_GB` (5), `MAX_CHAPTER_GB` (20), `SILENCE_S` (0.5), `WHISPER_MODEL`, `WHISPER_PROMPT`, `CHAPTER_BITRATE` (`64k`), `CHAPTER_GAP_S` (0.30), `CHAPTER_PARA_GAP_S` (0.60), `HLS_SEGMENT_S` (6), `TEXT_SHARD_BYTES`, `TEXT_SHARD_CHAPTERS`, `HEALTH_STALL_S` (300), `AUTOPACK`, `AUTOPACK_EVERY_S`, `NARRATOR_WATCH_BOOKS`, `NARRATOR_FAKE_TTS`, `SSE_HEARTBEAT_S`, `SSE_QUEUE`, `SSE_RENDER_MIN_S`, `SSE_RETRY_MS`.

New here, because the weights are not downloaded by a Python package on first use: `NARRATOR_MODELS` (`/models`), `KOKORO_MODEL`, `KOKORO_VOICES`, `WHISPER_VAD_MODEL`, `ESPEAK_BIN`, `ESPEAK_VOICE`. `HF_HOME` and `KOKORO_REPO` are gone — nothing here talks to Hugging Face at runtime.

A value that will not parse logs a warning and falls back. A typo in an env var is not a reason to refuse to start a reader.

## Docker

Three stages: node builds the reader, cargo builds the server behind a manifest-only dependency layer (so a source edit does not recompile whisper.cpp, which is most of the build), and a `debian:bookworm-slim` runtime carrying one binary plus ffmpeg, espeak-ng and `libgomp1` (ONNX Runtime's CPU provider is OpenMP-threaded). 583 MB.

`HEALTHCHECK` is `narrator --healthcheck`: one loopback GET of `/healthz` written against a `TcpStream`, rather than putting curl in the runtime image for a single request.

**amd64 is built and run end to end**, including the real reader: the production
build of the React app, unmodified, boots against this server and goes all the
way through — library, `/api/load`, the 1433-chapter list, the text bundle's
twelve shards, `/api/open`, per-chunk wavs, auto-packing, and finally HLS
playback with 206s on the segments. Zero failed requests, zero console errors.
Notably it already sends `?book=` on `/api/chapter/{ci}`, which the python server
ignores — requirement 2 below paid for itself on the first run.

Memory while rendering *Lord of Mysteries* settles: 599 MB idle with the model
loaded, climbing to ~1.24 GB over five minutes of continuous rendering and then
flat to within 4 MB while still working. That is ONNX Runtime's arena reaching
its working set, not a leak.

### Deploying it

`deploy/` carries the templates. The `docker run` the unit performs, spelled
out:

```
docker run --rm --name narrator-rs \
  -p 127.0.0.1:7870:7870 \
  -v /home/ubuntu/narrator/work:/work \
  -v /home/ubuntu/narrator/books:/books:ro \
  -v /home/ubuntu/narrator/models:/models:ro \
  -v /home/ubuntu/vault:/vault \
  --env-file /etc/narrator-rs.env \
  narrator-rs:latest
```

with `/etc/narrator-rs.env` holding `NARRATOR_VAULT=/vault`,
`MAX_AUDIO_GB=50`, `KOKORO_VOICE=af_heart`,
`WHISPER_MODEL=large-v3-turbo-q5_0` and the rest (`deploy/narrator-rs.env`).
Port 7870 is bound to **loopback only** — there is no auth and no TLS, the box
is reached over Tailscale, and publishing that port is the one change that turns
a private reader into a public one. `deploy/narrator-rs-watchdog.{sh,service,timer}`
mirror the python repo's: curl `/healthz` every two minutes, restart after three
consecutive failures, with a fifteen-minute floor so a crash loop cannot hide
the problem.

The work dir and the vault are **adopted in place**: same cache paths, same
`plan.json`, same `.narrator-positions.json` and `Reading Log.md`. Nothing is
re-rendered and no position moves — that is what the parity suites above are
for.

## What is still missing before this can replace production

1. **arm64.** The image has never been built for aarch64: this box has no qemu binfmt registered and no cross toolchain. The two risky dependencies both look fine on paper — `ort` ships a prebuilt ONNX Runtime 1.22.0 for `aarch64-unknown-linux-gnu` (confirmed fetchable), and whisper.cpp's primary target *is* ARM — but "looks fine" is not a build. Do it on the Oracle A1, or `docker run --privileged tonistiigi/binfmt --install arm64` here first.
2. **The web reader.** Still the Python repo's app, still calling its hand-written `api.ts`. The generated client is ready and drift-clean; porting it is mechanical except for three things the drift report names: `ChapRow` → `ChapterRow`, the SDK's `{data, error, response}` envelope replacing the reader's `get()`/`post()` helpers, and the binary endpoints (`.m4a`, `.m3u8`) which must stay plain URL builders because they feed `<audio src>` and Cache Storage.
3. **A real-book soak.** The longest run so far is a few minutes. *Lord of Mysteries* is 1433 chapters; the things that only show up there are memory growth across thousands of ONNX sessions, the `/api/chapters` scan under a full cache, and gc churn at the 5 GB cap.
4. **The listening verdict.** GATE 0 is rendered and waiting; nothing should deploy until Fernando has compared the two wavs, and the loudness delta above is decided one way or the other.
5. **`.m4b` export.** `app/export.py` has no port yet. It is a CLI path, not a server one, and the Python script runs on the host with nothing but python3 and ffmpeg — so it still works against this server's cache unchanged.

## Rules

- **Never edit `~/git/narrator`.** It is the reference and it is someone else's working tree.
- **Never touch production.** The VPS is deployed by hand.
- No `unwrap()` or `expect()` outside tests and the startup path. Errors are typed (`thiserror`), the edges use `anyhow`, and every failure path logs and degrades.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are clean, and stay clean.
- A change to any response shape means regenerating the client and re-running the drift check in the same commit.
- If the chunker's output changes, that is a **migration**, not an edit — it invalidates every cache and every stored position. Say so out loud before doing it.
