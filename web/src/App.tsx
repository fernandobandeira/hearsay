/**
 * The reader.
 *
 * One view over the chapter, because there was only ever one: the words. Nothing
 * playing means reading; playing means the same page, following along. The old
 * "read with your eyes" toggle described the default as if it were a mode, so it
 * is gone - along with its keybinding.
 */
import {useEffect, useState} from 'react';
import {BookOpen} from 'lucide-react';
import {BottomBar, TopBar, useAutoHide} from '@/components/Chrome';
import {Library} from '@/components/Library';
import {ChapterSkeleton, ReadingPage} from '@/components/ReadingPage';
import {useNarrator} from '@/state';

export default function App() {
  const n = useNarrator();
  const [drawer, setDrawer] = useState(true);
  const visible = useAutoHide() || drawer;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.target as HTMLElement)?.tagName === 'INPUT') return;
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
      {/* The bar is 76px of controls plus its own bottom padding, which is
          max(1rem, inset) — the same expression, so the two cannot drift.
          The top bar puts its inset *above* a 3.5rem row, so that one adds. */}
      <main className="fixed inset-x-0 bottom-[calc(76px+max(1rem,env(safe-area-inset-bottom)))] top-[calc(3.5rem+env(safe-area-inset-top))] overflow-hidden">
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
      </main>
      <TopBar visible={visible} onMenu={() => setDrawer((d) => !d)} />
      <BottomBar visible={visible} />
      <Library open={drawer} onOpenChange={setDrawer} />
    </>
  );
}
