# Drift: reader contract vs generated client

Generated 2026-09-11 19:29 by `scripts/drift-check.mjs`.

- hand-written contract: `/home/fernando/git/narrator/web/src/lib/types.ts` (read-only)
- generated client: `client/types.gen.ts`
- spec: `openapi.json`

A **MISMATCH** breaks `generated → hand-written` assignability: the reader
cannot compile against the generated client until it is resolved, on one side
or the other. **NOTE** and **ADDITIVE** are safe and do not fail the build.

## Types

### BookFile → BookFile

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| NOTE | `key` | missing-optional | `key?: string` | `absent from the generated type` |

- `key` — the reader already treats it as possibly absent, so nothing breaks

### ChapMeta → ChapMeta

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| NOTE | `est_min` | nullability | `est_min: number \| null` | `est_min: number` |

- `est_min` — generated never sends null where the reader allows it — assigns fine, the reader's guard is now dead code

### SavedPosition → StampedPosition  *(renamed)*

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| NOTE | `chapter_title` | now-always-sent | `chapter_title?: string` | `chapter_title: string  (always present)` |
| NOTE | `chapters_total` | now-always-sent | `chapters_total?: number` | `chapters_total: number  (always present)` |
| NOTE | `chunks_total` | now-always-sent | `chunks_total?: number` | `chunks_total: number  (always present)` |
| NOTE | `updated` | now-always-sent | `updated?: string` | `updated: string  (always present)` |
| ADDITIVE | `updated_ms` | extra | `not in the hand-written type` | `updated_ms?: number \| null` |

- `chapter_title` — server is stricter than the reader assumed — safe
- `chapters_total` — server is stricter than the reader assumed — safe
- `chunks_total` — server is stricter than the reader assumed — safe
- `updated` — server is stricter than the reader assumed — safe

### LoadResult → LoadResult

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| NOTE | `position` | now-always-sent | `position?: SavedPosition \| null` | `position: StampedPosition \| null  (always present)` |

- `position` — server is stricter than the reader assumed — safe

### ChapRow → ChapterRow  *(renamed)*

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| NOTE | `pack_queued` | now-always-sent | `pack_queued?: boolean` | `pack_queued: boolean  (always present)` |
| NOTE | `packing` | now-always-sent | `packing?: boolean` | `packing: boolean  (always present)` |
| NOTE | `queued` | now-always-sent | `queued?: boolean` | `queued: boolean  (always present)` |
| NOTE | `shard` | missing-optional | `shard?: number` | `absent from the generated type` |

- `pack_queued` — server is stricter than the reader assumed — safe
- `packing` — server is stricter than the reader assumed — safe
- `queued` — server is stricter than the reader assumed — safe
- `shard` — the reader already treats it as possibly absent, so nothing breaks

### ChaptersResult → ChaptersResult

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| ADDITIVE | `from` | extra | `not in the hand-written type` | `from?: number` |
| ADDITIVE | `to` | extra | `not in the hand-written type` | `to?: number` |
| ADDITIVE | `total` | extra | `not in the hand-written type` | `total?: number` |

### Status → Status

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| ADDITIVE | `pack_queue` | extra | `not in the hand-written type` | `pack_queue: number[]` |
| ADDITIVE | `pack_want` | extra | `not in the hand-written type` | `pack_want: number[]` |

### BookIndex → BookIndex

Identical. No drift.

### TextShard → TextShard

Identical. No drift.

### ChapterText → ChapterText

_No mismatches._

| verdict | field | kind | reader expects | generated has |
|---|---|---|---|---|
| NOTE | `paras` | now-always-sent | `paras?: number[]` | `paras: number[]  (always present)` |
| ADDITIVE | `i` | extra | `not in the hand-written type` | `i: number` |

- `paras` — server is stricter than the reader assumed — safe

### NoteResult → NoteResult

Identical. No drift.

## Endpoints

Every URL `api.ts` builds, checked against `openapi.json` (32 routes).

- ok — `GET /api/book.json`
- ok — `GET /api/books`
- ok — `GET /api/chapter/{ci}`
- ok — `GET /api/chapters`
- ok — `GET /api/chapters/{ci}.json`
- ok — `GET /api/chapters/{ci}.m3u8`
- ok — `GET /api/chapters/{ci}.m4a`
- ok — `GET /api/status`
- ok — `GET /api/text/{s}.json`
- ok — `POST /api/chapters/build`
- ok — `POST /api/chapters/cancel`
- ok — `POST /api/chapters/render`
- ok — `POST /api/load`

## Summary

- 0 mismatches (breaking)
- 12 notes (safe: optional field dropped, or now always sent)
- 7 additive fields (generated only)
- 0 missing endpoints

```
DRIFT: 0 mismatches, 7 additive, 0 missing endpoints
```
