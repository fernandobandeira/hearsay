/**
 * The geometry of following along.
 *
 * The reading view has one job that is easy to get wrong: keep the chunk being
 * spoken in front of the eyes without ever wrestling the scroll away from a
 * reader who is moving it themselves. Three rules, all of them arithmetic, so
 * they live here rather than in the component:
 *
 *   centred, not pinned   the active chunk sits on the middle of the viewport.
 *                         Pinning it to the top hides everything it follows from
 *                         and makes the page feel like it is being yanked.
 *   only on a change      auto-scroll happens when the active chunk *moves* under
 *                         playback - never on a re-render, never on a click.
 *   yield, then offer     once the reader scrolls the active chunk out of sight,
 *                         following stops until they ask for it back.
 */

export interface Span {top: number; height: number}

/** scrollTop that puts a span's centre on the viewport's centre, clamped. */
export function centerTop(
  span: Span, view: {viewHeight: number; maxScroll: number},
): number {
  const want = span.top + span.height / 2 - view.viewHeight / 2;
  return Math.max(0, Math.min(want, Math.max(0, view.maxScroll)));
}

/**
 * Is the span within the viewport? `pad` shrinks the viewport from both edges,
 * so a chunk clinging to an edge counts as gone and following gives up cleanly
 * instead of oscillating.
 */
export function isVisible(
  span: Span, view: {scrollTop: number; viewHeight: number}, pad = 0,
): boolean {
  const top = view.scrollTop + pad;
  const bottom = view.scrollTop + view.viewHeight - pad;
  return span.top < bottom && span.top + span.height > top;
}

/** The first span the reader can actually see. Spans are in document order. */
export function firstVisible(
  spans: (Span | null)[], view: {scrollTop: number; viewHeight: number}, pad = 0,
): number {
  const edge = view.scrollTop + pad;
  let lo = 0, hi = spans.length - 1, best = -1;
  while (lo <= hi) {
    const m = (lo + hi) >> 1;
    const s = spans[m];
    if (!s) { lo = m + 1; continue; }
    if (s.top + s.height > edge) { best = m; hi = m - 1; } else lo = m + 1;
  }
  return best === -1 ? Math.max(0, spans.length - 1) : best;
}

/**
 * Where a settled scroll leaves the reading position, or null to leave it alone.
 *
 * This is bug 4 in one function. The old reader recomputed the position from the
 * scroll offset on every scroll event, so a click that set the position was
 * immediately overwritten by the next momentum frame - the position snapped back
 * to whatever was under the top of the viewport. Now a scroll only moves the
 * position when the reader has actually scrolled *away* from it, which a click
 * never does.
 */
export function scrollPick(
  {active, spans, view, playing, pad = 0}: {
    active: number;
    spans: (Span | null)[];
    view: {scrollTop: number; viewHeight: number};
    /** while audio drives the position, scrolling never sets it */
    playing: boolean;
    pad?: number;
  },
): number | null {
  if (playing) return null;
  const cur = spans[active];
  if (cur && isVisible(cur, view, pad)) return null;
  const next = firstVisible(spans, view, pad);
  return next === active ? null : next;
}
