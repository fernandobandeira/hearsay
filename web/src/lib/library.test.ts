import {describe, expect, test} from 'vitest';
import {
  SCAN_STALE_MS, downloadCost, indexBooks, libraryState, orderBooks, positionLine,
  renderedPct, scanAge, scanStale,
} from './library';
import type {LibraryBook, LibraryResult, StampedPosition} from '@/client';

/** A book nothing has been done to yet: 1433 chapters, 118831 chunks, no audio. */
const book = (p: Partial<LibraryBook> = {}): LibraryBook => ({
  key: '01 - Lord of Mysteries',
  name: '01 - Lord of Mysteries.epub',
  path: '/books/01 - Lord of Mysteries.epub',
  title: 'Lord of Mysteries',
  chapters: 1433,
  last_open_ms: null,
  rendered_chapters: 0,
  packed_chapters: 0,
  packed_bytes: 0,
  total_chunks: 118_831,
  rendered_chunks: 0,
  est_min: 18_000,
  position: null,
  loaded: false,
  ...p,
});

const at = (chapter: number): StampedPosition => ({
  chapter, chapter_title: `${chapter + 1}: a chapter`, chapters_total: 1433,
  chunk: 3, chunks_total: 76, updated: '2026-09-21T09:03:00', updated_ms: 1,
});

describe('libraryState - the ladder', () => {
  test('nothing rendered', () => {
    const s = libraryState(book());
    expect([s.key, s.text, s.tone]).toEqual(['none', 'not rendered', 'none']);
    expect(s.tip).toMatch(/readable either way/);
  });

  test('one chunk of a 1433-chapter book is underway, not nothing', () => {
    const s = libraryState(book({rendered_chunks: 1}));
    expect([s.key, s.text, s.tone]).toEqual(['partial', '1%', 'part']);
    expect(s.tip).toBe('partly rendered: 1 of 118831 chunks have audio');
  });

  test('every chapter rendered, nothing packed', () => {
    const s = libraryState(book({rendered_chapters: 1433, rendered_chunks: 118_831}));
    expect([s.key, s.text, s.tone]).toEqual(['rendered', 'rendered', 'done']);
    expect(s.tip).toMatch(/none packed yet/);
  });

  test('one packed chapter outranks a whole book merely rendered', () => {
    const s = libraryState(book({
      rendered_chapters: 1433, rendered_chunks: 118_831, packed_chapters: 1,
    }));
    expect([s.key, s.text, s.tone]).toEqual(['ready', '1/1433', 'done']);
  });

  test('packed beats rendered even when almost nothing is rendered', () => {
    // The scan can genuinely see this: gc_audio evicts chunk wavs behind a
    // chapter that was already packed, so the m4a outlives its inputs.
    expect(libraryState(book({packed_chapters: 2, rendered_chunks: 40})).key).toBe('ready');
  });

  test('every chapter packed', () => {
    const s = libraryState(book({
      rendered_chapters: 1433, rendered_chunks: 118_831, packed_chapters: 1433,
      packed_bytes: 3_000_000_000,
    }));
    expect([s.key, s.text, s.tone]).toEqual(['complete', 'packed', 'ok']);
    expect(s.tip).toMatch(/all 1433/);
  });

  test('the boundary: one chapter short of complete is still ready', () => {
    expect(libraryState(book({packed_chapters: 1432})).key).toBe('ready');
    expect(libraryState(book({packed_chapters: 1433})).key).toBe('complete');
  });

  test('the boundary: one chapter short of rendered is still partial', () => {
    const nearly = book({rendered_chapters: 1432, rendered_chunks: 118_000});
    expect(libraryState(nearly).key).toBe('partial');
    expect(libraryState({...nearly, rendered_chapters: 1433}).key).toBe('rendered');
  });

  test('an unscanned book reports zero of everything and is not "complete"', () => {
    const fresh = book({chapters: 0, total_chunks: 0, est_min: null});
    expect(libraryState(fresh).key).toBe('none');
  });

  test('every phase carries a sentence', () => {
    const all = [
      book(),
      book({rendered_chunks: 5}),
      book({rendered_chapters: 1433}),
      book({packed_chapters: 4}),
      book({packed_chapters: 1433}),
    ].map(libraryState);
    expect(new Set(all.map((s) => s.key)).size).toBe(5);
    for (const s of all) expect(s.tip.length).toBeGreaterThan(20);
  });
});

describe('renderedPct - chunks, not chapters', () => {
  test('the zero-chunk guard is 0 and not NaN', () => {
    expect(renderedPct({rendered_chunks: 0, total_chunks: 0})).toBe(0);
    expect(renderedPct({rendered_chunks: 7, total_chunks: 0})).toBe(0);
  });

  test('nothing rendered is 0', () => {
    expect(renderedPct({rendered_chunks: 0, total_chunks: 100})).toBe(0);
  });

  test('a single chunk never rounds down to 0', () => {
    expect(renderedPct({rendered_chunks: 1, total_chunks: 118_831})).toBe(1);
  });

  test('one chunk short never rounds up to 100', () => {
    expect(renderedPct({rendered_chunks: 118_830, total_chunks: 118_831})).toBe(99);
  });

  test('done is 100, and over-count cannot exceed it', () => {
    expect(renderedPct({rendered_chunks: 118_831, total_chunks: 118_831})).toBe(100);
    expect(renderedPct({rendered_chunks: 120_000, total_chunks: 118_831})).toBe(100);
  });

  test('the ordinary middle rounds', () => {
    expect(renderedPct({rendered_chunks: 33, total_chunks: 100})).toBe(33);
    expect(renderedPct({rendered_chunks: 1, total_chunks: 3})).toBe(33);
  });
});

describe('downloadCost - the bitrate is the box\'s, never this file\'s', () => {
  const half = book({chapters: 10, packed_chapters: 5, packed_bytes: 5_000_000, est_min: 100});

  test('packed bytes are measured and the rest is estimated', () => {
    const c = downloadCost(half, 480_000);
    // half the book left: 100 min * 0.5 * 480000
    expect(c).toEqual({packed: 5_000_000, rest: 24_000_000, total: 29_000_000});
  });

  test('a different bitrate is a different answer', () => {
    // CLAUDE.md requirement 8: the reader used to hard-code 480000, so a box set
    // to 128k reported every unpacked size at exactly half what it would be.
    const a = downloadCost(half, 480_000);
    const b = downloadCost(half, 960_000);
    expect(b.rest).toBe(48_000_000);
    expect(b.rest).not.toBe(a.rest);
    // Only the estimate moves; what is already on disk was measured.
    expect(b.packed).toBe(a.packed);
  });

  test('no rate is "cannot say", not a guess', () => {
    for (const rate of [undefined, null, 0, -1, NaN]) {
      const c = downloadCost(half, rate);
      expect(c.rest).toBeNull();
      expect(c.total).toBe(5_000_000);
    }
  });

  test('a fully packed book has nothing left to estimate, rate or no rate', () => {
    const done = book({chapters: 10, packed_chapters: 10, packed_bytes: 9_000_000});
    expect(downloadCost(done, 480_000)).toEqual({packed: 9_000_000, rest: 0, total: 9_000_000});
    expect(downloadCost(done).rest).toBe(0);
  });

  test('nothing packed yet: the estimate is the whole book', () => {
    const c = downloadCost(book({chapters: 10, est_min: 100}), 480_000);
    expect(c).toEqual({packed: 0, rest: 48_000_000, total: 48_000_000});
  });

  test('an unscanned book cannot be prorated', () => {
    expect(downloadCost(book({chapters: 0, est_min: null}), 480_000).rest).toBe(0);
    expect(downloadCost(book({chapters: 10, est_min: null}), 480_000).rest).toBeNull();
  });
});

describe('positionLine', () => {
  test('no position, no line', () => {
    expect(positionLine(book())).toBeNull();
  });

  test('the zero-based chapter is printed one higher', () => {
    expect(positionLine(book({position: at(411)}))).toBe('ch 412 / 1433');
  });

  test('the book that is there now is the denominator', () => {
    // The record was written against a 1433-chapter chunking; this book is 20.
    expect(positionLine(book({chapters: 20, position: at(4)}))).toBe('ch 5 / 20');
  });

  test('a book with no chapter count still says where', () => {
    expect(positionLine(book({chapters: 0, position: {...at(6), chapters_total: 0}})))
      .toBe('ch 7');
  });
});

describe('scan freshness', () => {
  const now = 1_700_000_000_000;

  test('never scanned is stale and says so', () => {
    expect(scanStale(null, now)).toBe(true);
    expect(scanAge(null, now)).toBe('never scanned');
    expect(scanAge(undefined, now)).toBe('never scanned');
  });

  test('a scan inside two ticks of the scanner is not worth saying', () => {
    expect(scanStale(now - 60_000, now)).toBe(false);
    expect(scanStale(now - SCAN_STALE_MS, now)).toBe(false);
    expect(scanStale(now - SCAN_STALE_MS - 1, now)).toBe(true);
  });

  test('the thresholds', () => {
    expect(scanAge(now, now)).toBe('just now');
    expect(scanAge(now - 89_000, now)).toBe('just now');
    expect(scanAge(now - 90_000, now)).toBe('1m ago');
    expect(scanAge(now - 4 * 60_000, now)).toBe('4m ago');
    expect(scanAge(now - 59 * 60_000, now)).toBe('59m ago');
    expect(scanAge(now - 60 * 60_000, now)).toBe('an hour ago');
    expect(scanAge(now - 119 * 60_000, now)).toBe('an hour ago');
    expect(scanAge(now - 2 * 60 * 60_000, now)).toBe('2h ago');
    expect(scanAge(now - 23 * 60 * 60_000, now)).toBe('23h ago');
    expect(scanAge(now - 25 * 60 * 60_000, now)).toBe('1d ago');
    expect(scanAge(now - 9 * 24 * 60 * 60_000, now)).toBe('9d ago');
  });

  test('a stamp from the future is clock skew, not a negative age', () => {
    expect(scanAge(now + 30_000, now)).toBe('just now');
    expect(scanStale(now + 30_000, now)).toBe(false);
  });
});

describe('indexBooks', () => {
  test('keyed by the cache key, which is what ?book= takes', () => {
    const r: LibraryResult = {
      books: [book({key: 'a'}), book({key: 'b'})], scanned_ms: 1,
    };
    expect([...indexBooks(r).keys()]).toEqual(['a', 'b']);
  });

  test('no answer is an empty map, never a throw', () => {
    expect(indexBooks(undefined).size).toBe(0);
  });
});

describe('orderBooks - most recently opened first', () => {
  const rows = [{k: 'never'}, {k: 'old'}, {k: 'new'}, {k: 'unknown'}];
  const lib = indexBooks({
    scanned_ms: 1,
    books: [
      book({key: 'never', last_open_ms: null}),
      book({key: 'old', last_open_ms: 1_000}),
      book({key: 'new', last_open_ms: 9_000}),
    ],
  });
  const order = (r: readonly {k: string}[] = rows) =>
    orderBooks(r, (x) => x.k, lib).map((x) => x.k);

  test('newest first, nulls last', () => {
    expect(order()).toEqual(['new', 'old', 'never', 'unknown']);
  });

  test('a book the answer says nothing about keeps its place among the others', () => {
    expect(order([{k: 'unknown'}, {k: 'never'}])).toEqual(['unknown', 'never']);
  });

  test('offline - no answer at all - leaves the list exactly as it came', () => {
    const empty = new Map<string, LibraryBook>();
    expect(orderBooks(rows, (x) => x.k, empty).map((x) => x.k))
      .toEqual(['never', 'old', 'new', 'unknown']);
  });

  test('the input is not mutated', () => {
    const input = [...rows];
    orderBooks(input, (x) => x.k, lib);
    expect(input.map((x) => x.k)).toEqual(['never', 'old', 'new', 'unknown']);
  });
});
