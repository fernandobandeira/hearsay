/**
 * Download, as one user action.
 *
 * "Render" is gone from the UI. It was the server's word for the first half of
 * a two-stage pipeline - synthesize every chunk, then pack the chunks into one
 * m4a - and it was on a button because the pipeline was on the screen. Nobody
 * wants a rendered chapter they cannot listen to offline: rendering without
 * downloading is a step, not a goal. So download means the whole pipeline, and
 * this module says where in it a chapter is:
 *
 *     queue-render -> await-render -> request-pack -> await-pack -> store
 *
 * The phase is decided from the row the server just reported, never from a
 * counter this client keeps - the same disk-truth rule the server's own render
 * worker follows. That matters because every one of these waits is minutes
 * long: the app is backgrounded, the row comes back further along than we left
 * it, and a remembered step would be a lie. Ask the row.
 *
 * **What this module used to also be, and is not any more.** It carried the
 * bookkeeping for a *client-driven* climb - has the ask been acknowledged, has
 * the row stopped moving, is it time to ask again - because the drawer used to
 * walk each chapter up the ladder itself, in component state, one at a time.
 * That loop is gone: the durable queue in lib/reconcile.ts places a standing
 * order the server finishes on its own and re-derives what is still owed from
 * the rows on every pass, so there is nothing to acknowledge and no stall to
 * detect. What survives here is the reading of a row, which the queue and the
 * drawer both need.
 *
 * Pure, so it can be tested rather than watched.
 */
import type {ChapRow} from './types';

export type DownloadPhase =
  /** already on this device: nothing to do */
  | 'stored'
  /** no audio at all - ask the server to render it */
  | 'queue-render'
  /** the server is rendering (or has it queued); wait */
  | 'await-render'
  /** every chunk has audio - ask the server to pack them */
  | 'request-pack'
  /** the server is packing (or has it queued to pack); wait */
  | 'await-pack'
  /** the m4a exists - copy it onto this device */
  | 'store';

/** What this device is doing to a chapter right now, for the row's badge. */
export type DownloadJob = 'queued' | 'rendering' | 'packing' | 'saving';

/**
 * The rung a chapter is on.
 *
 * Order is the pipeline backwards, most-finished first, because that is the
 * only order in which a partly-true row cannot be misread: a chapter that is
 * packed *and* queued (the server queued a re-render behind it) is ready to
 * store, not ready to wait for.
 */
export function phaseFor(r: ChapRow | undefined, offline: boolean): DownloadPhase {
  if (offline) return 'stored';
  if (!r) return 'await-render';
  if (r.m4a) return 'store';
  if (r.packing || r.pack_queued) return 'await-pack';
  if (r.n > 0 && r.rendered >= r.n) return 'request-pack';
  if (r.queued) return 'await-render';
  return 'queue-render';
}

/**
 * What this device is doing about a chapter, for the row's badge.
 *
 * The drawer used to answer this from a map of jobs kept in component state,
 * written by the foreground ladder as it climbed. That map was empty after every
 * restart, so a chapter ordered last night came back looking like a chapter
 * nobody had asked for - and tapping it downloaded it again. It now comes from
 * the two facts that outlive the component: the chapter is in the durable queue,
 * and the sweep is copying it right now.
 *
 * `null` means the device is doing nothing about this chapter and the row should
 * say whatever the server's row says. A queued chapter reports the *server's*
 * stage - rendering, packing - because that is what it is genuinely waiting on;
 * "queued" on its own is only right before the server has started.
 */
export function queueJob(
  r: ChapRow | undefined, queued: boolean, saving: boolean,
): DownloadJob | null {
  if (saving) return 'saving';
  if (!queued) return null;
  return jobFor(phaseFor(r, false));
}

/** The badge for a rung, or null where the row's own state already says it. */
export function jobFor(phase: DownloadPhase): DownloadJob | null {
  switch (phase) {
    case 'queue-render': return 'queued';
    case 'await-render': return 'rendering';
    case 'request-pack':
    case 'await-pack': return 'packing';
    case 'store': return 'saving';
    case 'stored': return null;
  }
}

/**
 * The fallback rate, used only when the server has told us nothing: 64 kbit/s
 * is the default `CHAPTER_BITRATE`, so a minute is ~480 kB.
 *
 * It used to be the *only* rate, hard-coded here, because the server did not
 * report its own - so changing the env var on the box silently made every size
 * in the UI wrong by that ratio. Now each row carries `est_bytes`, computed
 * server-side from the constants the server actually has, and `/api/status`
 * carries the rate itself for anything that has to do its own arithmetic.
 */
export const FALLBACK_BYTES_PER_MIN = 64_000 / 8 * 60;

/**
 * How big the selection will be, in bytes.
 *
 * Three sources, in order of how much they know: the measured size of a packed
 * chapter, the server's own estimate for one that is not packed yet, and - only
 * if a row predates both - minutes times a rate. Which is why the confirm bar
 * says "~" whenever anything in the selection is not yet a file.
 */
export function estimateBytes(
  rows: readonly ChapRow[], bytesPerMin = FALLBACK_BYTES_PER_MIN,
): number {
  const rate = bytesPerMin > 0 ? bytesPerMin : FALLBACK_BYTES_PER_MIN;
  let n = 0;
  for (const r of rows) {
    if (r.bytes != null) n += r.bytes;
    else if (r.est_bytes != null) n += r.est_bytes;
    else if (r.est_min != null) n += Math.round(r.est_min * rate);
  }
  return n;
}

/** Did the selection include anything the server still has to make? */
export function anyEstimated(rows: readonly ChapRow[]): boolean {
  return rows.some((r) => r.bytes == null);
}
