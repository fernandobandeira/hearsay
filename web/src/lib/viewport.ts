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

/* ------------------------------------------------------ the bottom black band
 *
 * The other iOS standalone viewport fault, and the one that is not a bug in a
 * browser somewhere - it is how iOS has always laid an installed web app out
 * edge to edge. The page starts at the top of the *screen*, behind the status
 * bar, and is then handed a layout viewport one status bar *shorter* than the
 * screen. So `100dvh` reaches ~59 px short of the bottom edge, the player bar
 * stops there, and what is under it is the background colour of whatever is
 * painted behind the page: a black band, on every screen, for good.
 *   - https://developer.apple.com/forums/thread/110854
 *
 * The usual fix is `calc(100dvh + env(safe-area-inset-top))`, which assumes the
 * shortfall is exactly the top inset. It is near enough, but it is an
 * assumption, and it keeps being applied on the day WebKit stops doing this.
 * The shortfall can simply be measured instead: `screen.height` is the screen,
 * `innerHeight` is what the page got, and the difference is what to give back.
 */

/** The safe-area insets, in px, as this device actually reports them.
 *
 * Measured off a throwaway element rather than read from `--sat`/`--sab`: an
 * unregistered custom property computes to the *text* `env(safe-area-inset-top,
 * 0px)`, so asking the root for it gives a string and not a number. Spending it
 * as padding is what resolves it. */
export function readInsets(): {top: number; bottom: number} {
  if (typeof document === 'undefined') return {top: 0, bottom: 0};
  const probe = document.createElement('div');
  probe.style.cssText = 'position:absolute;visibility:hidden;top:0;left:0;'
    + 'padding-top:env(safe-area-inset-top,0px);padding-bottom:env(safe-area-inset-bottom,0px)';
  document.body.appendChild(probe);
  const cs = getComputedStyle(probe);
  const insets = {
    top: Math.round(parseFloat(cs.paddingTop) || 0),
    bottom: Math.round(parseFloat(cs.paddingBottom) || 0),
  };
  probe.remove();
  return insets;
}

/**
 * How many pixels short of the screen the layout viewport is - i.e. how much
 * `100dvh` has to be given back.
 *
 * It answers 0 for everything that is not this fault, because every one of
 * those would be a page taller than the screen it is on, with the player bar
 * pushed off the bottom - a worse bug than the band:
 *
 *   no top inset: the page is *not* laid out edge to edge (a browser tab, or a
 *     status bar style that insets the web view itself), so a short viewport is
 *     the whole viewport and there is nothing missing;
 *   not the full width of the screen: an iPad split view or a desktop window,
 *     where the height is short because the window is;
 *   short by more than a status bar: the keyboard shrink that `shouldHeal`
 *     owns, which is that other bug and not this one.
 */
export function viewportShortfall(
  v: {
    innerWidth: number; innerHeight: number;
    screenWidth: number; screenHeight: number;
    insetTop: number;
  },
  slack = 8,
): number {
  const {innerWidth: w, innerHeight: h, insetTop} = v;
  if (!Number.isFinite(w) || !Number.isFinite(h) || h <= 0 || w <= 0) return 0;
  if (!(insetTop > 0)) return 0;

  // screen.width/height do not agree across iOS versions about whether they
  // turn over with the device, so the orientation is taken from the viewport,
  // which always knows.
  const long = Math.max(v.screenWidth, v.screenHeight);
  const short = Math.min(v.screenWidth, v.screenHeight);
  const [sw, sh] = w > h ? [long, short] : [short, long];
  if (!Number.isFinite(sh) || sh <= 0) return 0;
  if (Math.abs(w - sw) > 1) return 0;

  const missing = sh - h;
  if (missing <= 0 || missing > insetTop + slack) return 0;
  return missing;
}
