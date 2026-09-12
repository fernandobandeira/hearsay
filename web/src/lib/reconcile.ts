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
 * This module is that step, run on every way back into the app: visibility,
 * focus, the network returning, and a `hello` off the live stream (which fires
 * on every reconnect and therefore *is* the "we were away" signal for a tab that
 * never went hidden). It asks three questions and intersects the answers.
 *
 *     what was asked for   the pending selection, in IndexedDB beside the outbox
 *                          so it survives the app being killed
 *     what is ready        the server's rows: `m4a` true means packed
 *     what is here         Cache Storage, asked rather than remembered - a quota
 *                          eviction has to read as missing
 *
 * Everything else about a download is unchanged, deliberately. Polling stays as
 * the fallback for a session with no live stream, and the ladder still runs in
 * the drawer while the app is open. This is the catch-up pass, not a replacement.
 */
import type {ChapRow} from './types';

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
  /** Called after anything actually landed, so the UI can re-read its state. */
  onStored?: (key: string, ci: number) => void;
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
 */
export async function reconcile(deps: SweepDeps): Promise<SweepBook[]> {
  const out: SweepBook[] = [];
  for (const p of await deps.pending()) {
    const stored = await deps.stored(p.key).catch(() => new Set<number>());
    const rows = await deps.rows(p.key).catch(() => null);
    const fetched: number[] = [];
    const failed: number[] = [];

    for (const ci of rows ? toFetch(p, rows, stored) : []) {
      try {
        await deps.fetchChapter(p.key, ci);
        fetched.push(ci);
        deps.onStored?.(p.key, ci);
      } catch {
        failed.push(ci);
      }
    }

    // Ask the device again rather than trusting the loop above.
    const after = fetched.length
      ? await deps.stored(p.key).catch(() => new Set([...stored, ...fetched]))
      : stored;
    const left = remaining(p, after);
    if (left.length) await deps.save({...p, chapters: left});
    else await deps.drop(p.key);
    out.push({key: p.key, fetched, failed, left});
  }
  return out;
}
