/**
 * The drawer: books, then the chapter manager for whichever is open.
 *
 * Offline the server can neither list the library nor parse an EPUB, but a book
 * that has been opened once has its table of contents in localStorage and its
 * words in Cache Storage - which is the whole point of taking them. So the list
 * falls back to what this device already knows rather than showing an error.
 */
import {useState} from 'react';
import {Type} from 'lucide-react';
import {Sheet, SheetContent, SheetHeader, SheetTitle} from '@/components/ui/sheet';
import {ScrollArea} from '@/components/ui/scroll-area';
import {Separator} from '@/components/ui/separator';
import {Skeleton} from '@/components/ui/skeleton';
import {Slider} from '@/components/ui/slider';
import {Spinner} from '@/components/ui/spinner';
import {useBooks} from '@/lib/api';
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
  const list: BookFile[] = books.data?.length ? books.data : knownBooks();
  const loadingList = books.isPending && !list.length;

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
        <SheetHeader className="px-4 pb-1 pt-4">
          <SheetTitle className="text-[10px] font-normal uppercase tracking-[0.18em] text-muted-foreground">
            Books
          </SheetTitle>
        </SheetHeader>

        <ScrollArea className="max-h-[26vh] shrink-0">
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
              onClick={() => { setOpening(b.path); void n.openBook(b).finally(() => setOpening(null)); }}
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

        <Separator className="my-2" />

        {/* The one thing the removed reading-mode button actually did. */}
        <div className="flex items-center gap-3 px-4 pb-2">
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

        <Separator className="mb-2" />
        <div className="px-4 pb-1 text-[10px] font-normal uppercase tracking-[0.18em] text-muted-foreground">
          Chapters
        </div>

        {n.book
          ? <ChapterManager open={open} onPick={(ci) => { onOpenChange(false); void n.openChapter(ci, 0); }} />
          : <div className="px-4 py-3 text-xs text-muted-foreground">no book loaded</div>}
      </SheetContent>
    </Sheet>
  );
}
