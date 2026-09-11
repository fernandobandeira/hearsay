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
