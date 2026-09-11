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
import type {BuildResult, ChapRow, RenderResult} from './types';

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

// ------------------------------------------------------- reading the answers
//
// The server used to answer `/api/chapters/build` with three lists and no way
// to tell "I refused this one" from "nobody mentioned it", so this client
// ignored the response entirely and re-asked every twenty seconds until the row
// moved. It now says what it refused and why, per chapter, which turns the
// re-ask from a policy into an exception - see `shouldReask`.

export type BuildVerdict =
  /** The packer has it (or it was already packed). Stop asking. */
  | {t: 'taken'}
  /** Not rendered yet; the server queued the render instead. Stop asking. */
  | {t: 'rendering'; rendered: number; n: number}
  /** It can never be packed - no such chapter, or no audio in it. Give up. */
  | {t: 'impossible'; reason: string}
  /** The answer does not mention it at all: ask again when it makes sense. */
  | {t: 'unknown'};

export function buildVerdict(r: BuildResult | null | undefined, ci: number): BuildVerdict {
  if (!r) return {t: 'unknown'};
  if (r.built?.includes(ci) || r.building?.includes(ci)) return {t: 'taken'};
  const refusal = r.refused?.find((x) => x.chapter === ci);
  if (refusal?.reason === 'not_rendered')
    return {t: 'rendering', rendered: refusal.rendered, n: refusal.n};
  if (refusal) return {t: 'impossible', reason: refusal.reason};
  if (r.rendering?.includes(ci)) return {t: 'rendering', rendered: 0, n: 0};
  return {t: 'unknown'};
}

/** Did `/api/chapters/render` take this chapter? Its queue is the receipt. */
export function renderAccepted(r: RenderResult | null | undefined, ci: number): boolean {
  return !!r && (r.queue?.includes(ci) || r.packing?.includes(ci));
}

/**
 * Everything about a row that means "the server is getting somewhere". Compared
 * between polls, so a rung that has genuinely stopped moving can be told from
 * one that is simply slow - a chapter of a big novel takes minutes to render,
 * and minutes of silence are not a stall.
 */
export function rowSignature(r: ChapRow | undefined): string {
  if (!r) return 'none';
  return [r.rendered, r.n, r.m4a ? 1 : 0, r.queued ? 1 : 0,
          r.packing ? 1 : 0, r.pack_queued ? 1 : 0].join(':');
}

/** How long a row may sit perfectly still before the ask is repeated. */
export const STALL_MS = 90_000;

/**
 * Should the ask be repeated?
 *
 * Yes if it was never acknowledged - the call may have been lost, and both
 * endpoints are idempotent. Yes if it was acknowledged but nothing has changed
 * for `stallMs`, which is the case this exists for: a server restart between the
 * ask and the work, where the acknowledgement was true when it was given and is
 * not true any more. Otherwise no, and the client waits like it should.
 */
export function shouldReask(
  {acked, sinceChangeMs}: {acked: boolean; sinceChangeMs: number},
  stallMs = STALL_MS,
): boolean {
  return !acked || sinceChangeMs >= stallMs;
}

/** Did the selection include anything the server still has to make? */
export function anyEstimated(rows: readonly ChapRow[]): boolean {
  return rows.some((r) => r.bytes == null);
}
