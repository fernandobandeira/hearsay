/**
 * Trimming the wake.
 *
 * He reads one book front to back - a 1433-chapter novel at the moment - and
 * downloads chapters ahead of himself. The ones behind never come up again, and
 * each is tens of megabytes on a phone whose quota is finite and unasked-for. So
 * the device keeps a window rather than a hoard: arriving in a new chapter gives
 * back the chapters well behind it.
 *
 * Two rules keep it from ever eating something wanted:
 *
 *   anchored to the furthest point reached, not to the current chapter. Scrolling
 *   back to re-read a scene must not drag the window backwards, and must never
 *   make the trim *wider* - so the anchor only ever moves forward.
 *
 *   nothing protected is touched. The chapter open right now is protected even
 *   when it sits far behind the anchor, because the one thing worse than keeping
 *   a chapter too long is deleting the one being listened to.
 *
 * Everything here is a device eviction: the server keeps its rendered and packed
 * files, so a trimmed chapter is one download away from coming back. And it is a
 * decision only - nothing in this file touches Cache Storage, which is what makes
 * the whole rule testable without a browser.
 */

/** How many chapters behind the furthest point survive. F, F-1 and F-2 stay. */
export const KEEP_BEHIND = 2;

const whole = (n: unknown): number => {
  const v = Math.floor(Number(n));
  return Number.isFinite(v) && v > 0 ? v : 0;
};

/** The anchor, which only ever moves forward. */
export function furthestReached(previous: number | null | undefined, ci: number): number {
  return Math.max(whole(previous), whole(ci));
}

/**
 * Which downloaded chapters are far enough behind to give back.
 *
 * `furthest` is the anchor; `protect` is whatever must survive regardless (the
 * chapter in hand). Returns them in reading order, which is also the order they
 * are cheapest to delete in and the order a log of it would want to be read in.
 */
export function chaptersToTrim(
  downloaded: Iterable<number>,
  furthest: number,
  keepBehind: number = KEEP_BEHIND,
  protect: Iterable<number> = [],
): number[] {
  const edge = whole(furthest) - whole(keepBehind);   // first chapter worth keeping
  if (edge <= 0) return [];
  const safe = new Set<number>(protect);
  const out = new Set<number>();
  for (const ci of downloaded)
    if (Number.isInteger(ci) && ci >= 0 && ci < edge && !safe.has(ci)) out.add(ci);
  return [...out].sort((a, b) => a - b);
}
