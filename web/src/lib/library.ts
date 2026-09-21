/**
 * What a *book's* readiness actually is, in words.
 *
 * lib/chapterstate.ts one level up, and for the same reason. A chapter row could
 * always say where in the render → pack → download pipeline it was; a book row
 * could say a name and the size of its epub, which answers nothing anybody asks
 * of a library. Every real question — is this rendered? is any of it packed? how
 * much of it could I take with me right now? — could only be answered by
 * *opening* the book, which loads it server-side, moves the one global session
 * and drags the render frontier onto it, and then opening the chapter drawer.
 * That is an expensive way to ask a cheap question, and on the phone it is
 * exactly the question you want answered *before* you commit to a book.
 *
 * `/api/library` answers it from a cached index, so this module is only the
 * reading of that answer. Kept out of the component for the same reason
 * chapterstate is: a ladder that can be tested rather than eyeballed.
 *
 * The types come straight from `@/client` (generated from the server's OpenAPI
 * document), not from a transcription here — a field that moves in Rust is a
 * type error in this file in the same change.
 */
import type {LibraryRow, Library} from './types';
import type {ChapterState} from './chapterstate';

/**
 * The same five tones the chapter rows use, imported rather than re-declared.
 *
 * The drawer has one colour vocabulary and the book level is the top of it; a
 * second `'ok' | 'work' | ...` written out here would be free to drift into
 * meaning something else, and the two lists are read one above the other.
 */
export type Tone = ChapterState['tone'];

export type LibraryPhase = 'none' | 'partial' | 'rendered' | 'ready' | 'complete';

export interface LibraryState {
  key: LibraryPhase;
  /** the terse part, on the row */
  text: string;
  /** the whole sentence, on hover */
  tip: string;
  tone: Tone;
}

/**
 * The ladder, most-finished first — the only order in which a partly-true row
 * cannot be misread, which is the same rule `phaseFor` follows in lib/download.ts.
 *
 * Two rungs are placed on purpose and the placement is the whole design:
 *
 * **`ready` sits above `rendered`, even when it is by far the smaller number.**
 * One packed chapter out of 1433 outranks a whole book rendered and packed
 * nowhere, because the question this row exists to answer is "what could I take
 * with me right now" and a rendered chunk is not a file any device can hold.
 * Packing is what turns the box's night of work into something downloadable, so
 * the first packed chapter is the news and the last unrendered one is not.
 *
 * **`partial` is decided on chunks, never on chapters.** `rendered_chapters`
 * counts only whole ones, so on *Lord of Mysteries* the renderer can spend hours
 * inside chapter one and the row still reads "0 of 1433" — a book that is
 * genuinely underway, indistinguishable from one nothing has ever touched.
 *
 * `complete` and `rendered` both require `chapters > 0`: a book whose plan has
 * not been scanned yet reports zero of everything, and `0 >= 0` would call that
 * finished.
 */
export function libraryState(b: LibraryRow): LibraryState {
  if (b.chapters > 0 && b.packed_chapters >= b.chapters) return {
    key: 'complete', text: 'packed', tone: 'ok',
    tip: `every chapter is packed on the server — all ${b.chapters} of them can be downloaded now`,
  };
  if (b.packed_chapters > 0) return {
    key: 'ready', text: `${b.packed_chapters}/${b.chapters}`, tone: 'done',
    tip: `${b.packed_chapters} of ${b.chapters} chapters are packed and can be downloaded now`
       + ' — the rest have to be rendered and packed first',
  };
  if (b.chapters > 0 && b.rendered_chapters >= b.chapters) return {
    key: 'rendered', text: 'rendered', tone: 'done',
    tip: 'every chapter is rendered on the server, none packed yet — downloading packs them first',
  };
  if (b.rendered_chunks > 0) return {
    key: 'partial', text: `${renderedPct(b)}%`, tone: 'part',
    tip: `partly rendered: ${b.rendered_chunks} of ${b.total_chunks} chunks have audio`,
  };
  return {
    key: 'none', text: 'not rendered', tone: 'none',
    tip: 'nothing rendered yet — readable either way, and opening it points the renderer here',
  };
}

/**
 * How far the render is, in percent of **chunks**.
 *
 * The two clamps are the same rule in both directions: 0 % and 100 % are the
 * only two numbers on this scale that mean something categorical — "nothing has
 * happened here" and "you can stop waiting" — so neither may be reached by
 * rounding. One chunk of 118831 reads as 1 %, and one chunk short of the whole
 * book reads as 99 %.
 *
 * Division by zero is a book whose plan has never been scanned, which is 0 and
 * not NaN: a row must not print `NaN%` because an index is a minute behind.
 */
export function renderedPct(
  b: Pick<LibraryRow, 'rendered_chunks' | 'total_chunks'>,
): number {
  const done = b.rendered_chunks;
  const all = b.total_chunks;
  if (!(all > 0) || !(done > 0)) return 0;
  if (done >= all) return 100;
  return Math.min(99, Math.max(1, Math.round((done / all) * 100)));
}

export interface LibraryCost {
  /** Measured, not estimated: the packed m4as that exist right now. */
  packed: number;
  /**
   * The rest of the book, estimated. `null` means it cannot be estimated —
   * the box's bitrate has not arrived, or nothing has been scanned — in which
   * case `total` is a floor rather than an answer.
   */
  rest: number | null;
  /** `packed + (rest ?? 0)`: what the whole book would weigh on this device. */
  total: number;
}

/**
 * What taking this book with you would cost, in bytes.
 *
 * **`bytesPerMin` is a parameter and there is no default on purpose.** It is
 * `/api/status`'s `bitrate_bytes_per_min`, the box doing the arithmetic with the
 * constant it actually has. The reader used to hard-code 64 kbit/s, so changing
 * `CHAPTER_BITRATE` on the box made every size in the UI wrong by exactly that
 * ratio, silently — which is why the server reports it at all (CLAUDE.md
 * requirement 8). A missing rate here produces `rest: null`, which the row says
 * out loud, rather than a confident number computed from a guess.
 *
 * The unpacked part is prorated by *chapters* rather than taken from the chunk
 * counts, because `est_min` is the whole book's spoken length and chapters are
 * the unit that gets packed. It is an estimate either way; the row marks it.
 */
export function downloadCost(
  b: Pick<LibraryRow, 'packed_bytes' | 'packed_chapters' | 'chapters' | 'est_min'>,
  bytesPerMin?: number | null,
): LibraryCost {
  const packed = Math.max(0, b.packed_bytes);
  const left = b.chapters - b.packed_chapters;
  if (!(b.chapters > 0) || left <= 0) return {packed, rest: 0, total: packed};
  const rate = typeof bytesPerMin === 'number' && bytesPerMin > 0 ? bytesPerMin : null;
  if (rate == null || b.est_min == null) return {packed, rest: null, total: packed};
  const rest = Math.max(0, Math.round(b.est_min * (left / b.chapters) * rate));
  return {packed, rest, total: packed + rest};
}

/**
 * "ch 412 / 1433" — the single most useful thing on a book row.
 *
 * A percentage would be the obvious thing and it is the wrong one: picking a
 * book back up is a question about *where*, and a chapter number is what the
 * drawer, the player and the vault's own record all say. `chapter` is the
 * zero-based `c.i` everything else in the reader uses, so it is printed one
 * higher, exactly as the diagnostics line prints the chapter being packed.
 *
 * The book's own `chapters` wins over the record's `chapters_total` when it has
 * one: the record was written whenever it was last read and the book may have
 * been re-chunked since, and the denominator on screen should be the book that
 * is there now.
 */
export function positionLine(b: LibraryRow): string | null {
  const p = b.position;
  if (!p) return null;
  const total = b.chapters > 0 ? b.chapters : p.chapters_total;
  return total > 0 ? `ch ${p.chapter + 1} / ${total}` : `ch ${p.chapter + 1}`;
}

/**
 * Two missed ticks of the five-minute scanner, plus a minute of slack.
 *
 * One missed tick is a box that was busy — it renders at a quarter of realtime
 * and a scan queues behind that, routinely. Two is a scanner that has stopped,
 * and the difference matters because every number on these rows comes from it:
 * a stale index does not look broken, it looks like a book that has not been
 * rendered, which is the one failure this whole screen could produce silently.
 */
export const SCAN_STALE_MS = 11 * 60_000;

/** Is the worst row in the answer old enough to be worth saying so? */
export function scanStale(scannedMs: number | null | undefined, now = Date.now()): boolean {
  if (scannedMs == null) return true;
  return now - scannedMs > SCAN_STALE_MS;
}

/**
 * `scanned_ms` in words.
 *
 * It is the **oldest** stamp in the answer, so this is how stale the worst row
 * is rather than how fresh the best one is — one book nobody has walked since
 * yesterday cannot hide behind eleven walked a minute ago.
 *
 * A stamp in the future is clock skew between the box and the phone, not a
 * negative age: it reads as "just now" rather than "-3m ago". Minutes floor and
 * hours round, which keeps the sequence monotonic across the 90-second boundary
 * — rounding there would step from "just now" straight to "2m ago".
 */
export function scanAge(scannedMs: number | null | undefined, now = Date.now()): string {
  if (scannedMs == null) return 'never scanned';
  const age = Math.max(0, now - scannedMs);
  if (age < 90_000) return 'just now';
  if (age < 60 * 60_000) return `${Math.floor(age / 60_000)}m ago`;
  if (age < 2 * 60 * 60_000) return 'an hour ago';
  if (age < 24 * 60 * 60_000) return `${Math.round(age / (60 * 60_000))}h ago`;
  return `${Math.floor(age / (24 * 60 * 60_000))}d ago`;
}

/**
 * The answer, as a lookup by cache key.
 *
 * The list on screen is `/api/books` (or, offline, localStorage), which is about
 * *files*; this is about what has been made of them. They meet on the key, which
 * the reader derives with `bookKey` for a row the server did not name — so a
 * book that has never been opened on this device still finds its row.
 */
export function indexBooks(r: Library | undefined): Map<string, LibraryRow> {
  const m = new Map<string, LibraryRow>();
  for (const b of r?.books ?? []) m.set(b.key, b);
  return m;
}

/**
 * Most recently opened first, never-opened last.
 *
 * The same order `Store::recent_books` hands the render worker's idle branch,
 * and the agreement is the point rather than a coincidence: when the box has
 * nothing under a playhead to render it works down the library in exactly this
 * order, so the top of this list is the book it is spending its night on. A
 * library sorted by filename would put that book anywhere.
 *
 * Generic over the row because the list on screen is `BookFile`, not
 * `LibraryRow` — and because of the case that matters most: **a row the library
 * answer says nothing about keeps its place.** Offline there is no answer at
 * all, every comparison is a tie, and `Array.sort`'s stability leaves the
 * localStorage fallback in the order it came in rather than shuffling it.
 */
export function orderBooks<T>(
  rows: readonly T[],
  keyOf: (row: T) => string,
  lib: ReadonlyMap<string, LibraryRow>,
): T[] {
  const at = (row: T): number | null => lib.get(keyOf(row))?.last_open_ms ?? null;
  return [...rows].sort((a, b) => {
    const x = at(a);
    const y = at(b);
    if (x == null && y == null) return 0;
    if (x == null) return 1;
    if (y == null) return -1;
    return y - x;
  });
}
