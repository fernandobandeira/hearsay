# Requirements for the Rust server

Things the reader needs from the server that the Python one does not do. Nothing
here was fixed in `app/` - the Python side is being replaced, so each item is
written as a requirement for the rewrite, with the client-side mitigation that is
in place meanwhile and what it costs.

Measured on the 1433-chapter *Lord of Mysteries* (7.8 MB EPUB, 17.5 MB of text,
12 shards), 2026-09-11.

## 1. `/api/load` must not re-parse the book to answer

**Measured: 12.4 s.** Every `POST /api/load` re-parses the EPUB, re-chunks it and
rewrites the whole text bundle, even when nothing about the file has changed. The
reading position comes back in that response, so *opening a book you have never
opened on this device cannot paint anything until the parse finishes*.

Requirement: cache the parse by (path, size, mtime) and answer
`{title, key, total_min, position, chapters}` from the cache in milliseconds.
Rebuild the shards only when the parse result actually differs - the reason they
are rebuilt today is that a moved chunk boundary would put stored positions on the
wrong words, which is a *content* change, not a call.

Mitigation in the reader: a book this device has opened before now paints from its
own cached copy first (`openBook`'s fast path in `src/state.tsx`) and reconciles
with the server's position when it arrives. A book this device has *never* opened
still waits, behind a skeleton, for as long as the parse takes.

## 2. `/api/chapter/{ci}` must take `?book=`

It answers for whichever book the process last loaded and ignores the `?book=`
query the reader sends. Every other cacheable endpoint (`/api/book.json`,
`/api/text/{s}.json`, `/api/chapters/{ci}.{m4a,json,m3u8}`) is keyed by the book,
which is what makes them safe to cache and safe to ask for at any time.

This one asymmetry is the reason first paint is coupled to `/api/load` at all: the
cheapest possible "give me the words of chapter 576" cannot be asked until the
server has been told which book it is on.

Requirement: `GET /api/chapter/{ci}?book=<key>` serves from that book's text
bundle, with no session involved.

Mitigation: the reader only uses the endpoint when it knows the server holds the
book (`serverHolds` in `src/state.tsx`); otherwise it reads the shard the bundle
left in Cache Storage, or waits.

## 3. Positions need an unambiguous timestamp

`save_position()` writes `datetime.now().isoformat(timespec="seconds")` - a naive
local stamp with no zone - and `/api/load` hands it back as `position.updated`.
The browser cannot know the server's timezone, and a container without
`/etc/localtime` mounted stamps UTC while the same reader's other positions are
local. Two of my test containers wrote the same file three hours apart while
agreeing on the wall clock.

Requirement: return epoch milliseconds (`updated_ms`), or an offset-aware RFC 3339
stamp. Better still, a monotonically increasing revision per book, since what the
reader actually needs is "is this newer than the one in my outbox", not a date.

Mitigation: `src/lib/resume.ts` parses the naive stamp as local time and refuses to
trust one more than five minutes in the future, letting a known-undelivered local
position win instead. Tested (`resume.test.ts`), but it is a guess standing in for
a fact.

## 4. Playback endpoints are session-scoped, so they can be aimed at the wrong book

`/api/open` and `/api/playhead` act on the one global session `S`. If the reader
opens a chapter while the server still holds another book, they move the wrong
book's render frontier and can save a position into the wrong record.

Requirement: both take the book key, like `/api/position` already does.

Mitigation: the reader suppresses `/api/open` unless it has just loaded that book.
The cost is that the optimistic first paint does not start the renderer; the real
open, moments later, does.

## 5. A missing vault path must not silently disable positions

With `NARRATOR_VAULT` set to a path that does not exist, `POS_DIR` becomes
`<missing>/02 - Studies`: every write fails (`/api/position` → 500 "Permission
denied") and every read returns `{}`, so `/api/load` reports `position: null` and
every book opens at chapter one. The CLI hides this by omitting the variable when
the directory is absent, so it only bites a hand-rolled `docker run` - but the
failure mode is "your reading positions quietly stop existing".

Requirement: validate the vault at startup; if it is not a writable directory, fall
back to the work dir and say so in the health check.

## 6. `/api/chapters` is a full scan, polled

The drawer polls it every 2 s while it is open, and for 1433 chapters that is a
few thousand `stat` calls per poll (memoised for 1.5 s, so most polls are free, but
every third one is not). The reader also renders all 1433 rows.

Requirement: a range or cursor (`?from=&to=`), and a cheap "what changed" - the
queue and the currently-building chapter are the only fast-moving parts.

Mitigation: none. It is tolerable today and the drawer is only polled while open.

---

Added 2026-09-11, when the reader collapsed "render" and "download" into one
action. Download now owns the whole server-side pipeline - queue the render,
wait for it, ask for the pack, wait for it, store the m4a (`src/lib/download.ts`)
- because rendering without downloading was never a thing anyone wanted. That
made three more gaps load-bearing.

## 7. The chapter endpoints must take `?book=`

`GET /api/chapters`, `POST /api/chapters/render`, `POST /api/chapters/build` and
`POST /api/chapters/cancel` all act on the one global session, exactly like
`/api/open` and `/api/playhead` in item 4 - and unlike every cacheable endpoint,
which is keyed by the book. Before, that only risked a misdirected *view*. Now a
single tap can queue 74 chapters of rendering, and if the server has swapped
books underneath (the watcher picked up a new epub, the Obsidian plugin opened
something, a second device loaded another book) those 74 renders land on the
wrong novel and occupy the worker for hours.

Requirement: the same `book` parameter item 4 asks for, with the same 409 on a
mismatch. It is the difference between a wasted request and a wasted afternoon.

Mitigation in the reader: none that closes it. The chapters query is only
enabled while the drawer is open on a book the reader believes is loaded, and
`serverHolds` already gates `/api/open` - but nothing stops the server changing
its mind between the poll and the tap. The window is small and the cost is
large, which is the wrong shape for a mitigation.

## 8. A chapter's packed size is not knowable before it is packed

The confirm bar has to say how big a download will be *before* anything is
rendered, and an unrendered chapter has never existed as a file. `est_min` is
there, so the estimate is `est_min x CHAPTER_BITRATE` - but the server does not
report `CHAPTER_BITRATE`, so the client hard-codes its default (64 kbit/s, i.e.
~480 kB a minute) in `estimateBytes`. Change the env var on the box and every
estimate in the UI is silently wrong by that ratio.

Requirement: either report `bitrate` on `/api/status` (cheapest), or put an
`est_bytes` on each `/api/chapters` row and let the server do the arithmetic it
actually has the constants for.

Mitigation: the figure is prefixed `~` whenever anything in the selection is an
estimate rather than a measured `bytes`, so it reads as a promise about the
order of magnitude and not about the number.

## 9. `POST /api/chapters/build` does not say what it refused

It answers `{built, building, rendering}`, and a chapter that is not fully
rendered appears in none of the three - indistinguishable from a chapter that
was never asked about. The ladder wants "I have taken this one" or "not yet, and
here is why"; it gets silence.

Requirement: a fourth list, or per-chapter statuses. The useful shape is the one
the reader's own state machine uses: `stored | rendering | packing | ready`.

Mitigation: `phaseFor` never trusts the build response at all. It re-reads the
chapter's row from the next `/api/chapters` poll and decides from that - the
same disk-truth rule the server's render worker follows, for the same reason.
The cost is up to 1.5 s of latency per rung and a `request-pack` that can be
sent twice if a poll lands between the call and the server's own bookkeeping;
`/api/chapters/build` is idempotent, so the second one is merely wasted.
