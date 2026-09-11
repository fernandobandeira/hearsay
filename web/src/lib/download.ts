/**
 * Download, as one user action.
 *
 * "Render" is gone from the UI. It was the server's word for the first half of
 * a two-stage pipeline - synthesize every chunk, then pack the chunks into one
 * m4a - and it was on a button because the pipeline was on the screen. Nobody
 * wants a rendered chapter they cannot listen to offline: rendering without
 * downloading is a step, not a goal. So download owns the whole pipeline, and
 * this module is the ladder it climbs, per chapter:
 *
 *     queue-render -> await-render -> request-pack -> await-pack -> store
 *
 * Each rung is decided from the row the server just reported, never from a
 * counter this client keeps - the same disk-truth rule the server's own render
 * worker follows. That matters because every one of these waits is minutes
 * long: the drawer is polled, the app is backgrounded, the row comes back
 * further along than we left it, and a remembered step would be a lie. Ask the
 * row, act once, ask again.
 *
 * Pure, so the ladder can be tested rather than watched.
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

/** True while a phase is the client's turn to act rather than to wait. */
export function isAction(phase: DownloadPhase): boolean {
  return phase === 'queue-render' || phase === 'request-pack' || phase === 'store';
}

/**
 * The chapters that need the server to render before anything can be packed.
 *
 * Sent in one call, up front, for the whole selection: the server works a queue
 * one chapter at a time, so telling it about all of them immediately means it
 * is never idle while this client is busy storing an earlier one.
 */
export function needsRender(
  rows: readonly ChapRow[], offline: ReadonlySet<number>,
): number[] {
  return rows
    .filter((r) => phaseFor(r, offline.has(r.i)) === 'queue-render')
    .map((r) => r.i);
}

/** 64 kbit/s is the server's CHAPTER_BITRATE, so a minute is ~480 kB. */
const BYTES_PER_MIN = 64_000 / 8 * 60;

/**
 * How big the selection will be, in bytes.
 *
 * A packed chapter reports its real size. An unrendered one has never existed
 * as a file, so the estimate comes off its minutes at the server's own bitrate
 * - which is why the confirm bar says "~".
 */
export function estimateBytes(rows: readonly ChapRow[]): number {
  let n = 0;
  for (const r of rows) {
    if (r.bytes != null) n += r.bytes;
    else if (r.est_min != null) n += Math.round(r.est_min * BYTES_PER_MIN);
  }
  return n;
}

/** Did the selection include anything the server still has to make? */
export function anyEstimated(rows: readonly ChapRow[]): boolean {
  return rows.some((r) => r.bytes == null);
}
