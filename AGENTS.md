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
| `src/tts/numbers.rs` | The text normalizer: currency and years into English words before espeak-ng sees them, ported from misaki's `Lexicon.get_number`. See [numbers](#numbers-srcttsnumbersrs). |
| `src/tts/mod.rs` | `Engine`: lazy load, never fatal, plus the deterministic fake (`NARRATOR_FAKE_TTS=1`) the whole test suite renders with. |
| `src/stt.rs` | whisper.cpp via `whisper-rs`: `large-v3-turbo-q5_0`, CPU, one transcription at a time, ffmpeg decoding the webm, silero VAD when its model is present. |
| `src/cache.rs` | `work/audio/<key>/chNNN/IIIII.wav` and `gc_audio`. |
| `src/chapters.rs` | Chapter packing (AAC m4a + chunk→second manifest), lazy HLS, the chapter gc. |
| `src/plancache.rs` | The parse cache: `plan.json` plus a `parse.json` stamp, so `/api/load` does not re-parse a book that has not changed. |
| `src/text.rs` | The two-tier offline text bundle: `index.json` + byte-budgeted shards. |
| `src/vault.rs` | `.narrator-positions.json`, `Reading Log.md`, fleeting notes — all byte-identical to the Python output. |
| `src/render.rs` | The render worker and the packer, one OS thread each. |
| `src/migrate.rs` | `narrator migrate` and `narrator retrim`: the chunker migration and the padding trim, over a work directory whose server is not running. |
| `src/export.rs` | `narrator export`: the streaming cache packed into one `.m4b`, chapter marks and cover art included. A port of `app/export.py`. |
| `src/events.rs`, `src/api/stream.rs` | The SSE bus and `/api/events`. |
| `src/state.rs` | One global session, exactly like Python's process-wide `S`. |
| `src/store.rs` | `work/state.db`: the durable **intent and identity** — devices, per-device positions and high-water marks, standing orders, per-device inventory, the scanned chapter index, the book registry. Two rules it never breaks: [it is not disk truth](#the-store-and-the-two-things-it-is-not), and it does not replace the vault files. |
| `src/api/device.rs` | Who is asking (`X-Narrator-Device`), and who is *here* (the in-memory roster). See [naming the devices](#naming-the-devices). |
| `src/library.rs`, `src/api/library.rs` | The library index — a background scan of what is rendered and packed across *every* book, and `GET /api/library` over it. A cache of a filesystem scan, never truth. |
| `src/api/` | Every endpoint, with typed request/response structs that **generate** the OpenAPI document. |
| `src/watch.rs` | The library watcher (`notify`), which turns "the vault's git sync pulled an epub onto the server" into a `books` event. |
| `src/wishlist.rs` | The chapters someone asked for, kept across restarts — now in `state.db`'s `intent` table, with `work/audio/<key>/queue.json` still written beside it. Intent only, never progress. See [downloading a chapter](#downloading-a-chapter-end-to-end). |
| `tests/` | Five parity suites, the reader-requirement suite, and a harness that runs a whole server in a temp dir. |
| `scripts/golden/` | Generates the Python golden fixtures (uv + ebooklib + bs4) the parity tests assert against. |
| `scripts/gen-client.sh` | OpenAPI → the reader's typed TS client (`web/src/client/`), and the `--check` gate CI runs. |
| `listen-test/` | GATE 0: the ONNX engine rendered against the PyTorch render Fernando accepted. |
| `web/src/client/` | **Generated**, committed, and the reader's only description of the API. Never hand-edited; see [the gate](#openapi-is-the-contract). |
| `web/src/lib/library.ts` | The readiness ladder behind each book row — what is rendered, what is packed, what it would cost to take. See [the reader's half](#the-readers-half-readiness-on-the-book-row). |
| `web/src/lib/device.ts` | This device's id, minted once into `localStorage`. See [naming the devices](#naming-the-devices). |
| `web/` | **The reader itself** — the Vite/React/Tailwind PWA, source and all. It moved in from the python repo; the build is no longer vendored (`web/.gitignore` ignores `dist/`), because two images are built from this tree now. See [the two images](#two-images-server-and-reader). `web/placeholder/` is the fallback page when there is no build at all. |
| `deploy/` | systemd templates for the Oracle A1 — **applied by hand, never by a playbook**, like the python repo's. |

## Running it

```bash
./narrator models        # fetch Kokoro + whisper weights into ./models (~900 MB)
./narrator web           # npm ci && npm run build in web/ (dev serves web/dist)
./narrator dev           # cargo run, serving ./web and ./work
./narrator build && ./narrator up     # docker, port 7870 on localhost only
./narrator test          # the whole suite: 208 tests, no model, no network
./narrator lint          # rustfmt --check + clippy -D warnings
./narrator client        # regenerate openapi.json + web/src/client
./narrator migrate       # re-chunk cached books, drop only what that invalidated
./narrator retrim        # take Kokoro's padding out of wavs already on disk
./narrator export --book books/Title.epub [--partial]   # the cache → a .m4b
./narrator listen-test   # re-render the GATE 0 passage and report RTF
```

`cargo test` needs neither weights nor network: `NARRATOR_FAKE_TTS` swaps in a deterministic tone as long as the real thing would be, which is how a chapter renders and packs in milliseconds.

## Parity guarantees

Everything below is asserted by a test. "Golden" means the fixture was produced by running the Python implementation, not by blessing this one's output.

### Chunking: the one that cannot move

A chunk index *is* a reading position. It names a wav on disk, an entry in a chapter manifest, a line in the vault's `Reading Log.md`, and the passage a voice note points back to. Move one boundary and Fernando's place in a 1433-chapter book silently relocates and every packed chapter in the cache becomes a lie. So `src/book.rs` reproduces the Python exactly, **including two bugs**:

- **~~The abbreviation guards are inert.~~** `_ABBR` puts lookbehinds like `(?<!\bDr)` *before* `(?<=[.!?])`, so they test the two characters ending at the split point — which are `r.`, never `Dr`. All ten guards do nothing and "Dr. Smith" splits in two. **This one was fixed**, as a migration — see [the migration](#the-abbreviation-migration) below. `Guards::Inert` still reproduces it, and is what the python-parity suites assert against.
- **The split eats a closing quote.** `["'”’)\]]*` sits inside the separator, so `He said "go." Then` loses the `"`. Still reproduced: it costs a punctuation character, not a boundary, and nobody has reported hearing it.

Fixing either is a migration, not an edit: it invalidates every rendered chunk that follows it, every packed chapter containing one and any stored position past one. Three things Rust does *not* get for free are also handled: `len()` counts characters, Python's `\s` is wider than `char::is_whitespace`, and `str.isalnum()` includes the `Nl`/`No` categories — that last one decides whether a footnote marker like `²` is a silent beat.

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

That check still passes, and that is the point of [`Guards`](#the-abbreviation-migration):
the port is still provably faithful to the python, and the one place this server
parts company with it is a flag rather than a drift.

### The abbreviation migration

The first boundary ever moved on purpose. `Mr. Franky` was two chunks, which put
a chunk boundary, a full stop's worth of silence and a sentence-final fall in the
middle of a name; on *Lord of Mysteries* that happened 291 times, plus 3 more at
an initial like `Mr. A.`. `is_abbreviation` in `src/book.rs` now guards the split
the way `_ABBR` was reaching for — the word before the `.` read back over letters
and interior dots (so `i.e.` is found whole), required to start at a word
boundary (so `sir.` is not `Sr.`), and a lone capital treated as an initial. It
fails toward *merging*, which is the safe direction: a missed split is a longer
sentence, never a stop inside a name.

Measured on the real book, the whole blast radius:

| | |
|---|---|
| chapters whose chunking changes | **244 of 1433** (17.0 %) |
| chunks whose index shifts | **11168 of 118831** (9.4 %) |
| chunks before the first change in those chapters | kept — identical text, identical slot |
| if whole affected chapters were dropped instead | 19332, so precision is worth 8164 chunks of re-rendering |
| chapters untouched | 1189 |

**`Guards::Inert` is why the parity claim above survives.** Retiring
`a_python_written_plan_matches_chunk_for_chunk` because one boundary moved
deliberately would throw away the guard against every boundary that might move by
accident — Python's wider `\s`, its character-counting `len()`, its `isalnum()`.
So the python behaviour stays reachable, the opt-in suites assert against it, and
the ten golden cases that now diverge are listed one by one in
`tests/parity_chunking.rs` rather than re-blessed. A fixture that claims to be
python output has to be python output.

**`narrator migrate` does the transition** (`src/migrate.rs`), and it is a CLI
path for the same reasons `export` is: destructive, one-shot, and nothing is
waiting on it.

```bash
narrator migrate           # report, touch nothing
narrator migrate --apply   # do it
```

It re-chunks each cached book, finds the first chunk in each chapter whose text
changed, and deletes from there to the end of that chapter — plus that chapter's
packed m4a, its manifest and its HLS, which are built from all of it. A chapter
that chunks identically is not touched. **The plan is written last**, so an
interrupted run leaves a cache with holes and the old plan, which
[the disk-truth invariant](#the-disk-truth-invariant) heals by itself; the other
order would leave a new plan over stale audio, which nothing can detect — a
present wav reads as rendered whatever text it holds.

A stored position at or past its chapter's first changed chunk is pulled **back**
to that chunk, never forward, and restamped: a reader who lands a paragraph early
has lost seconds, one who lands late has lost the thread, and a healed record
carrying its old timestamp would lose the reader's own timestamp comparison and
be put straight back.

### The silence between chunks

Reported as "it stops in the middle of a sentence". It was two things, and
**neither of them was the chunker**.

**The gap constants were describing a quarter of the gap.** Kokoro returns each
utterance inside its own silence, and nothing removed it. Measured on the box's
own rendered wavs — 124 chunks of *Lord of Mysteries* — that is a median of
**0.31 s before the speech and 0.49 s after it**, against a median chunk of
8.8 s. So two chunks in a packed chapter were separated by 0.80 s of model
padding *plus* the 0.30 s `CHAPTER_GAP_S` inserts: **1.10 s**, and 1.40 s at a
paragraph. Inside a sentence that is the reported stop; between two sentences of
the same paragraph it is still about three times a natural pause.

`trim_padding` (`src/tts/kokoro.rs`) removes it, in `Engine::generate` rather
than in the packer — the reader plays the per-chunk wavs directly while a
chapter is still streaming, and that path has no packer in it. The threshold is
0.5 % of the chunk's own peak with 25 ms kept either side: Kokoro's padding
peaks at 1–7 of 32767 against speech at ~15431, a margin of about 2000×, so
nothing is near an edge. Measured against the real model, 0.72 s comes off a
chunk and the peak is unchanged to within a float — the speech is not touched,
only the silence around it. A chunk is about 27 % shorter, which the cache and
every packed m4a get for free.

This changes what is *in* a rendered chunk, never which text is in it. No chunk
index, manifest entry or stored position moves, and a chapter's manifest is
computed from the durations actually on disk, so a cache holding both trimmed
and untrimmed wavs stays correct — an old chapter keeps its long gaps until
something re-renders it.

**And the packer put a full stop's worth of silence in the middle of
sentences.** It chose between the two gaps on the paragraph index alone, so
every boundary got an inter-sentence pause whether or not a sentence had ended.
Most had. Measured across all 1433 chapters, **978 boundaries — 8.8 % of the
ones inside a paragraph — interrupt a phrase**:

| | count | what it is |
|---|---|---|
| clause of an over-long sentence | 684 | `chunk_paragraph` splits a sentence longer than `max_chars` at its `,` `;` `:` |
| abbreviation | 291 | [quirk 1](#chunking-the-one-that-cannot-move): the guards are inert, so `Mr. Franky` is two sentences and `Mr.` ends a chunk |
| an initial | 3 | `Mr. A.` |

`Gap::between` now has three answers rather than two, and the third is
`CHAPTER_PHRASE_GAP_S` (0.10 s). Deciding it needs to know that `Mr.` is not a
full stop, so the packer carries the word list `app/book.py`'s `_ABBR` was
trying to guard with. **The chunker's guards stay inert** — that is what every
stored position was chunked with, and changing it there is a migration. Here the
same list answers a much smaller question, where being wrong costs a pause
rather than a position.

The 294 abbreviation boundaries were then fixed at the source as well, because
the pause was only half of it — Kokoro still rendered `…loudly, Mr.` as a
complete utterance with a sentence-final fall, since that is the text it was
handed. That took [the abbreviation migration](#the-abbreviation-migration).
`Gap::Phrase` still earns its keep: 684 of the 978 are clause splits out of an
over-long sentence, and those are boundaries no chunker change can remove.

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

**And the other half of rule 1, which is that it has no exit.** "Render the chunk
under the playhead, always" is exactly right while the render can succeed and
exactly wrong when it cannot — a wedged espeak-ng, a work directory gone
read-only, a model that never loaded — because then it is a full-speed retry
loop: a subprocess spawn and a `warn!` line per iteration, a core burnt for
nothing and the log buried. Measured at **3611 attempts in 2.5 s** with the
backoff disabled. So a run of *consecutive* failed renders now pauses the worker,
250 ms doubling to a 30 s cap, and any success puts it straight back to zero. The
invariant is untouched: the chunk is still retried, still first, just not
thousands of times a second. It is deliberately not per-chunk — a render that
fails is almost always systemic, so in that state every chunk fails and a
per-chunk counter would only ping-pong between two hot targets.

The worker also parks entirely while a voice memo is being transcribed — see
[the STT priority gate](#the-stt-priority-gate). That is not a failure and does
not touch the backoff.

### Surviving a restart

The other half of the same incident. Playback is one global in-memory session, so
a container restart emptied it: `book` was `None` until something called
`/api/load`, and a reader that was already mid-chapter never does — loading is
what *picking* a book does. In that state `/api/chunk/{ci}/{i}.wav` (which is
session-scoped) 404s, `/api/open`, `/api/playhead` and `/api/chapters?book=`
answer 409, and the worker has no plan to render from. The reader sat on a 404
that would never become a 200, silently, showing a healthy connection.

Everything needed was already on disk — the plan in `plan.json` with its
`parse.json` stamp, the position in the vault. The only missing thing was the
name of the book, which is now `work/session.json`, written by `/api/load` and
read back by `narrator::boot` at startup. Restoring costs one plan read (0.30 s
on the 1433-chapter book) and **never a parse**: a stamp that no longer matches
means the book changed, and re-chunking it here would move every stored position
in it. It also does not start the renderer — a process that comes up rendering a
book nobody is reading is a worse failure than the one being fixed. What starts
it is the reader's next `/api/playhead`, which now wakes the worker if nothing
has started it in this process (gated so an explicit `/api/renderer {"on":
false}` is not undone by a chunk advance).

The reader has a backstop for the cases the server cannot restore: `hello` fires
on every reconnect and names the book, so a `hello` naming *no* book while this
device has one open is an unambiguous "the session is gone", and the reader
re-issues `/api/load` and tells it where it is (`lostSession` in
`web/src/lib/live.ts`, rate-limited to once per ten seconds so a flapping tunnel
is not a load per flap). Deliberately narrow: a `hello` naming a *different* book
is another device having loaded one, which is the existing one-session-at-a-time
behaviour and not this reader's to undo — two readers healing a mismatch would
take turns kicking each other's book out.

### The API

Every path, method, field name, type and nullability of the Python contract, asserted against `tests/fixtures/api_contract.json` — a transcription of `AGENTS.md` + `app/server.py` + `web/src/lib/types.ts`, written down rather than generated here, so it can catch *this* server drifting.

25 paths. `/api/events` (SSE) is implemented over a tokio broadcast channel with the same wire format, the same coalescing of `progress` and the same refusal to replay. `/healthz` returns 503 with `problems[]` for the failures that actually happen: a dead render thread, a "rendering" status with no chunk in `HEALTH_STALL_S`, an unwritable work dir.

## Naming the devices

**The bug, as reported:** open the PWA on the phone, and it sometimes decides
another device has moved to a different chapter and follows it there — except
that chapter is *behind*, so the reader loses its place going backwards.

Three facts were missing, and none of them was in the arbitration logic:

1. **Who wrote a position.** There was one server-side session and one record
   per book, and nothing anywhere carried a device identity. So "is this event
   my own echo coming back?" was answered by distance: if it landed within two
   chunks of where this device already was, it was probably ours (`SLACK` in
   `web/src/lib/live.ts`). That guess is wrong in both directions — it swallows
   a genuine two-chunk move made on the laptop, and it waves through this
   device's own report the moment the playhead has drifted three chunks by the
   time the event arrives, which on a phone behind a tunnel is most of them.
2. **When, unambiguously.** `save_position` serialized the *vault* record, so
   the event carried `updated` — a naive local stamp with no zone, which a
   browser cannot order — and not the `updated_ms` that had been added to the
   API's edge for exactly this (requirement 3). Every recency rule in the reader
   was reasoning from a string it could not trust, so there weren't any.
3. **Which way.** `arbitrate` compared a book, an undelivered flag and a chunk
   distance, and nothing else. A position *behind* this device produced
   follow/offer identically to one ahead.

**And the mechanism that fires it** is a laptop in a background tab. `hello`
arrives on every reconnect, `healSession` posts `/api/open`, the one global
session moves back to where *that* device is, the server writes a position there
and broadcasts it. The phone follows — correctly, by its own rules. A position
from somewhere else is exactly what that event means.

### The header, and the one endpoint that cannot take one

A device now says who it is: `X-Narrator-Device`, a uuid the reader mints once
into `localStorage`, plus `X-Narrator-Device-Name` as a label. Set once in
`web/src/lib/api.ts` through the generated client's `setConfig`, so it rides on
every call rather than the handful somebody remembered.

`GET /api/events` takes it as `?device=` instead, and that asymmetry is not
laziness. `EventSource` is what gives the reader its reconnection for free — the
browser retries on the server's own `retry:` interval, with no code here to get
wrong — and the price is an API with **no way to set a request header**. So the
one endpoint that most needs to know who is connected is the one that cannot be
told the ordinary way. `src/api/device.rs` reads the header first and falls back
to the query, which keeps it one concept with two spellings. The id is a random
uuid rather than a credential: it identifies a browser profile to itself and
grants nothing, which is what makes it safe in a URL that lands in an access log.

**Additive, and tested as such.** A client that sends no identity is the
anonymous device, whose id is the empty string — the Obsidian plugin, the
python-era reader, `curl` — and it gets exactly the behaviour it had before any
of this existed. `tests/devices.rs` asserts that rather than assuming it.

### The arbitration, now seven rules

`arbitrate` (`web/src/lib/live.ts`), in order, each with what it prevents:

| | rule | why |
|---|---|---|
| 1 | another book → ignore | real, saved, and not about this page |
| 2 | `ev.device` is ours → **ignore** | the own-echo test, exact. Only a non-empty id matches, so two unidentified clients cannot mute each other |
| 3 | this device has undelivered writes → ignore | unchanged: the server is describing a session that stopped hearing from us chapters ago |
| 4 | `ev.updated_ms <= our last write` → ignore | it predates something we told the server; it cannot be news |
| 5 | same chapter, within `SLACK` chunks → ignore | the **backstop** for a client that sends no id, which is all it ever should have been |
| 6 | behind our high-water mark → **offer**, never follow | the reported bug |
| 7 | otherwise → playing ? offer : follow | unchanged, and still the whole feature |

Rule 6 is the one worth defending. Going back to re-read a chapter on the laptop
is a real thing to want, so a backwards position is not *ignored* — it is
offered, as a quiet line above the player bar. It is simply never done to
somebody without asking. The mark is chapter-granular (`narrator.furthest:`,
which the auto-trim already keeps) and `chunk: 0` is the conservative rounding:
an earlier position *in the same chapter* still reads as "not behind" and can be
followed, while an earlier chapter is always offered.

**Why the timestamp rule alone was never enough**, which is the same reason
written down under [coming back online](#coming-back-online-drags-you-backwards):
the bad event is not an old write, it is a **fresh write of stale content**. The
server stamps it `now`, so every recency rule waves it through. Rule 4 catches
the subset that predates our own write; rule 6 catches the rest, by asking about
direction instead of time.

### Presence, which is deliberately not in the database

Who is *connected right now* lives in an in-memory `Roster` (`src/api/device.rs`),
held by an RAII guard for the life of each `/api/events` stream — so it goes when
the connection goes, including when the connection ends by the task being dropped
because a phone locked, which a tidy-up at the end of the handler would miss.

It is not written down, on purpose, and the reason is the same one the whole
store rests on. A process that comes back up with nobody connected is correct. A
process that comes back up *believing three devices are listening* would render
ahead for readers who are not there — which is [the disk-truth
invariant](#the-disk-truth-invariant)'s bug wearing a different hat: bookkeeping
that outlived the thing it described.

## The store, and the two things it is not

`work/state.db`, SQLite through `rusqlite` (`bundled`, because the runtime image
is `debian:bookworm-slim` with no libsqlite3 and the build is multi-arch).

**Why a database at all.** Everything narrator remembered was remembered *per
book, in a JSON file beside that book's audio* — `queue.json`, `plan.json` and
its stamp, `session.json`. That shape answers every question about the book in
front of you and none about the library: which device is where, how far anything
has ever reached, which chapters somebody asked for on a book that is not loaded,
what each phone actually holds. Those are the questions this round is about, they
are asked by the scheduler and by the reader's reconciler, and answering them by
walking a few thousand directories per request is how the python server's
`/api/chapters` got slow.

**Why not Postgres**: a second container, a backup surface and a network hop for
a few megabytes on a box with two cores. **Why not PouchDB/CouchDB**: replication
with revision trees solves multi-master conflict, and the problem here was never
conflict — it was that there was no identity or ordering to resolve *with*. That
would have been sync machinery bought to sit on top of the same missing facts.

### It is not disk truth

**This database never holds "is this chunk or chapter rendered."** That is [the
disk-truth invariant](#the-disk-truth-invariant) and it is the exact bug this
rewrite exists to fix. A row in `chapter_index` saying `rendered = 33` is **a
cache of a filesystem scan**: it is there so a readiness view can answer for a
book the session has not loaded without a thousand `read_dir` calls, and it is
stale the instant the gc runs. Nothing in the renderer or the packer may consult
it. Ever.

This is written out at length in the module doc as well, because the next person
to touch the file will find a `rendered` column sitting right there and be
tempted, and the failure that follows is silent, slow to reproduce and
indistinguishable from a network problem. The same rule is why there is no
`chunk` table at all.

### It does not replace the vault files

`.narrator-positions.json` and `Reading Log.md` are a byte-level contract with the
Obsidian plugin and with the python reference — two programs that have never
heard of this file and never will. The direction is one-way: the store is the
record, the vault files are a **projection** of it, written exactly as they have
always been written.

The store keeps **one row per (book, device)** where the vault keeps one per
book, and that difference is the point. The vault record is last-write-wins,
which is the right rule and always was; but a projection throws away exactly the
fact the reader needed — that the laptop is at chapter 10 and the phone is at
chapter 40, rather than that "the position" is wherever the most recent report
came from. `newest_position` is that projection rule, ordered by `updated_ms`
then `seq`.

A **high-water mark** moves alongside it (`furthest`), forwards only. "Where I
am" and "how far I got" are different questions — re-reading a scene must not
shrink it — and it is what the reader's auto-trim and rule 6 above are both
anchored on.

### The sequence number

Every position write is stamped with a monotonic integer, persisted in the store
so it survives a restart. Two devices reporting inside the same millisecond is
unlikely; a *clock* that steps is not — an ntp correction on a box that has been
up for weeks, a container whose `/etc/localtime` changed under it — and then two
writes compare equal or backwards and "which of these is later" has no answer at
all. A monotonic integer always has one.

### Failure is the caller's to shrug at

`AppState::store()` is an `Option`. A work directory gone read-only, a
`state.db` that is not a database, a disk with nothing left on it — none of those
is a reason to refuse to start a reader. What is lost without it is the *extra*
answers; what keeps working is everything that worked before it existed, because
all of that still reads the filesystem and the vault. Every caller logs once and
does nothing.

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

### And one that is not on that list: `GET /api/library`

The nine above were written against the python server. This one is new, and it
is the endpoint the rest of this round exists to make possible.

Every readiness question — is this book rendered? is it packed? how much of it
could I take with me right now? — could only be asked about **the book the
session had loaded**. Asking it about anything else meant *opening* that book,
which loads it server-side, moves the one global session and drags the render
frontier with it. That is an expensive way to ask a cheap question, and on the
phone it is exactly the question you want answered *before* you commit to a
book.

So `/api/library` answers it for the whole library at once, out of the store's
`chapter_index`, with no session involved:

```
GET /api/library[?book=<key>][&chapters=true]  →  200 LibraryResult
```

Per book: chapters, chapters rendered, chapters packed, packed bytes, chunks
rendered out of total, an estimate in minutes, the stored reading position, and
whether it is the loaded one. `?book=` narrows to one; `?chapters=true` adds the
per-chapter rows **only** alongside `?book=`, because 1433 rows per book across
a library is precisely what this endpoint exists to avoid.

`scanned_ms` is the **oldest** stamp in the answer rather than the newest, so
one stale book cannot hide behind eleven fresh ones — the reader shows the age
only when it is worth showing.

**It is a scan result, not a live fact**, and it says so. `src/library.rs`
refreshes the index every `LIBRARY_SCAN_EVERY_S` (300 s) and immediately at the
two moments a row genuinely goes stale: a chapter finishing its last chunk, and
an m4a landing. The scanner stands down for [the STT
gate](#the-stt-priority-gate) and for a renderer stalled under the playhead,
with the same shape and the same bound as the packer's hold-back — a few
thousand `stat`s is a background load on two cores, and it outranks nothing.

Measured against the real python work directory: **0.10 s** for the
1433-chapter *Lord of Mysteries*, 8 ms for a 33-chapter book, on this desktop.

And the direction is one-way, which is the whole discipline of [the
store](#the-store-and-the-two-things-it-is-not): the renderer and the packer
**write** this index and never read it. A `rendered` column is not permission to
stop asking the filesystem.

With no store at all it answers `200` with an empty list rather than an error,
because every failure path here logs and degrades.

### The reader's half: readiness on the book row

`web/src/lib/library.ts` is the pure part, and the drawer's Books view renders
it as a second line under each title. What it says, and the two decisions worth
recording:

**`ready` outranks `rendered`, even at one packed chapter against a whole
rendered book.** The question a library row exists to answer is "what could I
take with me right now", and a rendered *chunk* is not a file any device can
hold — packing is what turns the box's night of work into something
downloadable. So the first packed chapter is the news, and a fully rendered book
with nothing packed reads as `rendered`, which is true and is not the same
claim.

**Progress is counted in chunks, not chapters.** On the 1433-chapter book the
renderer can spend hours inside chapter one, and `rendered_chapters` says 0 for
all of it. `rendered_chunks / total_chunks` says 1 %, which is both true and
useful, and it is clamped at both ends so that 0 % and 100 % stay categorical
rather than reachable by rounding.

The download estimate takes `bitrate_bytes_per_min` **as a parameter** and has
no default. That is [requirement 8](#what-this-server-does-that-the-python-one-does-not)
being obeyed rather than re-broken: the reader used to hard-code 480000, so
changing `CHAPTER_BITRATE` on the box made every size in the UI silently wrong
by that ratio. With no rate available the row says so instead of showing a
number it guessed.

Book order is most recently opened first, nulls last — **the same order the
worker renders in**, which is the point rather than a coincidence: the top of
the list is what the box is working on.

**The offline path is untouched**, and that is load-bearing. The list still
falls back to the books this device has opened before (localStorage) when the
server cannot be reached, nothing gates on the readiness query, and a row with
no readiness is byte-for-byte the row that was there before. The one place the
two meet is the sort, where an empty index ties every comparison and `Array`'s
stable sort leaves the offline list in the order it came in.

The scan's age is shown **only when it is stale** (two missed ticks plus slack).
A line that always says "scanned just now" is noise. And `/api/library` is
invalidated by the live stream on `hello`, on `books`, and on a `render` event
with `kind: "packed"` — but deliberately not on `progress`, which is throttled to
one a second and would refetch the whole library at that rate for a number the
list does not show.

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

### Coming back online drags you backwards

An hour of reading offline, and the moment the tunnel came back the reader jumped
to where it had been when the tunnel went. The position queue was not the bug —
it worked. **`/api/position` heals the vault record and nothing else.**

Playback is one server-side session, and the session's chapter only ever moves on
`/api/open`. Offline, `openChapter`'s `tellOpen` is fire-and-forget and its
failure is dropped, so three chapters of reading left the session exactly where
it was. The queue meanwhile kept the right position — chapter included — and
pushed it to `/api/position` on reconnect, which writes the vault, emits a
`position` event, and **does not touch the session**. Then the very next thing
that saves from that session — a `/api/playhead` one chunk later (it carries only
a `chunk`, so the chapter is the session's stale one), a `/api/pause`, the 15 s
throttle expiring — overwrote the record that had just been healed and broadcast
the old chapter. The reader followed it, correctly: a position from somewhere
else is exactly what that event means. The render frontier was on the wrong
chapter for the same reason.

Two changes, both in the reader:

- **The first contact after an undelivered position is `/api/open`, not
  `/api/playhead`.** It is the only call that carries a chapter, and the chapter
  is the whole of what is out of date. `healSession` does it on every reconnect
  path (`state.tsx`), and `report` does it inline, so a device that simply
  resumes playback heals without waiting for a flush. `flushPositions` now
  returns the books it delivered, which is how the caller knows there was
  anything to heal; the heal runs *before* the vault post, because `/api/open`
  writes the record itself and a session left stale would undo it either way
  round.
- **`arbitrate` ignores the server about a book this device has out-run.** While
  a position for it sits undelivered in IndexedDB, nothing the server can say
  about that book is news — it is describing a session that stopped hearing from
  the reader some chapters ago. That closes the window between the queue draining
  and the heal landing, and it is also the right rule on its own: last write
  wins, and this device wrote last.

**Why the obvious timestamp rule does not fix this.** Stamping the local position
and ignoring any server record older than it is the usual answer, and
`resolveResume` already does exactly that where it works — at *open*, against the
record `/api/load` returns. It cannot work here: the bad event is not an old
write, it is a **fresh write of stale content**. The server stamps it `now`, so it
is newer than anything this device holds, and every recency rule waves it
through. What is out of date is the session, not the record, and the only thing
that fixes a stale session is telling it where the reader is.

## Downloading a chapter, end to end

A download is two stages — render every chunk, then pack the chunks into one
m4a — and three parties: the server's render worker, the server's packer, and
the device that has to end up holding the file. The design rule for the whole
pipeline is that **no stage may depend on the app still being open**, because on
the A1 a 74-chapter download is an overnight job and iOS suspends a PWA within
seconds of the screen going off.

### The order: `pack: true`, and `work/audio/<key>/queue.json`

`POST /api/chapters/render {"chapters": [...], "pack": true}` is the whole
request. Additive in both halves — a client that sends neither the flag nor
`book` gets the python behaviour — and it means two things:

- the chapters go in the render queue, as before;
- each is marked `build_want`, so the worker hands it to the packer the moment
  its last chunk lands (`next_queued` in `src/render.rs`). **Nothing else has to
  be called.** The reader used to send one `/api/chapters/build` per chapter when
  it saw the row reach "fully rendered", which is why closing the app used to
  leave a night of rendered chapters and not one file.

`/api/chapters/build` takes the same flag's meaning implicitly: a chapter it has
to render first is queued with `pack: true`.

The order survives the process in `src/wishlist.rs` — one `queue.json` per book,
beside its `plan.json`, written `.part` → fsync → rename → fsync-dir on every
mutation, resumed by `narrator::boot` after the session restore and re-adopted by
`/api/load`. **Intent only, never progress**: which chapters, in what order, and
whether each wants packing. Completion stays disk truth, so a chapter finished
while the process was down simply leaves the queue the first time the worker
looks at it. A chapter picked back up by five restarts with no chunk landing is
*parked* — kept, logged, reported as `ChapterRow.parked`, retried by asking
again. The details, including why the file is per-book, are in that module's doc.

**What is deliberately not persisted: `autopack`'s own speculative orders.** The
packer also packs the chapter being read and the one after it, from the worker's
idle branches — organic listening rather than anything anybody asked for. Those
are re-derived from the current chapter on every boot and are *not* written to
the wishlist. Only a user-requested download is durable, which is the whole
distinction: an order somebody placed outlives the process, a guess about what
might be useful next does not.

### Never idle: what the worker does when there is nothing to do

The box renders at [about a quarter of realtime](#the-a1-measured) and can never
keep up with anyone listening, so every second the worker spends asleep is a
second somebody waits for later. It used to spend a great many of them: once the
playhead's lookahead and the prerender span were full, there was no branch left
and the loop slept — with, on *Lord of Mysteries*, fourteen hundred unrendered
chapters behind it.

So there is a sixth branch, below all the others:

1. the chunk under the playhead, if it is missing — **disk truth**, always first
2. a hole in the lookahead window
3. a chapter somebody named in the chapter manager, on the loaded book
4. **a standing order on any *other* book** — new, and the fix for a real hole:
   the order was durable the whole time (`state.db`'s `intent` table, and
   `queue.json` beside it), but nothing read it until that book was loaded
   again, so "download these 74" followed by opening something else left 74
   chapters waiting for an `/api/load` that might not come for days
5. the chapter being read, if it is not finished
6. the prerender span
7. **the rest of this book, then the rest of the library** — most recently
   *opened* first, each from the position it was last left at

The ranking is the honest one: an explicit ask beats a guess, and the book in
front of the reader beats one that is not.

Rule 7 outranks nothing. The loop re-reads the playhead every iteration, so the
work is abandoned the moment there is real work, and `render_stalled` is never
set by it, so the packer is not held back either. Book order comes from the
store's `last_open_ms`, which `/api/load` stamps — the last thing Fernando
actually chose, rather than a guess about what he might choose next. A book that
is not the loaded one has its plan read **straight off `plan.json`**, never
through the parse cache: most of the library is not loaded, those epubs may have
moved, and the chunks on disk correspond to that plan whatever has happened to
the file they came from. (That is `plancache::read_raw`, the view `narrator
export` has always taken.)

Completion stays disk truth throughout — an order whose chunks all exist simply
leaves the list the first time the worker looks at it. Nothing in either of these
branches reads the library index.

**The ceiling, which is the part that matters.** `gc_audio` trims the chunk cache
to 90 % of `MAX_AUDIO_GB` once it passes 100 %, oldest first — and oldest-first is
*precisely* the speculative work nobody has listened to yet. A renderer that ran
to the cap would therefore not settle: it renders to the cap, the gc deletes what
it just made, and it renders it again, for as long as the process lives. Worse,
it is invisible — the log looks busy, the RTF looks healthy, and nothing ever
finishes.

So speculation stops at `IDLE_CEILING`, **80 %**, and the band between there and
the collector belongs to demanded work only. The two never touch: one stops below
the floor the other starts at. The cache size is the number `gc_audio` already
returns (the walk has been done, so it is free), re-measured every 25 speculative
chunks — kilobytes of staleness against a band measured in gigabytes.

`tests/scheduler.rs` asserts the parts that make it dangerous rather than the part
that makes it useful: that it renders ahead at all, that it stands down above the
ceiling (with the seed's size asserted, so the test cannot pass for the wrong
reason), that a hole punched under the playhead still gets filled first, that
rendering ahead does not read as a stall to `/healthz`, that a finished library
stops rather than spins, and that a finished book moves on to the next one.

### Ordering a book you are not reading

Requirement 7 gave the chapter verbs a `book` field and a **409** on a mismatch,
and the reasoning holds: one tap is 74 chapters, and 74 chapters of rendering on
the wrong novel is an afternoon of the worker. But refusing was only ever half
the right answer. The client *named* a book; the correct thing is to act on that
one.

Nothing stood in the way any more. The worker follows standing orders across the
library (branch 4) and the packer can pack a chapter of a book the session is
not holding (`Session::pack_elsewhere`). So `/api/chapters/render`,
`/api/chapters/build` and `/api/chapters/cancel` now act on any book **the
library knows**, and a 409 means *no such book* — a real refusal rather than a
limitation wearing one's clothes. `GET /api/chapters` keeps its 409, because it
is a read and `/api/library` is the endpoint that answers for the whole library.

That is what makes the readiness line on a book row something you can act on:
see that a book is half packed, ask for the rest, and never open it.

Three things that had to be true for it to be safe, all tested:

- **A finished order is retired.** An order that is never dropped is walked
  again on every pass of the worker loop for the life of the process — a
  `read_dir` per chapter and a `plan.json` parse, which on the 1433-chapter book
  is 0.30 s *per chunk rendered*. A completed order hands itself to the packer if
  it asked to be packed and leaves the list either way. A slow leak with a
  healthy-looking log is the exact shape of failure this round is about.
- **The foreign plan is cached.** One book is worked through at a time, so the
  cache hits on every iteration but the first.
- **Cancelling reaches as far as ordering.** A download that could be started and
  never stopped is hours of the box's only spare core on something nobody wants
  any more. A chapter the packer has already picked up is left alone, exactly as
  the loaded book's `building` is.

### The CPU policy: pack when the renderer is ahead or idle

Two ARM cores, Kokoro at a quarter of realtime, and now packs that arrive with no
client watching — a resumed wishlist can fill the pack queue seconds after boot.
So the packer has a second hold-back below [the STT gate](#the-stt-priority-gate),
with the same shape and the same reason one rank down:

- the renderer sets `render_stalled` when it takes **rule 1** — the chunk under
  the playhead is missing, so somebody is waiting on that exact chunk *now* — and
  clears it on any other branch;
- the packer holds a **new** encode back while that flag is set. One already
  running is left to finish, for the same reason a memo does not kill it: an
  encode abandoned mid-chapter costs the box everything it has spent.

Only rule 1 counts as "behind". The lookahead is 80 chunks and also reports
`status: "rendering"`, and a packer that waited for *that* would never run on
this box at all. The hold is bounded at `PACK_HOLD_MAX_S` (30 s): past it the
renderer is not slow, it is wedged, and a download that waits forever on a wedged
renderer would be a worse bug than the one being prevented.

### The device's half: the download queue

The one step that can only happen on the phone is copying the m4a into Cache
Storage, and the phone is exactly what is not running. **Backgrounded work is not
attempted** — that is the accepted platform limit, not something to fight — so
what exists instead is a queue on the device, in IndexedDB (`downloads`, beside
the outbox), driven by `web/src/lib/reconcile.ts`. Every pass asks four
questions:

> what was asked for · what the server still has to be told · what is packed ·
> what is here

Each term comes from the only place that knows it: the pending selection from
IndexedDB, the standing order re-derived from the server's rows, `m4a` from those
same rows, and Cache Storage — *asked*, never remembered, so a quota eviction
reads as missing. The triggers are `visibilitychange`, `focus`, the network
returning, a cold launch, `hello` off the live stream, a `render` event with
`kind: "packed"`, and a 20 s interval while the app is visible. Chapters are
copied **three at a time** (`PARALLEL`); one that will not come down does not
cancel the rest, and what is still wanted is recomputed from Cache Storage
afterwards rather than from what the sweep thinks it stored.

The selection is written down *before* the first request, or a download confirmed
as the screen locks would be one nobody remembers. It is removed per chapter as
each lands, and the record is deleted when the last one does.

**The queue places the server's order too**, which is the half that was missing.
`pack: true` rides on a *render* call, so two kinds of chapter fell out of it: one
whose call was lost or never made (the app was killed between writing the
selection down and posting it), and one that was already fully rendered and
therefore needed no render at all — nothing ever asked the packer for it, and it
sat there for good. `toOrder` re-derives both from the rows on every pass and
posts `/api/chapters/render {pack:true}` and `/api/chapters/build`; the ask is
repeated only when it changes or after `ORDER_EVERY_MS` (2 min), since the order
is on disk at both ends.

**What this replaced, and the four bugs that came out of it.** The drawer used to
climb a per-chapter ladder in component state — queue the render, poll, ask for
the pack, poll, store, next chapter — and every one of Fernando's reports was a
consequence of where that loop lived:

- it died with the component and with the app, so a selection had no driver after
  a restart and the drawer showed nothing queued;
- it was the only thing that ever asked for a pack, so a chapter the server
  finished an hour later was never stored until the app happened to be re-opened;
- a chapter that threw set one shared `err` string and was dropped, and the next
  chapter's failure overwrote the message — so a failed chapter looked *skipped*;
- it walked chapters one at a time, so a 6 MB file had the whole link to itself
  and used a fraction of it.

The ladder is gone. `web/src/lib/download.ts` is down to reading a row
(`phaseFor`, `queueJob`, the size estimate); the drawer writes the selection and
returns. The rows read the durable queue, which is why a chapter ordered last
night still says "queued" on a phone that has been closed and re-opened since,
and why it cannot be picked a second time.

**Polling stays.** The drawer's two-second `/api/chapters` poll is unchanged
while it is open — it is the fast path and the fallback for a session with no
live stream.

### The trim keeps what you touched

A device that downloads ahead of itself has to give chapters back, or a
1433-chapter novel becomes 8 GB on a phone. The rule was "keep the furthest
chapter reached and the two behind it" — anchored to the furthest point rather
than the current one, so re-reading a scene never widens the trim.

Position alone turned out to be a bad proxy for *finished with it*. Jumping
forward to look something up moves the anchor permanently, and coming back found
every chapter in between deleted — chapters downloaded deliberately, minutes
earlier, over a tunnel, from a box that renders at a quarter of realtime.

So a chapter now has to be **both** well behind the anchor **and** untouched for
`KEEP_UNTOUCHED_MS` (48 h). "Touched" is *stored* as much as read, which is the
part that makes a forward jump survivable: a chapter downloaded ahead and not
reached yet has been touched. The log is `narrator.touched:<key>` in
localStorage, pruned on every trim to what is actually in Cache Storage, so it
stays the size of the wake rather than the size of the book. A chapter with no
entry is treated as cold — otherwise a device that predates the log could never
give anything back.

### Why a stored chapter used to stop being stored

The bug underneath all of it, and the one that made the reader untrustworthy:
chapters shown as saved at 09:03 were shown as merely packed at 09:05, across
nothing but a restart, with nothing in the reader having deleted them (the
auto-trim only ever touches chapters *behind* the anchor).

Every URL the reader stores deliberately is also matched by a Workbox runtime
rule — that is the point of the rules, they are what serves a cached chapter back
to `<audio>` with Range support. But they matched the **download** too, so a
deliberate save ran one streaming body through two consumers at once: the
strategy putting its own copy into `narrator-audio` inside `event.waitUntil`, and
the page putting `res.clone()` into the same entry. On WebKit that fails two
ways, and he saw both in one sitting — `TypeError: FetchEvent.respondWith
received an error` for the loud case, and an entry that reads back fine all
session and is gone after the next launch for the quiet one.

So: a deliberate save carries `x-narrator-store: 1`, every runtime rule in
`web/vite.config.ts` skips a request carrying it, and exactly one writer —
`put` in `web/src/lib/offline.ts` — writes the entry. Three further rules there
follow from the same incident:

- **the body is read to the end into a Blob before it is stored.** A
  `res.clone()` handed to `Cache.put` is a stream the browser finishes on its own
  time and may not finish at all. Reading first costs the chapter's size in
  memory for a moment and buys an entry that either exists or threw.
- **the entry is read back after the put.** `Cache.put` resolving is not the same
  claim as "this entry exists": quota refusals surface here. Reporting success
  for a chapter that will be missing at the next launch is the report he could
  not trust.
- **an entry that is present and declares itself zero bytes reads as missing.**
  Present-and-empty is worse than absent: nothing re-fetches it and it plays
  silence.

`downloadChapter` also retries on the shared backoff curve (`lib/backoff.ts`) —
only failures worth repeating, so a 404 is still one ask — and the reader asks
for `navigator.storage.persist()` once at startup. Not granted on iOS today and
never assumed; on a device that does grant it, it is the difference between a
night of downloads and a morning of them being gone.

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

**espeak-ng is a subprocess, not a library.** libespeak-ng keeps process-global state, is not thread-safe, and a wedge or a segfault inside it would take the server with it. A fork costs a few milliseconds against a chunk that takes a second or two to synthesize, and it **is** killed on a timeout — `ESPEAK_TIMEOUT`'s 15 s, spent polling rather than in `Command::output()`, which waits forever and for a while did. The pipes are drained on their own threads, because a child that fills the 64 KB pipe buffer while the caller polls for its exit is the same hang by another route. `probe()` is bounded too: it runs inside `Engine::load`, on the render thread. It also deletes the Python image's entire `espeakng_loader` symlink surgery: the apt binary and its data are simply what `espeak-ng` on PATH resolves to and can never be a mismatched pair.

**G2P is the fallback half of misaki, not all of it.** Kokoro's real front end looks English words up in a lexicon first and only falls back to espeak-ng; this implements the fallback for every word, with misaki's `EspeakFallback.E2M` mapping table verbatim. Measured cost on the GATE 0 passage: 269.25 s against the PyTorch render's 271.57 s (0.9 %), with per-chunk boundaries lining up.

**What it was getting wrong, and what fixed them.** All reported by ear, all
confirmed against the reference, and none of them a chunking change — the chunk
text, the chunk indices, the manifests and every stored position are untouched.
What changes is the audio, so a chapter already in the cache keeps the old
pronunciation until it is re-rendered; see [what has not been
proven](#what-has-and-has-not-been-proven).

The reference for all of it is misaki itself, which is sitting in the uv cache
the python repo already populated — `misaki/espeak.py` and `misaki/en.py`. Two
of these were found by diffing this module against that file rather than by
guessing.

- **One English word in eight lost a vowel.** `ᵻ` (U+1D7B) is espeak-ng's
  reduced vowel and the last entry in Kokoro's vocabulary; the inlined `VOCAB`
  had it transcribed as U+1DFB, a combining deletion mark — a transposition, one
  wrong entry out of 115. `tokenize` drops what it cannot map, silently and by
  design, so the vowel never reached the model: `before` was synthesized as
  `bfˈɔɹ` and came out "fore", `roses` as `ɹˈOzz`, `wanted` as `wˈɔntd`.
  Synthesizing `bᵻfˈɔɹ` and `bfˈɔɹ` produced byte-identical audio, which is the
  proof the character was never getting through. Measured over 3652 words, `ᵻ`
  was the **only** symbol being dropped and it was dropped 446 times. Now there
  are two tests: one on the symbol, and one that checks the whole inlined
  vocabulary against `models/kokoro/tokenizer.json` where the weights are
  fetched — the vocabulary is model contract, and nothing had ever compared it
  to the model.
- **`E2M`'s first entry never matched.** The same class of typo: misaki's key is
  `ʔ` + U+02CC (espeak-ng's secondary stress) + `n̩`, and it was transcribed with
  U+032C, a combining caron below. A glottalised syllabic `n` kept a stray schwa
  and a stray stress mark instead of collapsing to `ʔn`. The table is now
  asserted to be longest-key-first, which is the ordering misaki's
  `sorted(key=lambda kv: -len(kv[0]))` guarantees and which two entries were out
  of.
- **`I` was dropped at the head of a sentence.** espeak-ng applies sentence
  prosody and de-stresses a subject pronoun — `I think it is fine` phonemizes to
  `a͡ɪ θˈɪŋk …`, no stress mark — where misaki's `us_gold.json` says `ˈI`, full
  stop, and its `cap_stresses` leaves a capitalised word's primary stress alone.
  Kokoro renders the difference as 60 ms at half the amplitude of the word after
  it, against 120 ms at full amplitude, which is why it sounded skipped. That is
  also the argument for forcing it rather than trusting espeak-ng's prosody:
  every `I` in Kokoro's training input carried `ˈ`, so `ˈI` is the distribution
  the model was fitted on. This is **the small override table** the paragraph
  below has been promising: `OVERRIDES` in `src/tts/g2p.rs`, one entry today,
  applied as espeak-ng's own `[[…]]` inline escape rather than a separate call
  per word. That distinction is load-bearing — the escape forces one word's
  pronunciation and leaves the sentence a single utterance, so `so I think`
  keeps `sˌO`'s secondary stress, where phonemizing `I` on its own would make
  each fragment a fresh utterance and give every neighbour citation stress.
  Whole-word matching counts the apostrophe as part of the word, because
  `[['aI]]'m` is read "I em". The escape is espeak-ng's own syntax and so the
  one version-dependent thing in this module: verified against 1.52 on the
  desktop and against **1.51**, which is what `debian:bookworm-slim` carries and
  what the box runs.
- **A word boundary was being eaten.** espeak-ng does not put back the space
  around a run of words, so `Before we begin, he said` came out `bɪɡˈɪn,hi sˈɛd`
  with the two words run together across the comma. Kokoro has a symbol for the
  space and misaki's output always carries one.

### Numbers: `src/tts/numbers.rs`

**espeak-ng expands digits itself, and is good at it.** `1,000` is "one
thousand", `1433` is "one thousand four hundred thirty three", `1.8` is "one
point eight", `1st` is "first", `50%` is "fifty percent", `3:45` is "three
forty-five". All of that is left alone — a second implementation of something
already right is only a second thing to get wrong.

What it was never given a chance at was the number itself. `split_punctuation`
treated every `,` `.` `:` as prosody, so espeak-ng got the halves as separate
utterances with a spoken pause between them: `1,000` was read "one, zero zero
zero", `1.8` was "one. eight", `3:45` was "three: forty-five". A separator is
now punctuation only when it is *not* between two digits, strictly — a digit
immediately either side, so `In 2016.` keeps its full stop and `page 12, line 3`
keeps its comma, because espeak-ng breaks the clause there itself and should.

**Two readings espeak-ng gets wrong even with the whole number**, and both are
fixed the way misaki does it, because misaki is Kokoro's real front end and this
is the half of `Lexicon.get_number` that does not need a POS tagger:

| | espeak-ng alone | now |
|---|---|---|
| `$5` | "dollar five" | "five dollars" |
| `$1,500.50` | "dollar one, five hundred. fifty" | "one thousand five hundred dollars and fifty cents" |
| `£1.50` | "pound one. fifty" | "one pound and fifty pence" |
| `$1.5 million` | "dollar one point five million" | "one point five million dollars" |
| `1066` | "one thousand sixty six" | "ten sixty-six" |
| `1985` | "nineteen hundred eighty five" | "nineteen eighty-five" |
| `1990s` | "nineteen hundred ninety z" | "nineteen nineties" |

Currency is misaki's `CURRENCIES` table and its zero-half rule (`$0.50` is "fifty
cents", not "zero dollars and fifty cents"); the amount stays in digits and
espeak-ng expands it, because espeak-ng's cardinal is already what misaki's
`extend_num` produces. Years are the **`num2words` crate**, which is the same
algorithm as the Python package misaki imports — its year output was checked
against the Python for every year from 1000 to 2100 and is byte-identical across
the range. It is one small pure-Rust dependency and one transitive one, and it
buys the part that is fiddly to get right by hand: "eighteen oh-five",
"nineteen hundred", "two thousand", "ten sixty-six".

Two deliberate departures from misaki, both because the question here is what a
person would say rather than what the reference does:

- **`$1.5` is an amount, not one dollar and five cents.** misaki splits on any
  fraction of fewer than three digits; hundredths here need exactly two. And a
  scale word after the amount moves the unit to the end, which misaki cannot do
  because it phonemizes token by token and never sees the next word.
- **The year reading is bounded, and only for a number standing on its own.**
  misaki applies it to *any* four-digit number, which turns `9999` into
  "ninety-nine ninety-nine"; here it is 1000–2099, and a group joined to more
  digits by `-`, `/` or `:` is left alone, so `555-1066` and `1050-1066` stay
  numbers. Inside that it is still misaki's rule and still a heuristic: `1433
  chapters` becomes "fourteen thirty-three chapters", because nothing here knows
  the difference between a year and a count. Four-digit numbers in prose are
  overwhelmingly years, which is why misaki bets that way and why this does too
  — but it is the one reading in this module that is a guess rather than a fact,
  and the one to narrow if a book starts reading its counts as dates.

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
difference today is 0.9 % of duration with boundaries lining up, and the words
that have been reported wrong were all fixed above without it. Number expansion
turned out to be mostly espeak-ng's job already, with misaki's currency and year
rules ported on top of it; the one lexicon difference that mattered (`I`) is one
line of `OVERRIDES`. That table is the cheap fix this paragraph used to promise,
and it is where the next word goes: add it with a test. What is still missing is
only the tagger — `read`, `lead`, `live`, `bow`, `close`, `record` — and none of
the above is needed until the override list is long enough to be a lexicon.

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

So: parity on synthesis, within noise — on a desktop. The box is a different
machine and a different conclusion; see [the A1](#the-a1-measured).

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

### The A1, measured

The Oracle A1 — 2 cores, 11 GB — is the machine that matters, and none of the
numbers above came from it. These do, taken on the running box while it was
rendering *Lord of Mysteries* (which is the normal condition, not a spoiled
measurement: this box is always rendering something):

| | A1 | this 16-core x86 |
|---|---|---|
| Kokoro synthesis (`/api/status`'s `rtf`, cumulative over hours) | **0.26–0.30×** realtime | 4.3–4.9× |
| `GET /api/chapters`, 1433 rows | 25 ms cold, 7 ms memoised, 273 kB | 9 ms, 244 kB |
| `GET /api/chapters?from=0&to=30` | 2–26 ms, 6.0 kB | 12 ms, 5.3 kB |
| `GET /api/chapters?from=700&to=760` | 1.5–4.7 ms, 11.8 kB | — |
| `GET /api/status` | 20 ms, 631 B | — |
| `POST /api/load`, cached plan | 0.30 s | 0.36 s |

**Synthesis runs at about a quarter of realtime here** — roughly four seconds of
compute per second of audio, against 4.5× *faster* than realtime on the desktop.
That is the most consequential number in this file, and it is not a regression:
it is 2 ARM cores against 16 x86 ones. What it means is that the box can never
render a book as fast as anyone listens to it, so the prerender span, the
download-ahead drawer and the chapter queue are not optimisations — they are the
only reason the reader works at all. A 12-minute chapter is the better part of an
hour of rendering, and a 74-chapter download is an overnight job.

**`/api/chapters` pagination: measured, and not needed.** The full 1433-row
response is 273 kB and 25 ms cold on the A1 — 7 ms while the 1.5 s memo holds,
which is most polls — and the drawer polls it every two seconds only while it is
open. A cursor would add API surface, a second code path through the row scan and
a new way for the reader's list to be half-built, to save a quarter of a
megabyte over a tailnet on the one screen that is *about* those rows. The window
that already exists (`?from=&to=`, requirement 6) covers the case that actually
wanted paging — a reader that only shows a screenful — at 6 kB and 2 ms. So: no
cursor, deliberately, until a number says otherwise.

### Whisper on the A1

**Measured at last, and it is the bad news of this round.** A 10.0 s memo
(synthetic speech, webm/opus, exactly the payload the reader posts), against a
throwaway container on the box with a temp work dir and no vault mounted:

| `large-v3-turbo-q5_0` | wall | × realtime |
|---|---|---|
| 2 threads (the default: every core), first call — pays the 574 MB model load | 311 s | 31× |
| 2 threads, second call, model resident | 473 s | 47× |
| 1 thread (`WHISPER_THREADS=1`), model resident | 534 s | 53× |

The transcript was word-perfect — `large-v3-turbo` is a good model and the prompt
biasing works — and that is the only good part. **Thirty to fifty times realtime
means a one-minute memo is half an hour of CPU.** Two threads beat one, so the
default (every core) stays; but the second call being *slower* than the first,
with the model already resident, says what the real constraint is. The renderer
had the other core. Whisper took about one core's worth (≈ 95–99 % of one CPU)
either way, because on a 2-core box the other core is always busy synthesizing,
and every one of these numbers was taken with it busy — which is the honest
condition to measure in, since that is when a memo actually gets recorded. The
thread count is not the lever.

The lever is the model, and the options are:

1. **A smaller ggml model.** `small` is ~6× less compute than `large-v3-turbo`
   and `base` ~20×; either would bring a 10 s memo under a minute. The cost is
   accuracy on proper nouns — which `WHISPER_PROMPT` plus the book and chapter
   titles already exist to patch. This is the obvious first move: change
   `WHISPER_MODEL` in `/etc/narrator-rs.env`, fetch the ggml file into
   `models/whisper/`, restart. **Not done here** — it is a quality trade only
   Fernando can judge, and it wants his ear on a real memo.
2. **Transcribe somewhere else.** The memo is already a queued blob in the
   reader's outbox; nothing says the transcriber has to be the reader's server.
   A bigger machine (his desktop) with the same endpoint would do, at the cost
   of a second deployment target.
3. **Accept the latency.** Nothing breaks today: the outbox holds the recording
   until a 2xx comes back, the note is written minutes later, and Fernando is
   not waiting on the screen for it. The failure mode is not data loss, it is a
   note that appears in the vault five minutes after the thought.

What is *not* an option is leaving this undocumented, which is why it is here:
the voice-memo path works, and it works at a tenth of the speed anyone would
guess from the desktop numbers.

### The STT priority gate

**Whisper outranks the renderer and the packer, and they stand down for it.**
The measurement above is the argument: two ARM cores, Kokoro already at 0.26×
realtime, whisper at 30–50×, and with twenty chapters queued a ~45 s memo took
**over seven minutes** while the render RTF fell to 0.26. Neither job got a
machine.

The tie-break is not speed, it is what is recoverable. A memo exists in exactly
one place — IndexedDB on the phone that recorded it — until `/api/note` answers
2xx. A rendered chunk is a file the server can make again from a book it still
has. So:

* `Whisper::transcribe` takes an RAII claim on `SttGate` (`src/stt.rs`) at its
  very first line, and holds it across the ffmpeg decode, the **wait** for the
  model mutex, and the transcription. Covering the wait is the point: two memos
  arriving together park the renderer once, for both, instead of letting it wake
  up in the gap between them.
* The render worker parks between chunks — a bounded wait on the gate's condvar,
  so it restarts the instant the last transcription ends. Parking is **not** a
  render failure and does not touch the failure backoff.
* The packer holds back a *new* encode. One already running is left to finish:
  killing ffmpeg mid-chapter throws away every second it has spent and the
  chapter has to be packed again from nothing, which costs the box more than the
  transcription gains — and the packer is one chapter at a time, so the wait is
  bounded by one encode either way.
* `/healthz` does not call a parked renderer a stall. A one-minute memo is half
  an hour of not rendering on the A1, comfortably past `HEALTH_STALL_S`, and the
  watchdog restarting the container over it would kill the transcription every
  time it was retried. A stall that is explained is not a stall.
* Both threads log the park and the resume with its duration. A renderer that has
  quietly stopped is the exact shape of the bug this round was about; "it is
  parked for a memo" is only reassuring if it is written down.

Release is `Drop` and nothing else — no manual path to forget. That is also what
makes it safe under cancellation: `/api/note` transcribes inside
`spawn_blocking`, and a phone that locks mid-memo drops the *request future*
while the blocking job runs on. The claim belongs to the closure, not the future,
so it goes when the work does. `tests/parity_render.rs` asserts exactly that,
along with the renderer parking and resuming and the packer holding back.

`WHISPER_THREADS` still defaults to every core, and on the A1 that finally means
something: with the renderer parked, both cores are actually free for the
transcription rather than one of them being Kokoro's.

## Config surface

Every name the Python `AGENTS.md` documents, with the same default: `NARRATOR_PORT` (7870), `NARRATOR_VAULT`, `NARRATOR_WORK`, `NARRATOR_BOOKS`, `NARRATOR_WEB`, `BOOKS_SUBDIR`, `POSITIONS_SUBDIR` (`02 - Studies`), `NOTES_SUBDIR` (`05 - Fleeting`), `KOKORO_VOICE` (`af_heart`), `KOKORO_SPEED`, `KOKORO_GAIN` (1.0), `LOOKAHEAD` (80), `PRERENDER_CHAPTERS` (2), `PREFETCH_WHILE_PAUSED`, `MAX_AUDIO_GB` (5), `MAX_CHAPTER_GB` (20), `SILENCE_S` (0.5), `WHISPER_MODEL`, `WHISPER_PROMPT`, `WHISPER_THREADS` (every core), `CHAPTER_BITRATE` (`64k`), `CHAPTER_GAP_S` (0.30), `CHAPTER_PARA_GAP_S` (0.60), `HLS_SEGMENT_S` (6), `TEXT_SHARD_BYTES`, `TEXT_SHARD_CHAPTERS`, `CHAPTER_PHRASE_GAP_S` (0.10), `HEALTH_STALL_S` (300), `AUTOPACK`, `AUTOPACK_EVERY_S`,
`IDLE_RENDER` (on) and `IDLE_CEILING` (0.80) — see [never idle](#never-idle-what-the-worker-does-when-there-is-nothing-to-do); the ceiling is clamped to 0.85 on read, because a typo above the collector's trim floor would be a treadmill nobody could see, `NARRATOR_WATCH_BOOKS`, `NARRATOR_FAKE_TTS`, `SSE_HEARTBEAT_S`, `SSE_QUEUE`, `SSE_RENDER_MIN_S`, `SSE_RETRY_MS`.

New here, because the weights are not downloaded by a Python package on first use: `NARRATOR_MODELS` (`/models`), `KOKORO_MODEL`, `KOKORO_VOICES`, `WHISPER_VAD_MODEL`, `ESPEAK_BIN`, `ESPEAK_VOICE`, `ESPEAK_TIMEOUT` (15 s, seconds, after which the subprocess is killed), `QUEUE_RESUME_DELAY_S` (10 s, how long after startup a wishlist left by the last process may start rendering — the note queue's own startup sweep may be claiming one of the box's two cores). `HF_HOME` and `KOKORO_REPO` are gone — nothing here talks to Hugging Face at runtime.

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

- **The pronunciation fixes have not been heard.** The defects in
  [the engine notes](#engine-notes) are proven gone at the phoneme and token
  level, the number readings are asserted as text, and the suite checks all of
  it against the real binary — but nobody has listened to a chapter rendered
  with them. They also make the cache
  inconsistent with itself: every chunk already on the box was rendered with the
  reduced vowel missing and the numbers spelled out digit by digit, so a book
  mid-render changes pronunciation at the frontier. Positions, chunk indices and
  manifests are untouched — this is not a chunker migration — so the way to
  collect the fix on an already-rendered book is to delete its `work/audio/<key>`
  and its packed chapters and let the worker fill them in again, which on the A1
  is the overnight job [the A1 numbers](#the-a1-measured) describe. That is
  Fernando's call to make per book.
- **The listening verdict is Fernando's and has not been given.** GATE 0 is
  rendered and waiting (`./narrator listen-test`), and the ≈ +3 dB delta above is
  still undecided — which is why `KOKORO_GAIN` exists and is **not** set on the
  box. Setting it mid-book would make a novel that changes loudness at the render
  frontier, so it wants a decision and a re-render, not a quiet flip.
- **The soak is hours, not days.** The box has now rendered *Lord of Mysteries*
  for hours at a stretch under real use, with a 1433-chapter cache and a
  20-chapter download queue, and the endpoint numbers above were taken in the
  middle of it. Resident memory on the box settles around 1.9–2.3 GB while
  rendering (the desktop reaches ~1.24 GB in five minutes; the box has been at it
  far longer, and 11 GB of RAM makes the difference academic) — consistent with
  ONNX Runtime's arena rather than a leak, but *consistent with* is not *proven
  not to be*. What still has no evidence either way: gc churn at the
  `MAX_AUDIO_GB` cap over days, and whether anything drifts across thousands of
  sessions.
- **The A1 renders at a quarter of realtime, and whisper there is 30–50× it.**
  Both are measured now ([Performance](#the-a1-measured)), and neither is a bug —
  they are 2 ARM cores. The consequence is a design constraint rather than a
  to-do: nothing about this reader can assume the server keeps up with a
  listener, and the voice-memo path is minutes, not seconds. A smaller whisper
  model is the obvious lever and is Fernando's call to pull.

## Rules

- **Never edit `~/git/narrator`.** It is the reference and it is someone else's working tree — including its `web/`, which is now a historical copy of this repo's reader. The reader is edited **here**.
- **`web/src/client/` is generated.** Never hand-edit a `.gen.ts`; change the Rust and regenerate.
- **This repo is public.** Anything committed is on the internet: no tailnet addresses, no hostnames, no tokens, no vault contents. The reader talks to the API by relative path and has nothing to leak; keep it that way.
- **Never touch production.** The VPS is deployed by hand.
- No `unwrap()` or `expect()` outside tests and the startup path. Errors are typed (`thiserror`), the edges use `anyhow`, and every failure path logs and degrades.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are clean, and stay clean.
- A change to any response shape means regenerating in the same commit: `./narrator client`, then commit `openapi.json` and `web/src/client/` with the Rust change. CI fails otherwise, and the reader's types come from those files.
- If the chunker's output changes, that is a **migration**, not an edit — it invalidates every cache and every stored position. Say so out loud before doing it.
