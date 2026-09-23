/**
 * The reader.
 *
 * One view over the chapter, because there was only ever one: the words. Nothing
 * playing means reading; playing means the same page, following along. The old
 * "read with your eyes" toggle described the default as if it were a mode, so it
 * is gone - along with its keybinding.
 *
 * Structurally this component is one page and two overlays, and nothing else.
 * They are the direct children of `#root`, which index.css makes the whole
 * screen; the words fill it, and the bars sit over the two edges. The words
 * clear the bars by padding themselves with `--bar-top` / `--bar-bottom`, which
 * are the bars' own measured heights - `useBarInsets` below reads them off the
 * elements, so nothing here restates a height that could drift. Read the
 * scaffold comment in index.css before changing any of these class lists.
 */
import {useEffect, useRef, useState} from 'react';
import {BookOpen, CloudOff} from 'lucide-react';
import {FollowOffer, PlayerBar, TopBar, useAutoHide} from '@/components/Chrome';
import {Library} from '@/components/Library';
import {ChapterSkeleton, ReadingPage} from '@/components/ReadingPage';
import {isRotation, readInsets, shouldHeal, trackBaseline, viewportShortfall} from '@/lib/viewport';
import {useNarrator} from '@/state';

export default function App() {
  const n = useNarrator();
  const [drawer, setDrawer] = useState(true);
  const visible = useAutoHide() || drawer;
  useViewportHeal();
  useViewportFill();
  useBarInsets();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.tagName === 'INPUT' || t?.tagName === 'TEXTAREA') return;
      /* A focused control that handles its own arrows has already claimed the
         key. The text-size slider is the one that made this necessary: it lives
         in the top bar now, and six taps of ArrowRight on its thumb turned the
         page six chapters forward while also resizing the text. Radix calls
         preventDefault on the keys it uses, and this listener is on `window`,
         so by the time it runs the flag is set. */
      if (e.defaultPrevented || t?.closest?.('[data-slot="slider"]')) return;
      if (e.key === 'Escape') { setDrawer(false); return; }
      if (e.code === 'Space') { e.preventDefault(); n.toggle(); }
      else if (e.key === 'ArrowLeft') { e.preventDefault(); n.goChapter(-1); }
      else if (e.key === 'ArrowRight') { e.preventDefault(); n.goChapter(1); }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [n]);

  const busy = n.bookLoading || n.chapterLoading;

  return (
    <>
      <TopBar visible={visible} onMenu={() => setDrawer((d) => !d)} />
      {/* The page is the whole box, under both bars - but not under the status
          bar. `--sat` is where the words start and no higher: the system paints
          the clock and the battery over that strip, and text sliding beneath
          them is not reading space. Everything below it, down to the bottom
          edge, is the reader's. */}
      <main className="absolute inset-x-0 bottom-0 top-[var(--sat)] overflow-hidden">
        {n.chunks.length ? (
          <ReadingPage
            chunks={n.chunks} paras={n.paras} idx={n.idx}
            fontScale={n.fontScale} playing={n.playing} loading={n.chapterLoading}
            onPick={(i) => n.setIdx(i)}
            nextTitle={n.chapters[n.ci + 1]?.title ?? null}
            onNext={() => n.goChapter(1)}
          />
        ) : busy ? (
          <ChapterSkeleton />
        ) : n.book && n.message ? (
          /* A chapter that could not be opened. The message used to go only to
             the top bar, which hides itself after two seconds of stillness - so
             a chapter picked from the drawer with its words not on the device
             flashed an explanation and then sat on "Open the library to choose a
             book", as if nothing had been asked for at all. The words are the
             page, so when there are none the reason belongs on the page. */
          <div data-testid="chapter-miss"
               className="absolute inset-0 flex flex-col items-center justify-center gap-3
                          px-6 text-center text-sm text-muted-foreground">
            <CloudOff className="size-5" />
            <span className="max-w-[28em]">{n.message}</span>
            <button onClick={() => setDrawer(true)}
                    className="border-b border-border pb-0.5 text-xs hover:text-foreground">
              choose another chapter
            </button>
          </div>
        ) : (
          <button onClick={() => setDrawer(true)}
                  className="absolute inset-0 flex flex-col items-center justify-center gap-3
                             px-6 text-center text-sm text-muted-foreground">
            <BookOpen className="size-5" />
            Open the library to choose a book.
          </button>
        )}
        <FollowOffer />
      </main>
      <PlayerBar visible={visible} />
      <Library open={drawer} onOpenChange={setDrawer} />
    </>
  );
}

/**
 * Undo the iOS standalone keyboard shrink - see lib/viewport.ts for the bug.
 *
 * The chapter filter box is the only text input in the reader and it is enough
 * to trigger it: once the keyboard has been up, `100dvh` is a status bar short
 * for the rest of the session and everything bottom-anchored floats above a
 * dead band. So after a blur, if the viewport came back smaller than the
 * largest it has been at this orientation, `#root` is hidden and shown again
 * with a forced reflow in between, which is what makes WebKit re-measure.
 *
 * Nothing here fires on a browser that does not have the bug: `shouldHeal` is
 * false whenever the viewport is its usual size.
 */
function useViewportHeal() {
  const baseline = useRef<number | null>(null);
  const size = useRef({w: 0, h: 0});
  useEffect(() => {
    const read = () => ({w: window.innerWidth, h: window.innerHeight});
    size.current = read();
    baseline.current = trackBaseline(null, window.innerHeight);

    const onResize = () => {
      const now = read();
      if (isRotation(size.current, now)) baseline.current = null;
      size.current = now;
      baseline.current = trackBaseline(baseline.current, now.h);
    };

    const heal = () => {
      if (!shouldHeal({innerHeight: window.innerHeight, baseline: baseline.current})) return;
      const root = document.getElementById('root');
      if (!root) return;
      const keep = document.querySelector<HTMLElement>('[data-testid="reading"]')?.scrollTop ?? 0;
      root.style.display = 'none';
      void root.offsetHeight;                       // the reflow is the point
      root.style.display = '';
      requestAnimationFrame(() => {
        const box = document.querySelector<HTMLElement>('[data-testid="reading"]');
        if (box) box.scrollTop = keep;
      });
    };

    // `focusout` bubbles where `blur` does not, so one listener covers every
    // input the drawer ever grows.
    const onFocusOut = () => setTimeout(heal, 350);
    window.addEventListener('resize', onResize);
    window.visualViewport?.addEventListener('resize', onResize);
    document.addEventListener('focusout', onFocusOut);
    return () => {
      window.removeEventListener('resize', onResize);
      window.visualViewport?.removeEventListener('resize', onResize);
      document.removeEventListener('focusout', onFocusOut);
    };
  }, []);
}

/**
 * Give back what iOS took off the bottom of the viewport - see lib/viewport.ts
 * for the fault and index.css for what `--vh-extra` does with the answer.
 *
 * Measured, not assumed, and re-measured on every resize and rotation: the
 * shortfall is a property of how this device laid this app out, and the one
 * thing worse than the black band would be a page taller than the screen with
 * the player bar pushed off the bottom of it. `viewportShortfall` answers 0 for
 * everything that is not this fault, so on a desktop, in a browser tab and on a
 * phone without a notch this hook writes `0px` and changes nothing.
 */
function useViewportFill() {
  useEffect(() => {
    const apply = () => {
      const {screen} = window;
      const extra = viewportShortfall({
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        screenWidth: screen?.width ?? 0,
        screenHeight: screen?.height ?? 0,
        insetTop: readInsets().top,
      });
      document.documentElement.style.setProperty('--vh-extra', `${extra}px`);
    };
    apply();
    window.addEventListener('resize', apply);
    window.addEventListener('orientationchange', apply);
    window.visualViewport?.addEventListener('resize', apply);
    return () => {
      window.removeEventListener('resize', apply);
      window.removeEventListener('orientationchange', apply);
      window.visualViewport?.removeEventListener('resize', apply);
    };
  }, []);
}

/**
 * The bars' heights, published as `--bar-top` and `--bar-bottom`.
 *
 * The bars overlay the words, so the reading column has to pad itself by
 * exactly as much as they cover - and the previous scaffold's whole complaint
 * was about `calc()`s restating a bar's height and then being wrong by 16 px
 * for a year. So nothing restates anything: the bars size themselves from their
 * content as they always did, and what they end up being is measured off them
 * and written to the root. A ResizeObserver rather than a one-off read, because
 * a font that lands late or a text-size slider swapping the top row are both
 * changes to a height something else is standing on.
 *
 * Hiding is opacity, so a hidden bar still reports its height - which is the
 * behaviour wanted here: the padding must not move when the bars fade.
 */
function useBarInsets() {
  useEffect(() => {
    const bars: [string, string][] = [
      ['topbar', '--bar-top'],
      ['playerbar', '--bar-bottom'],
    ];
    const seen = bars
      .map(([id, prop]) => [document.querySelector<HTMLElement>(`[data-testid="${id}"]`), prop] as const)
      .filter((pair): pair is readonly [HTMLElement, string] => pair[0] != null);
    if (!seen.length || typeof ResizeObserver === 'undefined') return;

    // A zero is never an answer: it is the moment `useViewportHeal` has #root
    // hidden, and writing it would collapse the reading column's padding.
    const write = () => {
      for (const [el, prop] of seen)
        if (el.offsetHeight > 0)
          document.documentElement.style.setProperty(prop, `${el.offsetHeight}px`);
    };
    write();
    const ro = new ResizeObserver(write);
    for (const [el] of seen) ro.observe(el);
    return () => ro.disconnect();
  }, []);
}
