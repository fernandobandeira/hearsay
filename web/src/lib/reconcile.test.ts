import {describe, expect, test} from 'vitest';
import {
  mergePending, reconcile, remaining, toFetch,
  type PendingDownload, type SweepDeps,
} from './reconcile';
import type {ChapRow} from './types';

const row = (i: number, p: Partial<ChapRow> = {}): ChapRow => ({
  i, title: `${i + 1}: a chapter`, n: 40, est_min: 9.1, est_bytes: 4_368_000,
  rendered: 40, m4a: false, bytes: null, duration: null,
  queued: false, packing: false, pack_queued: false, ...p,
});
const packed = (i: number) => row(i, {m4a: true, bytes: 4_300_000});
const pending = (chapters: number[]): PendingDownload =>
  ({key: 'lom', path: '/books/lom.epub', chapters, ts: 1_700_000_000_000});

// ---------------------------------------------------------- the intersection

describe('toFetch - asked for, packed, not here', () => {
  test('only the chapters where all three are true', () => {
    const rows = [packed(1), packed(2), row(3), packed(9)];
    expect(toFetch(pending([1, 2, 3]), rows, new Set([2]))).toEqual([1]);
  });

  test('a chapter the server has not packed is the server\'s turn, not ours', () => {
    // it has the intent and will get there; asking for the m4a now is a 404
    expect(toFetch(pending([3]), [row(3, {rendered: 40})], new Set())).toEqual([]);
    expect(toFetch(pending([3]), [row(3, {pack_queued: true})], new Set())).toEqual([]);
  });

  test('a packed chapter nobody selected belongs to somebody else\'s download', () => {
    expect(toFetch(pending([1]), [packed(1), packed(7)], new Set())).toEqual([1]);
  });

  test('already in Cache Storage is done, whatever the bookkeeping says', () => {
    expect(toFetch(pending([1, 2]), [packed(1), packed(2)], new Set([1, 2]))).toEqual([]);
  });

  test('a row the server did not mention at all is not fetched', () => {
    // a windowed /api/chapters answer, or a book that has since been swapped
    expect(toFetch(pending([1, 2]), [packed(2)], new Set())).toEqual([2]);
  });

  test('sorted and de-duplicated, so the order of a sweep is the order of the book', () => {
    const rows = [packed(9), packed(2), packed(5)];
    expect(toFetch(pending([9, 2, 5, 2]), rows, new Set())).toEqual([2, 5, 9]);
  });
});

describe('remaining - what is still wanted', () => {
  test('is the device\'s answer, not the sweep\'s', () => {
    expect(remaining(pending([1, 2, 3]), new Set([2]))).toEqual([1, 3]);
  });

  test('empty when everything landed', () => {
    expect(remaining(pending([1, 2]), new Set([1, 2, 5]))).toEqual([]);
  });
});

describe('mergePending - two selections in one session', () => {
  test('the union, not the newer one', () => {
    const had = pending([1, 2]);
    const add = {...pending([2, 7]), ts: 1_700_000_009_000};
    expect(mergePending(had, add).chapters).toEqual([1, 2, 7]);
  });

  test('the newer record\'s stamp and path win', () => {
    const merged = mergePending(pending([1]), {...pending([2]), ts: 5, path: '/books/x.epub'});
    expect(merged.ts).toBe(5);
    expect(merged.path).toBe('/books/x.epub');
  });

  test('nothing pending yet is just the new selection', () => {
    expect(mergePending(undefined, pending([3, 1])).chapters).toEqual([1, 3]);
  });
});

// ------------------------------------------------------------- the sweep

/**
 * A device: what it has stored, what the server says, and the pending record.
 *
 * The record lives in `store`, which is passed by reference - so building a
 * second `deps` over the same `store` is a cold launch of the app with the same
 * IndexedDB behind it, which is exactly what the disconnect test needs.
 */
function device(opts: {
  store: Map<string, PendingDownload>;
  rows?: readonly ChapRow[] | null;
  stored?: Set<number>;
  fails?: ReadonlySet<number>;
}) {
  const stored = opts.stored ?? new Set<number>();
  const asked: number[] = [];
  const deps: SweepDeps = {
    pending: async () => [...opts.store.values()],
    rows: async (key) => {
      if (!opts.rows) throw new Error(`no network (${key})`);
      return opts.rows;
    },
    stored: async () => new Set(stored),
    fetchChapter: async (_key, ci) => {
      asked.push(ci);
      if (opts.fails?.has(ci)) throw new Error('the tunnel went away');
      stored.add(ci);
    },
    save: async (p) => { opts.store.set(p.key, p); },
    drop: async (key) => { opts.store.delete(key); },
  };
  return {deps, stored, asked};
}

describe('reconcile - the foreground sweep', () => {
  test('stores what is packed and forgets the order when it is done', async () => {
    const store = new Map([['lom', pending([1, 2])]]);
    const d = device({store, rows: [packed(1), packed(2)]});

    expect(await reconcile(d.deps)).toEqual([
      {key: 'lom', fetched: [1, 2], failed: [], left: []},
    ]);
    expect(d.stored).toEqual(new Set([1, 2]));
    expect(store.size).toBe(0);
  });

  test('what the server has not packed yet stays pending', async () => {
    const store = new Map([['lom', pending([1, 2])]]);
    const d = device({store, rows: [packed(1), row(2)]});

    expect(await reconcile(d.deps)).toEqual([
      {key: 'lom', fetched: [1], failed: [], left: [2]},
    ]);
    expect(store.get('lom')?.chapters).toEqual([2]);
    // The same order, trimmed - not a new one with a new stamp.
    expect(store.get('lom')?.ts).toBe(1_700_000_000_000);
  });

  test('one chapter that will not come down does not cancel the rest', async () => {
    const store = new Map([['lom', pending([1, 2, 3])]]);
    const d = device({store, rows: [packed(1), packed(2), packed(3)], fails: new Set([2])});

    expect(await reconcile(d.deps)).toEqual([
      {key: 'lom', fetched: [1, 3], failed: [2], left: [2]},
    ]);
    expect(d.asked).toEqual([1, 2, 3]);   // it kept going past the failure
  });

  test('no network: nothing is fetched and nothing is forgotten', async () => {
    const store = new Map([['lom', pending([1, 2])]]);
    const d = device({store, rows: null});

    expect(await reconcile(d.deps)).toEqual([
      {key: 'lom', fetched: [], failed: [], left: [1, 2]},
    ]);
    expect(d.asked).toEqual([]);
    expect(store.get('lom')?.chapters).toEqual([1, 2]);
  });

  test('no network, but an earlier sweep already stored them: the order is settled', async () => {
    // the sweep still answers the only question it can offline - is it here? -
    // so a finished download stops being pending on a plane
    const store = new Map([['lom', pending([1, 2])]]);
    const d = device({store, rows: null, stored: new Set([1, 2])});

    expect((await reconcile(d.deps))[0].left).toEqual([]);
    expect(store.size).toBe(0);
  });

  test('every pending book is swept, not just the open one', async () => {
    const store = new Map([['lom', pending([1])], ['7p', {...pending([4]), key: '7p'}]]);
    const d = device({store, rows: [packed(1), packed(4)]});

    expect((await reconcile(d.deps)).map((b) => b.key)).toEqual(['lom', '7p']);
    expect(store.size).toBe(0);
  });

  test('nothing pending is a no-op', async () => {
    const d = device({store: new Map(), rows: [packed(1)]});
    expect(await reconcile(d.deps)).toEqual([]);
    expect(d.asked).toEqual([]);
  });
});

// --------------------------------------------------- the case it exists for

describe('a download interrupted mid-flow', () => {
  /**
   * The whole feature, end to end, with the device doing the things a phone
   * actually does.
   *
   * The selection is confirmed, the server takes the order, and the phone is
   * locked before anything is packed. The app is killed. Later it is opened
   * again - a different `deps` over the same store, which is what a cold launch
   * is - and the sweep finishes the download with nobody asking it to.
   */
  test('a later foreground completes it', async () => {
    const store = new Map<string, PendingDownload>();
    store.set('lom', pending([11, 12, 13]));

    // Foreground one: the server is still rendering. Nothing to store yet, and
    // the record is untouched.
    const first = device({store, rows: [row(11, {rendered: 8}), row(12), row(13)]});
    expect((await reconcile(first.deps))[0]).toEqual(
      {key: 'lom', fetched: [], failed: [], left: [11, 12, 13]});

    // The phone goes away mid-download: the network dies with one chapter
    // packed, and the one it tries to fetch does not land.
    const second = device({
      store, rows: [packed(11), row(12), row(13)], fails: new Set([11]),
    });
    expect((await reconcile(second.deps))[0]).toEqual(
      {key: 'lom', fetched: [], failed: [11], left: [11, 12, 13]});

    // The app is killed. This is the part the record exists for: a new process,
    // a new sweep, the same IndexedDB - and by now the server has finished all
    // three, because its half of the order never depended on this device.
    const later = device({store, rows: [packed(11), packed(12), packed(13)]});
    expect((await reconcile(later.deps))[0]).toEqual(
      {key: 'lom', fetched: [11, 12, 13], failed: [], left: []});
    expect(later.stored).toEqual(new Set([11, 12, 13]));
    expect(store.size).toBe(0);           // and the order is finished
  });

  test('a quota eviction during the sweep leaves the chapter pending', async () => {
    // `remaining` is computed from Cache Storage rather than from what the sweep
    // thinks it stored, and this is the case where the two answers differ.
    const store = new Map([['lom', pending([1, 2])]]);
    const stored = new Set<number>();
    let swept = false;
    const deps: SweepDeps = {
      pending: async () => [...store.values()],
      rows: async () => [packed(1), packed(2)],
      stored: async () => {
        // The second ask - the one after the fetches - finds chapter 1 gone.
        if (!swept) { swept = true; return new Set(stored); }
        return new Set([...stored].filter((ci) => ci !== 1));
      },
      fetchChapter: async (_k, ci) => { stored.add(ci); },
      save: async (p) => { store.set(p.key, p); },
      drop: async (key) => { store.delete(key); },
    };

    const [book] = await reconcile(deps);
    expect(book.fetched).toEqual([1, 2]);
    expect(book.left).toEqual([1]);       // evicted, so still wanted
    expect(store.get('lom')?.chapters).toEqual([1]);
  });
});

// ------------------------------------------------------- the record itself

describe('the pending record round-trips', () => {
  /**
   * The persistence contract, against the three operations lib/db.ts provides:
   * merge in a selection, write back what is left, delete when it is finished.
   * (lib/db.ts itself is a thin IndexedDB wrapper around exactly these; the
   * rule worth testing is the one in `mergePending`, and it is shared.)
   */
  test('survives being written, merged, swept and re-read', async () => {
    const store = new Map<string, PendingDownload>();
    const add = (p: PendingDownload) => store.set(p.key, mergePending(store.get(p.key), p));

    add(pending([11, 12]));
    add({...pending([12, 13]), ts: 2});
    expect(store.get('lom')).toEqual(
      {key: 'lom', path: '/books/lom.epub', chapters: [11, 12, 13], ts: 2});

    // A sweep gets one of them, and what is written back is what a cold launch
    // reads: the same book, the same path, the chapters that are left.
    const d = device({store, rows: [packed(11), row(12), row(13)]});
    await reconcile(d.deps);
    expect(store.get('lom')).toEqual(
      {key: 'lom', path: '/books/lom.epub', chapters: [12, 13], ts: 2});
  });
});
