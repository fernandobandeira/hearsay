/**
 * One retry policy, used everywhere.
 *
 * The reader lives on a phone behind a tailnet tunnel. The server restarts, wifi
 * hands over to LTE, the tunnel blips - none of which should ever reach the user
 * as something to act on. TanStack Query retries every request with this curve,
 * and the media player uses the same one when a load fails.
 *
 * Jitter is not decoration: without it every queued call retries on the same tick
 * and hammers a server that has just come back.
 */
export interface Policy {
  base: number;    // first delay, ms
  factor: number;  // multiplier per attempt
  max: number;     // ceiling for a single delay, ms
  jitter: number;  // 0..1 - the fraction of the delay that is random
}

export const DEFAULT: Policy = {base: 400, factor: 2, max: 15_000, jitter: 0.5};
/** Media reload: the first retry should be quick enough to pass for a gap. */
export const MEDIA: Policy = {base: 250, factor: 2, max: 6_000, jitter: 0.5};

/** Delay before attempt `attempt` (0 = the first retry). */
export function delayFor(attempt: number, opts?: Partial<Policy>, random?: () => number): number {
  const p = {...DEFAULT, ...opts};
  const rnd = random ?? Math.random;
  const flat = Math.min(p.max, p.base * Math.pow(p.factor, Math.max(0, attempt)));
  const jit = Math.max(0, Math.min(1, p.jitter));
  return Math.round(flat * (1 - jit) + flat * jit * rnd());
}

/**
 * Is this failure worth repeating? A 4xx means we asked wrongly and asking again
 * will fail the same way - except 408/429, which are explicitly "later".
 */
export function isRetryable(status: number): boolean {
  if (status === 0) return true;               // transport failure, no response
  if (status === 408 || status === 429) return true;
  return status >= 500;
}

/** How many times a request is worth repeating before it is simply broken. */
export const MAX_RETRIES = 5;

/**
 * The status of a failure, whatever threw it.
 *
 * Duck-typed on purpose: `ApiError` lives in api.ts, api.ts imports this module,
 * and reading `.status` off an unknown keeps this file at the bottom of the
 * import graph instead of in a cycle with the thing it is a policy for.
 * Anything with no status is a transport failure, which is status 0.
 */
export function statusOf(error: unknown): number {
  const s = (error as {status?: unknown} | null | undefined)?.status;
  return typeof s === 'number' ? s : 0;
}

/** The default policy: the curve above, five times, for failures worth repeating. */
export function retryQuery(count: number, error: unknown): boolean {
  return count < MAX_RETRIES && isRetryable(statusOf(error));
}

/**
 * The same policy, except that being offline ends it immediately.
 *
 * This exists for one shape of call: an **awaited** `fetchQuery` that has a
 * fallback behind it. Every query here is `networkMode: 'offlineFirst'`, because
 * the service worker answers plenty of requests with no network - but when a
 * request genuinely fails and a retry is scheduled, TanStack Query *pauses* the
 * retry until the browser is online again rather than rejecting. For a hook that
 * is right: the screen keeps its last data and the query resumes on reconnect.
 * For an awaited call it is fatal - the promise never settles, so the
 * `.catch(() => null)` that was supposed to move on to the cached copy never
 * runs, and the reader shows a loading skeleton for as long as the plane is in
 * the air. That was the bug: in airplane mode only the chapter that was already
 * open would display, with every shard of the book sitting in Cache Storage.
 *
 * Offline, the first failure settles and the caller falls through to the cache.
 * Online, this is the default policy exactly - same curve, same five attempts.
 *
 * One case is deliberately left alone: query-core's `canContinue()` gates on the
 * focus manager as well, so a document that is hidden pauses in the same way
 * even with a network. That one is not this bug and the trade is different - a
 * backgrounded tab giving up after a single attempt on a slow connection is
 * worse than one that waits to be looked at again.
 */
export function retryWhileOnline(isOnline: () => boolean) {
  return (count: number, error: unknown): boolean => isOnline() && retryQuery(count, error);
}
