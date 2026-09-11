/**
 * The drawer's two-level stack: Books, then the Chapters of whichever is open.
 *
 * Three rules live here rather than in the component, because all three are the
 * kind of thing that is wrong in a way you only notice on the big book:
 *
 *   which view      opening the drawer with a book open must land on that book's
 *                   chapters, not on the library he already chose from.
 *   which row       the chapter being read, *if the filter has not hidden it* -
 *                   scrolling to a row that is not in the list is how you end up
 *                   scrolled to nowhere.
 *   how far         centred, not "scrolled into view". At chapter 700 of 1433 the
 *                   difference between the two is the whole point: `scrollIntoView`
 *                   leaves the row wherever the least work puts it, usually the
 *                   very top or bottom edge, with no context either side.
 */

/** The drawer shows exactly one of these at a time. */
export type DrawerView = 'books' | 'chapters';

/**
 * The view the drawer opens on. A book is open → its chapters; nothing open →
 * the library, because there is nothing to show chapters of.
 */
export function initialView(hasOpenBook: boolean): DrawerView {
  return hasOpenBook ? 'chapters' : 'books';
}

/**
 * Where in the *rendered* list the chapter being read sits, or null if it is not
 * in it at all (no book, or the filter box is hiding it). `visible` is the
 * chapter indices actually rendered, in render order - so this is a position in
 * that list, not a chapter index.
 */
export function scrollTargetIndex(
  visible: readonly number[], ci: number | null | undefined,
): number | null {
  if (ci == null || !Number.isFinite(ci)) return null;
  const at = visible.indexOf(ci);
  return at < 0 ? null : at;
}

export interface CenterMetrics {
  /** the row's top edge, in the scroller's own coordinates */
  rowTop: number;
  rowHeight: number;
  viewportHeight: number;
  scrollHeight: number;
}

/**
 * The `scrollTop` that puts a row's middle on the viewport's middle, clamped to
 * what the scroller can actually reach: a row in the first or last half-screen
 * cannot be centred, and asking for it must not scroll past the ends.
 */
export function centeredScrollTop(m: CenterMetrics): number {
  const max = Math.max(0, m.scrollHeight - m.viewportHeight);
  const want = m.rowTop + m.rowHeight / 2 - m.viewportHeight / 2;
  return Math.max(0, Math.min(max, Math.round(want)));
}
