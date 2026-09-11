/**
 * The reader.
 *
 * One view over the chapter, because there was only ever one: the words. Nothing
 * playing means reading; playing means the same page, following along. The old
 * "read with your eyes" toggle described the default as if it were a mode, so it
 * is gone - along with its keybinding.
 *
 * Structurally this component is three rows and nothing else. They are the
 * direct children of `#root`, which index.css makes a `100dvh` flex column; the
 * middle one is `flex-1 min-h-0`, so the words get exactly the screen minus the
 * two bars, with no `calc()` restating either bar's height. Read the scaffold
 * comment in index.css before changing any of these three class lists.
 */
import {useEffect, useRef, useState} from 'react';
import {BookOpen} from 'lucide-react';
import {FollowOffer, PlayerBar, TopBar, useAutoHide} from '@/components/Chrome';
import {Library} from '@/components/Library';
import {ChapterSkeleton, ReadingPage} from '@/components/ReadingPage';
import {isRotation, shouldHeal, trackBaseline} from '@/lib/viewport';
import {useNarrator} from '@/state';

export default function App() {
  const n = useNarrator();
  const [drawer, setDrawer] = useState(true);
  const visible = useAutoHide() || drawer;
  useViewportHeal();

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
      {/* `min-h-0` is the load-bearing half: without it a flex item's automatic
          minimum size is its content, so a long chapter would push the player
          bar off the bottom of the screen instead of scrolling. */}
      <main className="relative min-h-0 flex-1 overflow-hidden">
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
