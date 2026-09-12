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
| `src/export.rs` | `narrator export`: the streaming cache packed into one `.m4b`, chapter marks and cover art included. A port of `app/export.py`. |
| `src/events.rs`, `src/api/stream.rs` | The SSE bus and `/api/events`. |
| `src/state.rs` | One global session, exactly like Python's process-wide `S`. |
| `src/api/` | Every endpoint, with typed request/response structs that **generate** the OpenAPI document. |
| `src/watch.rs` | The library watcher (`notify`), which turns "the vault's git sync pulled an epub onto the server" into a `books` event. |
| `tests/` | Five parity suites, the reader-requirement suite, and a harness that runs a whole server in a temp dir. |
| `scripts/golden/` | Generates the Python golden fixtures (uv + ebooklib + bs4) the parity tests assert against. |
| `scripts/gen-client.sh` | OpenAPI → the reader's typed TS client (`web/src/client/`), and the `--check` gate CI runs. |
| `listen-test/` | GATE 0: the ONNX engine rendered against the PyTorch render Fernando accepted. |
| `web/src/client/` | **Generated**, committed, and the reader's only description of the API. Never hand-edited; see [the gate](#openapi-is-the-contract). |
| `web/` | **The reader itself** — the Vite/React/Tailwind PWA, source and all. It moved in from the python repo; the build is no longer vendored (`web/.gitignore` ignores `dist/`), because two images are built from this tree now. See [the two images](#two-images-server-and-reader). `web/placeholder/` is the fallback page when there is no build at all. |
| `deploy/` | systemd templates for the Oracle A1 — **applied by hand, never by a playbook**, like the python repo's. |

## Running it

```bash
./narrator models        # fetch Kokoro + whisper weights into ./models (~900 MB)
./narrator web           # npm ci && npm run build in web/ (dev serves web/dist)
./narrator dev           # cargo run, serving ./web and ./work
./narrator build && ./narrator up     # docker, port 7870 on localhost only
./narrator test          # the whole suite: 112 tests, no model, no network
./narrator lint          # rustfmt --check + clippy -D warnings
./narrator client        # regenerate openapi.json + web/src/client
./narrator export --book books/Title.epub [--partial]   # the cache → a .m4b
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

`work/audio/<stem[:50]>/chNNN/IIIII.wav`, 24 kHz mono s16le; `plan.json` beside the chapter directories; `chapters/<key>/chNNN.{m4a,json}`; `hls/<key>/chNNN/`; `text/<key>/{index,NNN}.json`, each with a `.json.gz` beside it. **A Rust deploy adopts the existing cache in place** — a test seeds a Python-shaped cache and asserts nothing is re-rendered.

**The text bundle is pre-gzipped at build time**, exactly as the python reference does it: `write_json_gz` writes the `.json` and its `.gz` sibling in one call (level 9, `mtime=0`, so a rebuild from the same words produces the same bytes), and `/api/book.json` and `/api/text/{s}.json` serve the `.gz` with `Content-Encoding: gzip` when the request's `Accept-Encoding` takes it, `Vary: Accept-Encoding` either way. The decoded body is byte-identical to the plain file — content negotiation only, the frozen contract is untouched. JSON this repetitive compresses to ~33 %: *Lord of Mysteries* goes 17.7 MB → 5.8 MB over the wire, once per device rather than once per request. A missing `.gz` (a bundle written before this existed) falls back silently to the plain file, and the next `/api/load` rewrites the bundle to earn one even when the plan itself is reused. The negotiation is hand-rolled rather than a `tower-http` compression layer for two reasons: a layer would re-compress those megabytes on every request instead of serving the file that is already on disk, and it must never touch the audio endpoints, which serve byte ranges — a range of a compressed body is not the range iOS asked for.

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

The reader kept a list of nine requirements written against the Python server,
each with the client-side mitigation standing in for it meanwhile. All nine are
implemented here, all nine are **additive** — a client that sends none of the new
parameters gets exactly the Python behaviour, which is what keeps the Obsidian
plugin working untouched — and each has a test in `tests/reader_requirements.rs`.
The list is gone; this is what it said, and what answered it.

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
7. **Every chapter endpoint takes the book.** `?book=` on `/api/chapters`, a
   `"book"` field on `/api/chapters/{render,build,cancel}`, and the same **409**
   on a mismatch as `/api/open`. This is the endpoint where the race is
   expensive rather than merely wrong: one tap can queue 74 chapters, and if the
   server swapped books between the reader's poll and the reader's tap — the
   watcher picked up an epub, the plugin opened something, a second device
   loaded another book — those renders occupy the worker for hours on the wrong
   novel. The reader had no mitigation that closed it; this does.
8. **The packed bitrate is reported.** `/api/status` carries `bitrate` (`"64k"`)
   and `bitrate_bytes_per_min` (480000), and every `/api/chapters` row carries
   `est_bytes` — the server doing the arithmetic with the constants it actually
   has. The reader used to hard-code the 64 kbit/s default, so changing
   `CHAPTER_BITRATE` on the box made every size in the UI wrong by that ratio,
   silently.
9. **`/api/chapters/build` says what it refused, and why.** A fourth list,
   `refused`, with one entry per chapter the packer did not take:
   `not_rendered` (carrying `rendered`/`n`, and the chapter is queued to render
   instead), `no_chunks`, `out_of_range`. Before, a refused chapter appeared in
   none of `built`/`building`/`rendering` and was indistinguishable from one
   nobody asked about, so the reader ignored the response entirely and re-asked
   every twenty seconds. It now reads the answer and keeps the repeat for a row
   that has not moved in ninety seconds, which is what a lost call actually
   looks like.

## OpenAPI is the contract

Handlers carry `#[utoipa::path]` and every request/response is a typed struct, so the document is **generated from the code**, not maintained beside it.

```bash
./target/release/narrator --openapi > openapi.json   # no server, no work dir
./scripts/gen-client.sh                              # spec → web/src/client
./scripts/gen-client.sh --check                      # ... and fail on any difference
```

**The reader is on that client.** `web/src/lib/api.ts` calls the generated
functions and `web/src/lib/types.ts` is a handful of aliases over the generated
types (`ChapRow` = `ChapterRow`, `SavedPosition` = `StampedPosition`), so there is
no longer a hand-written description of this API anywhere in the tree: a field
that moves in Rust is a TypeScript error in the same change. What stayed
hand-written in the reader is policy no generator knows — the offline-first
retry rules, and the URL builders for `<audio src>`, HLS and Cache Storage keys,
which are strings rather than calls (the generated client encodes query values
with `encodeURIComponent` too, so a builder URL and an SDK URL for the same
resource are byte for byte the same request).

That leaves one thing worth checking, and CI checks it: **is what is committed
what this server generates?** `--check` regenerates and diffs against git.
`release.yml` runs it with the debug binary (a spec change in Rust that was not
carried through is a red build); `web.yml` runs `--from-spec --check`, which
regenerates the client from the committed spec and catches a hand-edited
generated file on a push that never touches Rust.

The old field-by-field drift check — which compared the generated types against
the reader's hand-written ones through the TypeScript compiler — is gone with the
hand-written types it compared against. It earned its keep first: 29 mismatches
on its first run, all real. utoipa renders `Option<T>` as both nullable *and*
absent from `required`, while serde without `skip_serializing_if` always sends
the key — so the spec was lying about eleven `Status` fields. Fixed with
`#[schema(required = true)]` and `value_type` overrides on the fields the server
genuinely always sends. **That rule stands: if serde will always serialize it,
say so in the schema** — the reader's types are generated from those annotations
now, so getting one wrong is no longer a report, it is a bug in the reader.

Five paths carry a file extension after their parameter (`/api/chunk/{ci}/{i}.wav` and friends). matchit matches whole segments only, so those are routed by hand against `{file}` and split their own suffix; the *document* still carries the real contract URL, because that is what the client is generated from.

## Live updates: `/api/events`

One long-lived `text/event-stream` per client, fed by a tokio broadcast channel
(`src/events.rs`) and served by `src/api/stream.rs`. It is how anything learns that anything
changed — the heartbeat poll dropped to a fifth of its rate behind it, and the
chapter drawer's two-second poll survives only as a backstop while it is open —
and the design rule is one sentence:
**an event is not state.** Every event means "this changed, go and look", which
is what makes a dropped one — a phone the server lagged, a reconnect — a
non-event rather than a lost update. Nothing is replayed, and `Last-Event-ID` is
accepted and ignored on purpose.

| event | when | payload |
|---|---|---|
| `hello` | on connect, and on every reconnect | `{heartbeat_s, book, key, chapter}` |
| `position` | every time a position is *written* (so at the 15 s throttle, and forced on open/pause) | the vault record plus `book` and `source`: `session` for the loaded book's playhead, `api` for a named `/api/position` write |
| `render` | the worker moved | `{kind, key, chapter, render_idx, playhead, n, status}`; `kind` is `progress` (throttled to `SSE_RENDER_MIN_S`, default 1/s), `complete`, `chapter`, or `packed` (which also carries `ok` and an `error`) |
| `books` | an epub appeared in or left either book root, debounced 500 ms | `{changed: [names], count}` |
| `note` | a voice memo was transcribed and filed | `{file, book, chapter, chunk, language}` |

Plus what is not an event: `: narrator live` on open, `retry: <SSE_RETRY_MS>`,
`: ping` every `SSE_HEARTBEAT_S`, and `: lagged N` for a subscriber that stopped
reading. Those comments are the point of the heartbeat — bytes on the wire prove
a tunnel that is still there, which `navigator.onLine` does not.

**A slow client never backpressures the renderer.** `broadcast::Sender::send`
does not block and does not fail on a full queue; a receiver that falls behind
gets `Lagged(n)`, is told so as a comment, and carries on with what is left.
`SSE_QUEUE` (64) is the depth. Tested both ways in `tests/parity_events.rs`: at
the bus (twenty events into a four-deep queue, emit never fails, the reader sees
one lag and then the last four) and over the wire (a stream nobody is polling
gets `: lagged`, and still delivers the next event).

The `books` event is why `src/watch.rs` exists: both book roots are watched
(`./books` and the vault's `03 - Resources/Books`), so an epub the vault's git
sync pulls onto the server appears in the library with nothing to call and no git
coupling. A root that does not exist is skipped rather than fatal.

### The reader's half

`web/src/lib/live.ts` is the client: a typed `EventSource` wrapper that validates
each payload (this is the one part of the API that is *not* generated — its body
is a stream, so there is nothing for openapi-ts to type) and turns each event into
a TanStack Query invalidation. `hello` refetches everything, because a reconnect
by definition missed whatever happened while it was down. Reconnection is the
browser's own: `EventSource` retries on the server's `retry:` interval, and the
connection indicator in the top bar is fed from the stream's state rather than
from a failing poll.

`position` is the one event that is not a refetch, because the reading position
is not a query — it is the page in front of someone. The arbitration
(`arbitrate`, unit-tested in `live.test.ts`):

- another book, or within two chunks of where this device already is (its own
  echo, a chunk or two stale by the time it arrives) → **ignore**
- somewhere else, and nothing is playing here → **follow**: this is the
  phone-down, laptop-up case, and it is the whole feature
- somewhere else, and this device *is* playing → **offer**: a quiet line above
  the player bar ("moved on another device · <chapter>", follow / dismiss).
  Jumping the page mid-sentence because a phone in another room saved a position
  would be the worst thing this feature could do.

The heartbeat query (`/api/status`) survives at a fifth of its old rate — 5 s
instead of 1 s — as the fallback for a browser with no live stream and the source
of the few numbers no event carries.

## The `.m4b` export

`narrator export --book <file.epub> [--partial]` packs the rendered chunks in the
work directory into one AAC `.m4b` under `work/export/<Title>.m4b`: chapter marks
from the *real* wav durations (so a seek in a player lands where the chapter
does), the EPUB's title, author and cover art embedded, the same inter-chunk
(0.30 s) and paragraph (0.60 s) gaps the chapter packer uses, and 1 s between
chapters.

It is a **CLI path, not a server one**, exactly as `app/export.py` was: a
minutes-long ffmpeg run producing a file for a different device entirely (Apple
Books, a car), with nobody waiting on a response, and it has to work against a
work directory whose server is not running. What it needs is what the server
already leaves behind — `plan.json`, read raw rather than through the parse cache,
because an export of audio on disk has no business refusing to run because the
epub's mtime moved.

On the box, where the cache actually is, it runs inside the container the server
is already using:

```bash
docker exec narrator-rs narrator export --book "/vault/03 - Resources/Books/…/Title (2016).epub" --partial
# → /work/export/Title.m4b, i.e. /home/ubuntu/narrator/work/export/ on the host
```

A chapter with one missing chunk is not packable: the gap would swallow the hole
and every chapter mark after it would be wrong. So it is left out and named, and
without `--partial` the whole run refuses rather than quietly shipping a book
with holes. `--dir`, `--out`, `--bitrate`, `--title`, `--author` and the three
gap widths are the python script's flags, spelled the same way.

## Engine notes

**Kokoro through ONNX.** `onnx-community/Kokoro-82M-v1.0-ONNX`, fp32 `model.onnx`, driven by `ort`. Three inputs: phoneme token ids wrapped in the boundary token `$`, a 256-float style vector picked out of the voice pack **by phoneme count** (that is how Kokoro gets its pacing right), and a speed scalar. Output is f32 mono at 24 kHz — the same rate and layout `app/tts.py` produces, so nothing downstream knows which engine rendered a chunk.

**espeak-ng is a subprocess, not a library.** libespeak-ng keeps process-global state, is not thread-safe, and a wedge or a segfault inside it would take the server with it. A fork costs a few milliseconds against a chunk that takes a second or two to synthesize, and it can be killed on a timeout. It also deletes the Python image's entire `espeakng_loader` symlink surgery: the apt binary and its data are simply what `espeak-ng` on PATH resolves to and can never be a mismatched pair.

**G2P is the fallback half of misaki, not all of it.** Kokoro's real front end looks English words up in a lexicon first and only falls back to espeak-ng; this implements the fallback for every word, with misaki's `EspeakFallback.E2M` mapping table verbatim. Measured cost on the GATE 0 passage: 269.25 s against the PyTorch render's 271.57 s (0.9 %), with per-chunk boundaries lining up.

**The lexicon half: what a port would take, and why it has not been done.** It is
not a lookup table. misaki's `en.G2P` is a ~1200-line front end around two gold
lexicons (`us_gold.json` + `us_silver.json`, ~4 MB of JSON) plus:
a spaCy `en_core_web_sm` POS tag per token — a 12 MB statistical model, and the
only reason `read`, `lead`, `live`, `bow`, `close`, `record` come out right;
`num2words` for every number, ordinal, year, currency and decimal; a stress
system that re-marks a word by its part of speech and its position in the
sentence; and a stack of special cases (contractions, possessive `'s`, currency
symbols, `Mr`/`Mrs`, acronyms, hyphenation). The lexicon lookup alone, without
the tagger, would be worse than the fallback — it would give `read` one
pronunciation and be confidently wrong half the time, where espeak-ng at least
guesses from context. So a faithful port is a POS tagger in Rust or an ONNX
export of one, plus number expansion, plus the stress rules: days of work, a
~16 MB asset addition to the image, and a new class of divergence from the
Python render to test against. **Not attempted**, deliberately. The measured
difference today is 0.9 % of duration with boundaries lining up, and no word has
actually been reported as wrong. If one ever is, the cheap fix is a small
override table (a JSON map of word → phonemes consulted before espeak-ng),
which is an afternoon and carries none of the above.

**`KOKORO_GAIN`, and the delta it exists for.** The ONNX render is uniformly
~1.4× louder (≈ +3 dB) than `work/kokoro-test/kokoro_af_heart.wav` — same timing,
same prosody, a flat gain; one sample of 6.5 million clipped. Whether that is
wrong is Fernando's ear to decide, so what exists is the knob rather than a
decision: `KOKORO_GAIN` (default **1.0**, the identity — it changes not one byte
of a rendered chunk) multiplies every synthesized chunk post-synthesis, with a
soft knee at 0.95 so it cannot clip. Everything below the knee is a plain
multiply; above it, the remaining headroom is a `tanh`, which is smooth,
monotonic and can never leave ±1.0. Multiplying a waveform that already touches
±1.0 and then clamping is how a flat gain becomes audible distortion on exactly
the loudest words, which is the failure this avoids. `KOKORO_GAIN=0.71` would
undo the measured delta.

**It is not set on the box.** The verdict has not been given, and a gain applied
to chunks already rendered at another gain would make a book that changes
loudness at the frontier.

**Whisper.** `large-v3-turbo-q5_0` ggml, CPU, serialized behind one mutex like the Python's lock. Language auto-detect, `WHISPER_PROMPT` + book title + chapter title as the initial prompt, silero VAD when `models/whisper/ggml-silero-v5.1.2.bin` is present. `WHISPER_MODEL` takes either a bare name (resolved to `<models>/whisper/ggml-<name>.bin`) or a path. `WHISPER_THREADS` sets the thread count; it defaults to every core, which is what it was before it was configurable — see [the measurement](#whisper-on-the-a1).

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

**Whisper is not measured yet.** A 4.3 s memo took 2m10s end to end, twice —
but both runs happened while an emulated aarch64 `docker buildx` had the box at
load 59 on 16 cores, and a second run with the model already resident was no
faster, which is what saturation looks like rather than a warm-up cost. The
number means nothing until it is taken on an idle machine, and the one that
matters is the A1's anyway. Take it with
`time curl -XPOST .../api/note` twice in a row (the first call pays a 574 MB
model load) before trusting the voice-memo path to feel responsive.

## Config surface

Every name the Python `AGENTS.md` documents, with the same default: `NARRATOR_PORT` (7870), `NARRATOR_VAULT`, `NARRATOR_WORK`, `NARRATOR_BOOKS`, `NARRATOR_WEB`, `BOOKS_SUBDIR`, `POSITIONS_SUBDIR` (`02 - Studies`), `NOTES_SUBDIR` (`05 - Fleeting`), `KOKORO_VOICE` (`af_heart`), `KOKORO_SPEED`, `KOKORO_GAIN` (1.0), `LOOKAHEAD` (80), `PRERENDER_CHAPTERS` (2), `PREFETCH_WHILE_PAUSED`, `MAX_AUDIO_GB` (5), `MAX_CHAPTER_GB` (20), `SILENCE_S` (0.5), `WHISPER_MODEL`, `WHISPER_PROMPT`, `WHISPER_THREADS` (every core), `CHAPTER_BITRATE` (`64k`), `CHAPTER_GAP_S` (0.30), `CHAPTER_PARA_GAP_S` (0.60), `HLS_SEGMENT_S` (6), `TEXT_SHARD_BYTES`, `TEXT_SHARD_CHAPTERS`, `HEALTH_STALL_S` (300), `AUTOPACK`, `AUTOPACK_EVERY_S`, `NARRATOR_WATCH_BOOKS`, `NARRATOR_FAKE_TTS`, `SSE_HEARTBEAT_S`, `SSE_QUEUE`, `SSE_RENDER_MIN_S`, `SSE_RETRY_MS`.

New here, because the weights are not downloaded by a Python package on first use: `NARRATOR_MODELS` (`/models`), `KOKORO_MODEL`, `KOKORO_VOICES`, `WHISPER_VAD_MODEL`, `ESPEAK_BIN`, `ESPEAK_VOICE`. `HF_HOME` and `KOKORO_REPO` are gone — nothing here talks to Hugging Face at runtime.

A value that will not parse logs a warning and falls back. A typo in an env var is not a reason to refuse to start a reader.

## Docker

### Two images, server and reader

The repo publishes **two** images to GHCR, and the split exists for one reason:
the server takes the better part of an hour to build (whisper.cpp, twice, for
amd64 and arm64) and the reader takes eight seconds. Fernando edits the reader
far more often than the server, and a CSS fix should not wait on a Rust
compiler.

| Image | Built by | What it is |
|---|---|---|
| `ghcr.io/fernandobandeira/hearsay` | `Dockerfile`, `.github/workflows/release.yml` | The server. Multi-arch, built natively on both runners. Its node stage still builds `web/`, so the image carries a reader at `/web` and `docker run`ing it alone is a whole working thing. |
| `ghcr.io/fernandobandeira/hearsay-web` | `web/Dockerfile`, `.github/workflows/web.yml` | The reader's `dist/`, and nothing else: `FROM scratch`, contents at `/dist`, ~600 KB. Not runnable — it is a file delivery mechanism with a registry in front of it. |

`release.yml` carries `paths-ignore: web/**` and `web.yml` carries
`paths: web/**`, so a push touches one pipeline or the other. Both are
test-gated identically: the image only builds on green (the reader's gate is
`vitest` + `tsc -b`, and `release.yml` runs those too, because a reader that
does not typecheck is a server image that does not build).

`hearsay-web` is built for both architectures even though its payload is
architecture-free — the node stage is pinned to `$BUILDPLATFORM` so it runs once
natively, and the second manifest is pure metadata. The cost is nil and the box
(arm64) pulls without a `--platform` flag or a platform-mismatch warning.

### The server image

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

### Deploying the reader

The unit mounts `-v /home/ubuntu/web:/web:ro` over the reader baked into the
server image, so what the box actually serves is a directory of files on disk.
`deploy/hearsay-web-update.{sh,service,timer}` keep that directory current:

    pull hearsay-web:latest → compare the image id to /var/lib/hearsay/web.digest
    → unchanged? exit
    → changed? docker create (never started) · docker cp /dist → a staging dir
      · chown ubuntu:ubuntu · rsync into /home/ubuntu/web · stamp the digest

Every ten minutes, plus two minutes after boot. **So a UI deploy is `git push`**
— web.yml builds and pushes the image, the timer lays it down within ten
minutes, and `sudo systemctl start hearsay-web-update` on the box is the same
thing now.

Three details that are load-bearing, not taste:

- **The contents are synced, the directory is not replaced.** `/home/ubuntu/web`
  is a bind-mount *source*: the running container holds the inode it started
  with, so a `mv` of a freshly built directory into place would leave the server
  serving files nobody can reach. rsync writes each file to a temp name and
  renames it, so an update is atomic per file with the container none the wiser
  — and narrator-rs is never restarted for a UI change.
- **An empty directory is an empty reader.** The mount shadows the image's copy
  unconditionally; the server does not fall back. The updater refuses to install
  anything without an `index.html`.
- **Offline is not a failure.** A pull it cannot do logs a line and exits 0. The
  box keeps serving what it has, and picks the change up on a later tick.

Install (by hand, like everything else in `deploy/`):

```bash
sudo install -m 0755 hearsay-web-update.sh /usr/local/bin/
sudo cp hearsay-web-update.service hearsay-web-update.timer /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now hearsay-web-update.timer
```

## What has and has not been proven

This server *is* production: the box runs the published arm64 image, the reader
runs against it, and the cache and the vault it adopted are the same ones the
python server left. What that sentence does not cover:

- **The listening verdict is Fernando's and has not been given.** GATE 0 is
  rendered and waiting (`./narrator listen-test`), and the ≈ +3 dB delta above is
  still undecided — which is why `KOKORO_GAIN` exists and is **not** set on the
  box. Setting it mid-book would make a novel that changes loudness at the render
  frontier, so it wants a decision and a re-render, not a quiet flip.
- **No long soak.** The longest continuous render observed is minutes, not days.
  What only shows up over a real book: memory growth across thousands of ONNX
  sessions (it settled flat at ~1.24 GB over five minutes here, which is the
  arena reaching its working set rather than a leak, but five minutes is five
  minutes), gc churn at the `MAX_AUDIO_GB` cap, and the `/api/chapters` scan
  under a *full* 1433-chapter cache rather than a mostly-empty one.
- **The A1's synthesis RTF is unmeasured.** Every Kokoro number below is x86.
  The one that decides whether the box can keep ahead of a listener is the A1's,
  and taking it means rendering a real chapter there.

## Rules

- **Never edit `~/git/narrator`.** It is the reference and it is someone else's working tree — including its `web/`, which is now a historical copy of this repo's reader. The reader is edited **here**.
- **`web/src/client/` is generated.** Never hand-edit a `.gen.ts`; change the Rust and regenerate.
- **This repo is public.** Anything committed is on the internet: no tailnet addresses, no hostnames, no tokens, no vault contents. The reader talks to the API by relative path and has nothing to leak; keep it that way.
- **Never touch production.** The VPS is deployed by hand.
- No `unwrap()` or `expect()` outside tests and the startup path. Errors are typed (`thiserror`), the edges use `anyhow`, and every failure path logs and degrades.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are clean, and stay clean.
- A change to any response shape means regenerating in the same commit: `./narrator client`, then commit `openapi.json` and `web/src/client/` with the Rust change. CI fails otherwise, and the reader's types come from those files.
- If the chunker's output changes, that is a **migration**, not an edit — it invalidates every cache and every stored position. Say so out loud before doing it.
