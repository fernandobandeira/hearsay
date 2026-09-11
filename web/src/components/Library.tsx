/**
 * The drawer: two levels, one at a time - Books, then the Chapters of whichever
 * book is open. It used to stack both, which meant the chapter manager of a
 * 1433-chapter book lived under a 26vh window with the library above it; now
 * picking a book *pushes* its chapters over the list, and "‹ Books" pops back.
 *
 * Opening the drawer while reading lands on Chapters, scrolled so the chapter
 * being read is in the middle of the list - see lib/drawernav.ts for the three
 * rules that decide which view, which row and how far.
 *
 * Offline the server can neither list the library nor parse an EPUB, but a book
 * that has been opened once has its table of contents in localStorage and its
 * words in Cache Storage - which is the whole point of taking them. So the list
 * falls back to what this device already knows rather than showing an error.
 */
import {useEffect, useState} from 'react';
import {ChevronLeft, Type} from 'lucide-react';
import {Sheet, SheetContent, SheetHeader, SheetTitle} from '@/components/ui/sheet';
import {ScrollArea} from '@/components/ui/scroll-area';
import {Separator} from '@/components/ui/separator';
import {Skeleton} from '@/components/ui/skeleton';
import {Slider} from '@/components/ui/slider';
import {Spinner} from '@/components/ui/spinner';
import {useBooks} from '@/lib/api';
import {initialView, type DrawerView} from '@/lib/drawernav';
import {cn} from '@/lib/utils';
import {useNarrator} from '@/state';
import {ChapterManager} from './ChapterManager';
import type {BookFile} from '@/lib/types';

const LIB_KEY = 'narrator.lib';

function knownBooks(): BookFile[] {
  try {
    const lib = JSON.parse(localStorage.getItem(LIB_KEY) ?? '{}') as
      Record<string, {path: string; name: string; key: string}>;
    return Object.values(lib).map((e) => ({path: e.path, name: e.name, key: e.key, mb: 0}));
  } catch { return []; }
}

export function Library({open, onOpenChange}: {open: boolean; onOpenChange: (b: boolean) => void}) {
  const n = useNarrator();
  const books = useBooks();
  const [opening, setOpening] = useState<string | null>(null);
  const [view, setView] = useState<DrawerView>(() => initialView(false));
  const list: BookFile[] = books.data?.length ? books.data : knownBooks();
  const loadingList = books.isPending && !list.length;

  /* Which view is decided on each open, not remembered from the last one: the
     answer depends on whether a book is open *now*. Deliberately keyed on `open`
     alone - a book finishing loading while the drawer is up must not re-push. */
  useEffect(() => {
    if (open) setView(initialView(!!n.book));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      {/* The drawer is `position: fixed`, so the body's safe-area padding does not
          reach it: without these two insets the notch covers the Books header and
          the home indicator sits on the chapter action bar. */}
      <SheetContent
        side="left"
        className="flex w-[min(400px,88vw)] flex-col gap-0 p-0
                   pt-[env(safe-area-inset-top)] pb-[env(safe-area-inset-bottom)]"
      >
        <SheetHeader className="gap-0.5 px-4 pb-1.5 pt-4">
          {view === 'books' ? (
            <SheetTitle className="text-[10px] font-normal uppercase tracking-[0.18em] text-muted-foreground">
              Books
            </SheetTitle>
          ) : (
            <>
              <button
                data-testid="drawer-back"
                onClick={() => setView('books')}
                title="Back to the library"
                className="-ml-1 flex w-fit items-center gap-0.5 rounded py-0.5 pl-0.5 pr-1.5
                           text-[10px] font-normal uppercase tracking-[0.18em] text-muted-foreground
                           transition-colors hover:text-foreground"
              >
                <ChevronLeft className="size-3" /> Books
              </button>
              <SheetTitle data-testid="drawer-title"
                          className="truncate pr-8 text-[13px] font-normal text-foreground/80">
                {n.book?.title || n.book?.name.replace(/\.epub$/i, '') || 'Chapters'}
              </SheetTitle>
            </>
          )}
        </SheetHeader>

        {/* The stack. Both levels stay mounted and slide on a transform, so the
            chapter list keeps its scroll position (and its selection) across a
            trip to the library; `inert` keeps the off-screen one out of the tab
            order and away from the pointer. */}
        <div data-testid="drawer-stack" className="relative min-h-0 flex-1 overflow-hidden">
          <div
            data-testid="view-books"
            data-active={view === 'books'}
            inert={view !== 'books'}
            className={cn('absolute inset-0 flex flex-col transition-transform duration-200 ease-out',
                          view !== 'books' && '-translate-x-full')}
          >
            <ScrollArea className="min-h-0 flex-1">
              {loadingList && (
                <div data-testid="books-skeleton" className="space-y-2 px-4 py-2">
                  {['82%', '64%', '73%', '55%'].map((w, i) => (
                    <Skeleton key={i} className="h-4 bg-white/[0.05]" style={{width: w}} />
                  ))}
                </div>
              )}
              {!loadingList && list.length === 0 && (
                <div className="px-4 py-3 text-xs text-muted-foreground">
                  {books.isError ? 'cannot reach the server, and nothing is saved here' : 'no epubs found'}
                </div>
              )}
              {list.map((b) => (
                <button
                  key={b.path}
                  data-testid="book"
                  data-key={b.key}
                  onClick={() => {
                    setOpening(b.path);
                    // Push first: the chapters view is where the book is opening,
                    // and it has its own skeleton to show while it does.
                    setView('chapters');
                    void n.openBook(b).finally(() => setOpening(null));
                  }}
                  className={cn(
                    'flex w-full items-baseline gap-2 px-4 py-2 text-left text-[13px] font-light',
                    'text-muted-foreground transition-colors hover:bg-white/5 hover:text-foreground',
                    b.path === n.book?.path &&
                      'bg-white/[0.06] text-foreground shadow-[inset_2px_0_0_var(--color-ring)]',
                  )}
                >
                  <span className="min-w-0 flex-1 truncate">{b.name.replace(/\.epub$/i, '')}</span>
                  {opening === b.path
                    ? <Spinner className="size-3 shrink-0 self-center text-muted-foreground" />
                    : b.mb ? <span className="shrink-0 text-[10px] text-muted-foreground">{b.mb}MB</span> : null}
                </button>
              ))}
            </ScrollArea>
          </div>

          <div
            data-testid="view-chapters"
            data-active={view === 'chapters'}
            inert={view !== 'chapters'}
            className={cn('absolute inset-0 flex flex-col transition-transform duration-200 ease-out',
                          view !== 'chapters' && 'translate-x-full')}
          >
            {n.book
              ? <ChapterManager
                  open={open}
                  active={view === 'chapters'}
                  onPick={(ci) => { onOpenChange(false); void n.openChapter(ci, 0); }} />
              : <div className="px-4 py-3 text-xs text-muted-foreground">no book loaded</div>}
          </div>
        </div>

        <Separator />

        {/* The one thing the removed reading-mode button actually did. It sits
            under the stack rather than inside a level: on a phone the top bar
            hides its own copy, so this is the only text-size control there is,
            and it must not be a level away. */}
        <div className="flex items-center gap-3 px-4 py-2">
          <Type className="size-3 shrink-0 text-muted-foreground" />
          <span className="shrink-0 text-[11px] text-muted-foreground">text size</span>
          <Slider className="flex-1" min={70} max={180} step={5}
                  value={[Math.round(n.fontScale * 100)]}
                  onValueChange={([v]) => n.setFontScale(v / 100)}
                  aria-label="Reading text size" />
          <span className="w-8 shrink-0 text-right text-[10px] tabular-nums text-muted-foreground">
            {Math.round(n.fontScale * 100)}%
          </span>
        </div>
      </SheetContent>
    </Sheet>
  );
}
