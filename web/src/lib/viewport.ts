/**
 * The iOS standalone viewport heal.
 *
 * The scaffold in index.css is `100dvh` and that is correct - but there is one
 * iOS 17/18 bug that breaks the unit itself, and this reader walks straight
 * into it: the chapter drawer has a filter box. In an *installed* PWA, the first
 * time the software keyboard opens, the layout viewport shrinks and never grows
 * back for the rest of the session - `window.innerHeight`,
 * `visualViewport.height` and therefore `100dvh` all drop together (reported as
 * 932 -> 873 on an iPhone Pro Max, i.e. one status-bar's worth) and stay there.
 * The symptom is every bottom-anchored control floating ~59 px too high with a
 * dead band under it, on every screen, until the app is force-quit.
 *
 * `interactive-widget=resizes-content` does not help: WebKit does not implement
 * it, and it is ignored outright in standalone mode. Swapping `100dvh` for
 * `height: 100%` does not prevent it either. The only known fix is to make
 * WebKit re-measure: hide a full-viewport-height element, force a synchronous
 * reflow, show it again.
 *   - https://dev.to/cederhook/fixing-the-ios-standalone-pwa-keyboard-bug-that-shrinks-your-viewport-for-good-63d
 *   - https://github.com/WebKit/standards-positions/issues/65
 *
 * Only the device can prove this one: headless Chrome does not reproduce the
 * bug, so what is tested here is the decision - when a heal is called for - and
 * not the reflow itself.
 */

/** The largest height seen since the last orientation change. */
export function trackBaseline(prev: number | null, innerHeight: number): number {
  if (!Number.isFinite(innerHeight) || innerHeight <= 0) return prev ?? 0;
  return prev == null ? innerHeight : Math.max(prev, innerHeight);
}

/**
 * Has the viewport shrunk and stayed shrunk?
 *
 * `tolerance` keeps normal sub-pixel and toolbar noise out of it; the bug is a
 * whole status bar, tens of pixels. A viewport that is *larger* than the
 * baseline is not a fault - that is a rotation, and the caller resets.
 */
export function shouldHeal(
  {innerHeight, baseline}: {innerHeight: number; baseline: number | null},
  tolerance = 8,
): boolean {
  if (baseline == null || !Number.isFinite(innerHeight) || innerHeight <= 0) return false;
  return innerHeight < baseline - tolerance;
}

/** A rotation invalidates the baseline; anything else does not. */
export function isRotation(
  before: {w: number; h: number}, after: {w: number; h: number},
): boolean {
  return before.w !== after.w || (before.h > before.w) !== (after.h > after.w);
}
