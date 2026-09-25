import {describe, expect, test} from 'vitest';
import {
  mergePending, orderSignature, reconcile, remaining, shouldOrder, toFetch, toOrder,
  ORDER_EVERY_MS, type PendingDownload, type SweepDeps,
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
  /** collect the standing orders this device placed */
  orders?: {key: string; render: number[]; build: number[]}[];
  /** how long ago this device last ordered, per book */
  since?: () => number;
  parallel?: number;
  /** resolve each chapter's copy by hand, to watch how many run at once */
  gate?: (ci: number) => Promise<void>;
}) {
  const stored = opts.stored ?? new Set<number>();
  const asked: number[] = [];
  const sigs = new Map<string, string>();
  const deps: SweepDeps = {
    parallel: opts.parallel,
    ...(opts.orders ? {
      order: async (key, o) => { opts.orders?.push({key, ...o}); },
      lastOrder: (key) => ({sig: sigs.get(key) ?? null, sinceMs: opts.since?.() ?? 0}),
      onOrdered: (key, sig) => sigs.set(key, sig),
    } : {}),
    pending: async () => [...opts.store.values()],
    rows: async (key) => {
      if (!opts.rows) throw new Error(`no network (${key})`);
      return opts.rows;
    },
    stored: async () => new Set(stored),
    fetchChapter: async (_key, ci) => {
      asked.push(ci);
      await opts.gate?.(ci);
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

// ------------------------------------------- the record moving under a pass

describe('a selection that changes while a sweep is running', () => {
  /** Run a sweep whose copy of chapter 1 is held open while `during` happens. */
  async function midPass(
    store: Map<string, PendingDownload>, rows: readonly ChapRow[], during: () => void,
  ) {
    let release = () => {};
    const d = device({
      store, rows,
      gate: (ci) => (ci === 1 ? new Promise<void>((r) => { release = r; }) : Promise.resolve()),
    });
    const run = reconcile(d.deps);
    await new Promise((r) => setTimeout(r, 0));
    during();
    release();
    return run;
  }

  test('chapters added during the pass are still pending after it', async () => {
    const store = new Map([['lom', pending([1, 2])]]);
    const [out] = await midPass(store, [packed(1), row(2), row(7)], () => {
      store.set('lom', pending([1, 2, 7]));        // the drawer, used again
    });
    expect(out.fetched).toEqual([1]);
    expect(store.get('lom')?.chapters).toEqual([2, 7]);
  });

  test('chapters taken out during the pass are not put back', async () => {
    const store = new Map([['lom', pending([1, 2, 3])]]);
    await midPass(store, [packed(1), row(2), row(3)], () => {
      store.set('lom', pending([1, 3]));           // 2 unqueued
    });
    expect(store.get('lom')?.chapters).toEqual([3]);
  });

  test('an order cancelled during the pass stays cancelled', async () => {
    const store = new Map([['lom', pending([1, 2])]]);
    const [out] = await midPass(store, [packed(1), row(2)], () => { store.delete('lom'); });
    expect(out.fetched).toEqual([1]);
    expect(store.has('lom')).toBe(false);
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

// ------------------------------------------------- the server's half of it

describe('toOrder - what the server still has to be told', () => {
  test('a chapter with no audio at all wants rendering, with a pack behind it', () => {
    const rows = [row(1, {rendered: 0, n: 40})];
    expect(toOrder(pending([1]), rows, new Set())).toEqual({render: [1], build: []});
  });

  test('a fully rendered chapter wants packing, and would never have been ordered', () => {
    // This is the gap that made "download" silently do nothing for a chapter
    // the server had already rendered: `pack: true` rides on a *render* call,
    // and this chapter needs no render, so nothing ever asked for the pack.
    expect(toOrder(pending([2]), [row(2, {rendered: 40, n: 40})], new Set()))
      .toEqual({render: [], build: [2]});
  });

  test('work already under way is not re-ordered', () => {
    const rows = [row(1, {queued: true, rendered: 0}), row(2, {pack_queued: true}),
                  row(3, {packing: true}), packed(4)];
    expect(toOrder(pending([1, 2, 3, 4]), rows, new Set()))
      .toEqual({render: [], build: []});
  });

  test('a chapter the rows do not mention has never been asked for', () => {
    expect(toOrder(pending([9]), [packed(1)], new Set())).toEqual({render: [9], build: []});
  });

  test('what is already on the device is nobody\'s order', () => {
    expect(toOrder(pending([1, 2]), [row(1, {rendered: 0}), row(2, {rendered: 0})],
                   new Set([1, 2]))).toEqual({render: [], build: []});
  });
});

describe('shouldOrder - saying it again, and not more often than that', () => {
  const sig = (o: {render: number[]; build: number[]}) => orderSignature(o);

  test('a changed ask goes up immediately', () => {
    expect(shouldOrder({want: 'r1|b', last: 'r1,2|b', sinceMs: 0})).toBe(true);
  });

  test('the same ask waits out the interval - the order is on disk at both ends', () => {
    expect(shouldOrder({want: 'r1|b', last: 'r1|b', sinceMs: 1_000})).toBe(false);
    expect(shouldOrder({want: 'r1|b', last: 'r1|b', sinceMs: ORDER_EVERY_MS})).toBe(true);
  });

  test('nothing to ask for is never an ask', () => {
    expect(sig({render: [], build: []})).toBe('');
    expect(shouldOrder({want: '', last: null, sinceMs: Infinity})).toBe(false);
  });

  test('render and pack are distinguishable, so one becoming the other re-asks', () => {
    expect(sig({render: [1], build: []})).not.toBe(sig({render: [], build: [1]}));
  });
});

describe('the sweep places the order as well as storing the file', () => {
  test('a pending chapter nobody told the server about is ordered on this pass', async () => {
    const orders: {key: string; render: number[]; build: number[]}[] = [];
    const store = new Map([['lom', pending([1, 2])]]);
    // 1 has no audio, 2 is rendered and waiting for a packer nobody called.
    const d = device({store, orders, rows: [row(1, {rendered: 0}), row(2, {rendered: 40})]});

    await reconcile(d.deps);
    expect(orders).toEqual([{key: 'lom', render: [1], build: [2]}]);
    expect(store.get('lom')?.chapters).toEqual([1, 2]);   // still pending, correctly
  });

  test('the same order is not repeated on the next pass', async () => {
    const orders: {key: string; render: number[]; build: number[]}[] = [];
    const store = new Map([['lom', pending([1])]]);
    const d = device({store, orders, rows: [row(1, {rendered: 0})], since: () => 1_000});

    await reconcile(d.deps);
    await reconcile(d.deps);
    expect(orders).toHaveLength(1);
  });

  test('an order that could not be placed is retried, and does not stop the storing', async () => {
    const store = new Map([['lom', pending([1, 2])]]);
    const d = device({store, rows: [row(1, {rendered: 0}), packed(2)]});
    d.deps.order = async () => { throw new Error('409: another book'); };
    d.deps.lastOrder = () => ({sig: null, sinceMs: 0});

    const [out] = await reconcile(d.deps);
    expect(out.ordered).toBeUndefined();     // nothing to remember: it did not land
    expect(out.fetched).toEqual([2]);        // the packed one still came down
  });

  test('offline, no order is attempted at all', async () => {
    const orders: {key: string; render: number[]; build: number[]}[] = [];
    const store = new Map([['lom', pending([1])]]);
    await reconcile(device({store, orders, rows: null}).deps);
    expect(orders).toEqual([]);
  });
});

// ----------------------------------------------------------- three at a time

describe('chapters are copied in parallel', () => {
  /** Let every pending microtask and timer settle, so the pool has actually run. */
  const tick = () => new Promise((r) => setTimeout(r, 0));

  /** Hold every copy open until `release` is called, and count the overlap. */
  function gated() {
    const open = new Set<number>();
    let peak = 0;
    const waiting: (() => void)[] = [];
    return {
      peak: () => peak,
      open,
      gate: (ci: number) => new Promise<void>((resolve) => {
        open.add(ci);
        peak = Math.max(peak, open.size);
        waiting.push(() => { open.delete(ci); resolve(); });
      }),
      release: () => { for (const f of waiting.splice(0)) f(); },
    };
  }

  test('three run at once, and the fourth waits for a free worker', async () => {
    const g = gated();
    const store = new Map([['lom', pending([1, 2, 3, 4, 5])]]);
    const rows = [1, 2, 3, 4, 5].map(packed);
    const d = device({store, rows, gate: g.gate, parallel: 3});

    const run = reconcile(d.deps);
    await tick();
    expect(g.open.size).toBe(3);        // not one, and not all five
    g.release();
    await tick();
    expect(g.open.size).toBe(2);        // the two that were waiting for a worker
    g.release();
    await run;

    expect(g.peak()).toBe(3);
    expect(d.stored).toEqual(new Set([1, 2, 3, 4, 5]));
  });

  test('the answer is still in the book\'s order, however they finished', async () => {
    const store = new Map([['lom', pending([3, 1, 2])]]);
    const d = device({store, rows: [packed(1), packed(2), packed(3)],
                      fails: new Set([2]), parallel: 3});
    const [out] = await reconcile(d.deps);
    expect(out.fetched).toEqual([1, 3]);
    expect(out.failed).toEqual([2]);
  });

  test('one at a time is still available, and is what a single chapter gets', async () => {
    const g = gated();
    const store = new Map([['lom', pending([1, 2])]]);
    const d = device({store, rows: [packed(1), packed(2)], gate: g.gate, parallel: 1});
    const run = reconcile(d.deps);
    await tick();
    expect(g.open.size).toBe(1);
    g.release(); await tick(); g.release();
    await run;
    expect(g.peak()).toBe(1);
  });
});
