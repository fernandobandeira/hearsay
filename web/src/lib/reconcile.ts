/**
 * The foreground reconciliation sweep.
 *
 * **The platform fact this is built around.** iOS suspends a PWA within seconds
 * of the screen going off: no timers, no fetches, no service worker, and
 * `EventSource` comes back as a fresh connection rather than resuming. That is
 * not a bug to work around, it is the deal, and the download ladder in
 * lib/download.ts is a *foreground* process - it climbs a rung, waits, polls,
 * climbs the next. Backgrounded, it stops mid-ladder.
 *
 * The server's half no longer stops with it: `pack: true` on the render call is
 * a download intent the server writes down and finishes on its own, restart
 * included (see `src/wishlist.rs`). So by the time the app is opened again the
 * chapters are usually sitting there packed, and the only thing left undone is
 * the one step that can only happen on the device - copying the m4a into Cache
 * Storage.
 *
 * This module is that step - and, since the round that follows, the *whole*
 * device-side half of a download rather than a catch-up pass beside it. It runs
 * on every way back into the app (visibility, focus, the network returning, a
 * `hello` off the live stream) and on a short timer while the app is open, and
 * every pass asks the same four questions and acts on the answers.
 *
 *     what was asked for   the pending selection, in IndexedDB beside the outbox
 *                          so it survives the app being killed
 *     what the server owes the standing order, re-derived from the rows: which
 *                          chapters still need rendering, which need packing
 *     what is ready        the server's rows: `m4a` true means packed
 *     what is here         Cache Storage, asked rather than remembered - a quota
 *                          eviction has to read as missing
 *
 * **Why it took over the drawer's ladder.** The drawer used to climb a per-chapter
 * ladder of its own - queue the render, poll, ask for the pack, poll, store - in
 * component state, one chapter at a time, with a single `err` string for the
 * whole run. Three of the four things he reported came out of that: the ladder
 * dies with the component and with the app, so a restart left a selection with
 * nobody driving it; a chapter that failed mid-run set `err` and was dropped,
 * and the next chapter's failure overwrote the message, so a chapter looked
 * *skipped*; and nothing outside the run ever asked the server for a pack, so a
 * chapter that finished rendering an hour later sat there packed and unstored
 * until the app happened to be re-opened.
 *
 * There is one queue now, it is on disk, and this is the only thing that drives
 * it. The drawer writes the selection down and gets out of the way.
 */
import {phaseFor} from './download';
import type {ChapRow} from './types';

/**
 * How many chapters are copied onto the device at once.
 *
 * One at a time was leaving the wifi mostly idle: each chapter is a single ~6 MB
 * GET, and the box - two ARM cores, busy synthesizing - answers one of those far
 * below the link's capacity, so the transfer spends most of its life waiting
 * rather than moving. Three overlap without turning the phone's radio or the
 * server's disk into the new bottleneck, and they are three *separate* files, so
 * nothing has to be packed together to get the parallelism.
 *
 * Deliberately not larger. The A1 is also rendering while this runs, every
 * connection is a byte-range-capable read off the same disk, and a chapter that
 * is stored is only useful once it is *whole* - twelve half-finished downloads
 * are worth less than three finished ones when the screen goes off mid-sweep.
 */
export const PARALLEL = 3;

/**
 * How often the standing order is repeated to the server, at most.
 *
 * The order is durable on both sides - `queue.json` there, `downloads` here - so
 * repeating it is housekeeping rather than the mechanism, and its one job is to
 * survive a server that lost its queue. A sweep runs every few seconds while the
 * app is open, and re-posting a 74-chapter order at that rate would be an
 * fsync-per-tick on the box for nothing.
 */
export const ORDER_EVERY_MS = 120_000;

/**
 * How often a sweep runs while the app is open and visible.
 *
 * The event that should drive it is the live stream's `render` with `kind:
 * "packed"`, and it does - this is the floor under it, for a session whose
 * stream never came up (an old browser, a proxy that will not pass
 * `text/event-stream`) and for the gap between a chapter finishing and anybody
 * noticing. A pass with nothing to do is one IndexedDB read and one
 * `/api/chapters`, which the drawer already polls harder than this while it
 * is open.
 */
export const SWEEP_EVERY_MS = 20_000;

/** A download selection this device has not finished storing. */
export interface PendingDownload {
  /** The book's cache key: how chapters are addressed in every cache and URL. */
  key: string;
  /** The epub path, so a sweep can name the book without the library loaded. */
  path?: string;
  /** Chapters asked for and not yet on this device. */
  chapters: number[];
  /** When the selection was confirmed. */
  ts: number;
}

const sorted = (xs: Iterable<number>): number[] => [...new Set(xs)].sort((a, b) => a - b);

/**
 * What to fetch right now: **asked for, packed on the server, not here yet**.
 *
 * The intersection is the whole rule, and each term comes from the only place
 * that knows it. A chapter that is in the selection but not packed is the
 * server's turn - it has the intent and will get there. A chapter that is packed
 * but not in the selection is somebody else's download. A chapter already in
 * Cache Storage is done, whatever any bookkeeping says.
 */
export function toFetch(
  pending: Pick<PendingDownload, 'chapters'>,
  rows: readonly ChapRow[],
  stored: ReadonlySet<number>,
): number[] {
  const packed = new Set(rows.filter((r) => r.m4a).map((r) => r.i));
  return sorted(pending.chapters.filter((ci) => packed.has(ci) && !stored.has(ci)));
}

/**
 * What the *server* still has to be asked for, out of a pending selection.
 *
 * The other half of the durable queue, and the half that was missing. `pack:
 * true` makes a render call a standing order the server finishes on its own, so
 * a selection placed last night is normally packed by morning - but only for the
 * chapters that call covered. Two kinds slip out of it:
 *
 *   never ordered    the app was killed between writing the selection down and
 *                    posting it, or the post was lost. The record is on the
 *                    device and nothing on the server knows about it.
 *   already rendered a chapter whose chunks all had audio at the time needed no
 *                    render, so it was never in a `pack: true` call - it needs
 *                    the packer asked directly.
 *
 * Recomputing both from the rows on every sweep means the order is re-derived
 * from disk truth on each pass rather than remembered, which is the same rule
 * the server's own render worker follows. Both endpoints are idempotent.
 */
export function toOrder(
  pending: Pick<PendingDownload, 'chapters'>,
  rows: readonly ChapRow[],
  stored: ReadonlySet<number>,
): {render: number[]; build: number[]} {
  const byIndex = new Map(rows.map((r) => [r.i, r]));
  const render: number[] = [];
  const build: number[] = [];
  for (const ci of sorted(pending.chapters)) {
    if (stored.has(ci)) continue;
    const row = byIndex.get(ci);
    // No row at all is not "wait and see": the rows are the whole book, so a
    // chapter missing from them is a chapter the server has never been told
    // about. Ask for it.
    if (!row) { render.push(ci); continue; }
    const phase = phaseFor(row, false);
    if (phase === 'queue-render') render.push(ci);
    else if (phase === 'request-pack') build.push(ci);
  }
  return {render, build};
}

/**
 * Is it worth repeating the standing order?
 *
 * Yes when it has changed - a chapter finished rendering and now wants packing,
 * or a new selection arrived - because that is a different ask. Yes when it has
 * been `everyMs` since the last one, which is the case this exists for: a server
 * that restarted and came back without the queue. Otherwise no: the order is on
 * disk at both ends and saying it again changes nothing.
 */
export function shouldOrder(
  {want, last, sinceMs}: {want: string; last: string | null; sinceMs: number},
  everyMs = ORDER_EVERY_MS,
): boolean {
  return !want ? false : want !== last || sinceMs >= everyMs;
}

/** The order as one comparable string, for `shouldOrder`. */
export const orderSignature = (o: {render: number[]; build: number[]}): string =>
  o.render.length || o.build.length ? `r${o.render.join(',')}|b${o.build.join(',')}` : '';

/**
 * What is left of the order after a sweep: everything not on this device.
 *
 * Computed from Cache Storage rather than from what the sweep thinks it stored,
 * for the same reason the server's render worker reads the filesystem instead of
 * a counter - a quota eviction mid-sweep is exactly the case where the two
 * answers differ, and the one that matters is the disk.
 */
export function remaining(
  pending: Pick<PendingDownload, 'chapters'>,
  stored: ReadonlySet<number>,
): number[] {
  return sorted(pending.chapters.filter((ci) => !stored.has(ci)));
}

/**
 * Fold a freshly confirmed selection into whatever is already pending.
 *
 * Union rather than replace, because two selections can overlap in time: the
 * drawer is used twice in one session, or a sweep is still settling last night's
 * order when a new one is confirmed. Replacing would leave chapters the server is
 * still packing with nobody waiting to store them - the exact leak this module
 * exists to plug, reintroduced one level up.
 *
 * The newer record's `ts` and `path` win; the chapters are the union.
 */
export function mergePending(
  had: PendingDownload | undefined, add: PendingDownload,
): PendingDownload {
  return {...had, ...add, chapters: sorted([...(had?.chapters ?? []), ...add.chapters])};
}

/** What one book's sweep did. */
export interface SweepBook {
  key: string;
  /** Stored on this device by this sweep. */
  fetched: number[];
  /** Tried and did not land: the network went away again, or the server did. */
  failed: number[];
  /** Still wanted, so the next sweep picks them up. */
  left: number[];
  /** The standing order this pass placed, if it placed one. */
  ordered?: string;
}

export interface SweepDeps {
  /** Every unfinished selection this device holds. */
  pending: () => Promise<PendingDownload[]>;
  /** The server's chapter rows for one book. May reject: offline is normal here. */
  rows: (key: string) => Promise<readonly ChapRow[]>;
  /** What Cache Storage actually holds for one book. */
  stored: (key: string) => Promise<Set<number>>;
  /** Copy one chapter onto this device. */
  fetchChapter: (key: string, ci: number) => Promise<unknown>;
  /** Persist a selection that is not finished. */
  save: (p: PendingDownload) => Promise<void>;
  /** Forget a selection that is. */
  drop: (key: string) => Promise<void>;
  /**
   * Repeat the standing order to the server: render these (with `pack: true`),
   * pack those. Called only when `shouldOrder` says the ask has changed or has
   * gone stale, and its failure is not the sweep's - the storing half still runs.
   */
  order?: (key: string, o: {render: number[]; build: number[]}) => Promise<unknown>;
  /** Remembered per book, so an unchanged order is not repeated every few seconds. */
  lastOrder?: (key: string) => {sig: string | null; sinceMs: number};
  /** Called after `order` succeeds, with what was ordered. */
  onOrdered?: (key: string, sig: string) => void;
  /** Called when a chapter's copy starts and ends, so the row can say so. */
  onFetching?: (key: string, ci: number, active: boolean) => void;
  /** Called after anything actually landed, so the UI can re-read its state. */
  onStored?: (key: string, ci: number) => void;
  /** How many chapters to copy at once. */
  parallel?: number;
}

/**
 * Run `n` workers over one list, keeping the results in the list's order.
 *
 * A pool rather than chunked batches: a batch of three waits for its slowest
 * member before starting the next three, which on a link this variable is most
 * of the time. Each worker takes the next index the moment it is free.
 */
async function pool<T>(
  items: readonly T[], n: number, work: (item: T) => Promise<boolean>,
): Promise<boolean[]> {
  const out = new Array<boolean>(items.length);
  let next = 0;
  const worker = async () => {
    for (;;) {
      const i = next++;
      if (i >= items.length) return;
      out[i] = await work(items[i]);
    }
  };
  await Promise.all(Array.from({length: Math.max(1, Math.min(n, items.length))}, worker));
  return out;
}

/**
 * Run the sweep. Returns one entry per pending book, in the order stored.
 *
 * Failures are collected rather than thrown - one chapter that will not come
 * down must not cancel the twelve behind it, which is the same lesson the
 * whole-book text download learned the hard way (lib/offline.ts). A book whose
 * rows cannot be fetched at all is not a failure either: the sweep still settles
 * whatever is already in Cache Storage, so a download finished by an earlier
 * sweep stops being pending even with no network.
 *
 * Books are swept one after another and chapters within a book in parallel. That
 * asymmetry is deliberate: chapters of the book being read are what somebody is
 * waiting for, and spreading the link across two books would make both later.
 */
export async function reconcile(deps: SweepDeps): Promise<SweepBook[]> {
  const out: SweepBook[] = [];
  for (const p of await deps.pending()) {
    const stored = await deps.stored(p.key).catch(() => new Set<number>());
    const rows = await deps.rows(p.key).catch(() => null);

    /* The server's half first, so a chapter that needs rendering is ordered on
       this pass rather than on the next one - the order is what runs while the
       phone is asleep, and it costs one POST. Its failure is not the sweep's:
       whatever is already packed still gets stored below. */
    let ordered: string | undefined;
    if (rows && deps.order) {
      const want = toOrder(p, rows, stored);
      const sig = orderSignature(want);
      const {sig: last, sinceMs} = deps.lastOrder?.(p.key) ?? {sig: null, sinceMs: Infinity};
      if (shouldOrder({want: sig, last, sinceMs})) {
        const ok = await deps.order(p.key, want).then(() => true, () => false);
        if (ok) { ordered = sig; deps.onOrdered?.(p.key, sig); }
      }
    }

    const wanted = rows ? toFetch(p, rows, stored) : [];
    const fetched: number[] = [];
    const failed: number[] = [];
    const results = await pool(wanted, deps.parallel ?? PARALLEL, async (ci) => {
      deps.onFetching?.(p.key, ci, true);
      try {
        await deps.fetchChapter(p.key, ci);
        deps.onStored?.(p.key, ci);
        return true;
      } catch {
        return false;
      } finally {
        deps.onFetching?.(p.key, ci, false);
      }
    });
    wanted.forEach((ci, i) => (results[i] ? fetched : failed).push(ci));

    // Ask the device again rather than trusting the loop above.
    const after = fetched.length
      ? await deps.stored(p.key).catch(() => new Set([...stored, ...fetched]))
      : stored;
    /* Written back against the record as it stands *now*, not as it stood when
       this pass read it. A pass over a big order is minutes of downloading, and
       the drawer is usable the whole time: a selection added meanwhile would be
       lost by writing back the old list, and one taken out would be put back.
       So the only edit a sweep makes is its own - striking what is stored - and
       a record that has gone (the whole order cancelled) stays gone. A record
       that cannot be re-read is the one this pass started from: better a stale
       write than none, which would leave stored chapters pending forever. */
    const now = await deps.pending()
      .then((all) => all.find((x) => x.key === p.key) ?? null, () => p);
    const left = now ? remaining(now, after) : [];
    if (now && left.length) {
      if (left.length !== now.chapters.length) await deps.save({...now, chapters: left});
    } else if (now) await deps.drop(p.key);
    out.push({key: p.key, fetched, failed, left, ordered});
  }
  return out;
}
