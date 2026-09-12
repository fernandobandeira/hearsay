/**
 * Trimming the wake.
 *
 * He reads one book front to back - a 1433-chapter novel at the moment - and
 * downloads chapters ahead of himself. The ones behind never come up again, and
 * each is tens of megabytes on a phone whose quota is finite and unasked-for. So
 * the device keeps a window rather than a hoard: arriving in a new chapter gives
 * back the chapters well behind it.
 *
 * Three rules keep it from ever eating something wanted:
 *
 *   anchored to the furthest point reached, not to the current chapter. Scrolling
 *   back to re-read a scene must not drag the window backwards, and must never
 *   make the trim *wider* - so the anchor only ever moves forward.
 *
 *   nothing protected is touched. The chapter open right now is protected even
 *   when it sits far behind the anchor, because the one thing worse than keeping
 *   a chapter too long is deleting the one being listened to.
 *
 *   nothing recently touched is taken, however far behind it is. Position alone
 *   turned out to be a bad proxy for "finished with it": jumping forward to look
 *   something up moves the anchor permanently, and coming back found every
 *   chapter in between deleted - chapters downloaded on purpose, minutes
 *   earlier, over a tunnel, on a box that renders at a quarter of realtime. So
 *   a chapter has to be *both* well behind the anchor and untouched for
 *   `KEEP_UNTOUCHED_MS` before it is a candidate. "Touched" is stored as much as
 *   read: a chapter downloaded ahead and not reached yet has been touched, which
 *   is what makes a forward jump survivable.
 *
 * Everything here is a device eviction: the server keeps its rendered and packed
 * files, so a trimmed chapter is one download away from coming back. And it is a
 * decision only - nothing in this file touches Cache Storage, which is what makes
 * the whole rule testable without a browser.
 */

/** How many chapters behind the furthest point survive. F, F-1 and F-2 stay. */
export const KEEP_BEHIND = 2;

/**
 * How long a chapter survives past the window on having been touched recently.
 *
 * Two days. Long enough that a jump forward and back across an evening - the
 * case this exists for - costs nothing, and that a chapter downloaded overnight
 * is still there the next night. Short enough that the wake of a book read daily
 * does not become the whole book: at his pace that is tens of chapters, a few
 * hundred megabytes, against a quota that runs to gigabytes.
 *
 * It is a floor on deletion, not a promise of deletion: a chapter is given back
 * once it is both stale *and* well behind, and the device's own eviction is what
 * bounds the total. Getting this wrong in the generous direction costs storage;
 * getting it wrong in the mean direction costs a re-render on a box that takes
 * the better part of an hour per chapter.
 */
export const KEEP_UNTOUCHED_MS = 48 * 60 * 60 * 1000;

const whole = (n: unknown): number => {
  const v = Math.floor(Number(n));
  return Number.isFinite(v) && v > 0 ? v : 0;
};

/** The anchor, which only ever moves forward. */
export function furthestReached(previous: number | null | undefined, ci: number): number {
  return Math.max(whole(previous), whole(ci));
}

/**
 * Which downloaded chapters are far enough behind, and cold enough, to give back.
 *
 * `furthest` is the anchor; `protect` is whatever must survive regardless (the
 * chapter in hand); `touched` maps a chapter to when this device last read or
 * stored it, and a chapter touched inside `keepUntouchedMs` is kept wherever it
 * sits. A chapter with no entry at all is one this device has no memory of
 * having touched - an old download from before the log existed, or one whose
 * entry was pruned - and it is treated as cold, because the alternative is a
 * device that can never give anything back.
 *
 * Returns them in reading order, which is also the order they are cheapest to
 * delete in and the order a log of it would want to be read in.
 */
export function chaptersToTrim(
  downloaded: Iterable<number>,
  furthest: number,
  keepBehind: number = KEEP_BEHIND,
  protect: Iterable<number> = [],
  touched: ReadonlyMap<number, number> = new Map(),
  now: number = Date.now(),
  keepUntouchedMs: number = KEEP_UNTOUCHED_MS,
): number[] {
  const edge = whole(furthest) - whole(keepBehind);   // first chapter worth keeping
  if (edge <= 0) return [];
  const safe = new Set<number>(protect);
  const warm = (ci: number): boolean => {
    const at = touched.get(ci);
    // A stamp from the future is a clock that moved, not a chapter to delete.
    return at != null && Number.isFinite(at) && now - at < whole(keepUntouchedMs);
  };
  const out = new Set<number>();
  for (const ci of downloaded)
    if (Number.isInteger(ci) && ci >= 0 && ci < edge && !safe.has(ci) && !warm(ci)) out.add(ci);
  return [...out].sort((a, b) => a - b);
}
