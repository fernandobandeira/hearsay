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
 *
 * It is also where a book is given back. Removing it belongs to the book, not to
 * the chapter level: the words and every downloaded chapter go together, in one
 * act, for any book in the list - including one that is not open, which the
 * chapter level could never reach. It is a phone-sized destructive button with
 * nothing behind it to undo with, so it asks once: the first tap arms it, the
 * second does it.
 */
import {useCallback, useEffect, useMemo, useState} from 'react';
import {ChevronLeft, ChevronRight, Trash2} from 'lucide-react';
import {Sheet, SheetContent, SheetHeader, SheetTitle} from '@/components/ui/sheet';
import {ScrollArea} from '@/components/ui/scroll-area';
import {Separator} from '@/components/ui/separator';
import {Skeleton} from '@/components/ui/skeleton';
import {Spinner} from '@/components/ui/spinner';
import {useBooks, useLibrary} from '@/lib/api';
import {bookKey, heldBooks, storageEstimate} from '@/lib/offline';
import {bytes as fmtBytes} from '@/lib/format';
import {initialView, type DrawerView} from '@/lib/drawernav';
import {
  downloadCost, indexBooks, libraryState, orderBooks, positionLine, renderedPct, scanAge,
  scanStale, type LibraryCost, type LibraryPhase,
} from '@/lib/library';
import {cn} from '@/lib/utils';
import {readInsets} from '@/lib/viewport';
import {useNarrator} from '@/state';
import {ChapterManager} from './ChapterManager';
import type {BookFile, LibraryRow} from '@/lib/types';
import {Hairline, StatusIcon, type Mark, type Tone} from './StatusIcon';

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
  const [held, setHeld] = useState<Set<string>>(new Set());
  const [armed, setArmed] = useState<string | null>(null);
  const [dropping, setDropping] = useState<string | null>(null);
  const found: BookFile[] = books.data?.length ? books.data : knownBooks();
  const loadingList = books.isPending && !found.length;

  /* What the box has made of each book, which is a different question from what
     files it has. Strictly a decoration of the list above: `lib` failing (or
     never being asked, offline) leaves every row without a readiness line and
     changes nothing else. The list itself must never wait on it - a phone with
     no tunnel still has to be able to pick a book it downloaded last night. */
  const lib = useLibrary(open);
  const index = useMemo(() => indexBooks(lib.data), [lib.data]);
  /* Most recently opened first, which is also the order the render worker walks
     the library in when it has nothing under a playhead to do - so the top of
     this list is the book the box is spending its night on. Unsorted (and in the
     order the server listed them) when there is no library answer to sort by. */
  const list = orderBooks(found, bookKey, index);
  /* What a minute of packed audio weighs, from the server rather than a constant
     here: CHAPTER_BITRATE is the box's to set, and a size computed at the wrong
     rate is wrong by exactly that ratio - silently. */
  const perMin = n.status?.bitrate_bytes_per_min;

  /* Which books have anything on this device. Always asked, never remembered -
     a quota eviction (or the auto-trim) has to show up as a book with nothing
     left to remove. The open book's own offline state changing is the other
     reason to re-ask. */
  const look = useCallback(async () => { setHeld(await heldBooks()); }, []);
  useEffect(() => {
    if (!open) { setArmed(null); return; }
    void look();
  }, [open, look, n.offlineChapters, n.textShards]);

  /* An armed remove disarms itself - on a timer, and on any trip away from the
     list. A red "remove?" left sitting under a thumb is the one way this could
     delete a 400 MB download nobody asked it to. */
  useEffect(() => {
    if (!armed) return;
    const t = setTimeout(() => setArmed(null), 5_000);
    return () => clearTimeout(t);
  }, [armed]);
  useEffect(() => { if (view !== 'books') setArmed(null); }, [view]);

  const drop = async (key: string) => {
    setArmed(null);
    setDropping(key);
    try { await n.dropBook(key); } finally { setDropping(null); void look(); }
  };

  /* Which view is decided on each open, not remembered from the last one: the
     answer depends on whether a book is open *now*. Deliberately keyed on `open`
     alone - a book finishing loading while the drawer is up must not re-push. */
  useEffect(() => {
    if (open) setView(initialView(!!n.book));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      {/* Radix portals the drawer to `body`, so it is a sibling of `#root` and
          none of the scaffold's padding reaches it - it spends the insets
          itself, which is not a double count for exactly that reason. Without
          them the notch covers the Books header and the home indicator sits on
          the chapter action bar. */}
      {/* No close button: the overlay closes it, and so does the back gesture.
          A corner X costs a line of the header on a phone and sits exactly where
          the chapter list wants to start. */}
      <SheetContent
        side="left"
        showCloseButton={false}
        /* Escape in the chapter filter clears the filter, not the drawer. */
        onEscapeKeyDown={(e) => {
          if ((document.activeElement as HTMLElement | null)?.dataset.testid === 'chapter-filter') e.preventDefault();
        }}
        className="flex w-[min(400px,88vw)] flex-col gap-0 p-0
                   pt-[var(--sat)] pb-[var(--sab)] pl-[var(--sal)]"
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
              {list.map((b) => {
                /* The library's rows come from the server without a cache key,
                   so the device copy is named by the same rule the server files
                   it under - which is what lets a book that has never been
                   opened here still be recognised, and removed. */
                const key = bookKey(b);
                const title = b.name.replace(/\.epub$/i, '');
                const here = b.path === n.book?.path;
                /* Everything below is absent unless the library answered, and a
                   row without it is exactly the row that was here before. */
                const info = index.get(key);
                const ready = info && libraryState(info);
                const cost = info && downloadCost(info, perMin);
                const where = info && positionLine(info);
                const mark = ready && info ? bookMark(ready.key, info) : null;
                /* How far in, 0..1: the stored position when the library has
                   one, and for the book open here, where this device is. */
                const read = here && n.chapters.length
                  ? (n.ci + 1) / n.chapters.length
                  : info?.position
                    ? (info.position.chapter + 1)
                      / Math.max(1, info.chapters > 0 ? info.chapters : info.position.chapters_total)
                    : null;
                const tip = bookTip({
                  title, ready: ready?.tip, cost, where: here && n.chapters.length
                    ? `ch ${n.ci + 1} / ${n.chapters.length}` : where,
                  loaded: !!info?.loaded, mb: b.mb,
                });
                return (
                  <div
                    key={b.path}
                    data-testid="book-row"
                    data-key={key}
                    className={cn(
                      'flex items-center transition-colors hover:bg-white/5',
                      here && 'bg-white/[0.06] shadow-[inset_2px_0_0_var(--color-ring)]',
                    )}
                  >
                    <button
                      data-testid="book"
                      data-key={key}
                      title={tip}
                      aria-current={here ? 'true' : undefined}
                      onClick={() => {
                        setArmed(null);
                        setOpening(b.path);
                        // Push first: the chapters view is where the book is
                        // opening, and it has its own skeleton for the wait.
                        setView('chapters');
                        void n.openBook(b).finally(() => setOpening(null));
                      }}
                      className={cn(
                        'flex min-w-0 flex-1 flex-col gap-1.5 py-2.5 pl-4 pr-2 text-left text-[13px]',
                        'font-light text-foreground/75 transition-colors hover:text-foreground',
                        here && 'text-foreground',
                      )}
                    >
                      <span className="flex w-full min-w-0 items-center gap-2">
                        <span className="min-w-0 flex-1 truncate">{title}</span>
                        {opening === b.path
                          ? <Spinner className="size-3 shrink-0 text-muted-foreground" />
                          : mark && (
                            /* The readiness mark: what the box has made of this
                               book, answered without loading it. The same
                               vocabulary as the chapter rows below it. */
                            <StatusIcon testId="book-ready" phase={ready?.key}
                                        mark={mark.mark} tone={mark.tone} frac={mark.frac}
                                        label={ready?.tip ?? ''} />
                          )}
                      </span>
                      {/* How far into it the stored position is. Absent for a
                          book that has never been opened anywhere, which is
                          itself the answer. */}
                      {read != null && (
                        <Hairline testId="book-position" value={read}
                                  className="mr-1"
                                  barClassName={here ? 'bg-foreground/55' : 'bg-foreground/30'} />
                      )}
                    </button>

                    {/* Only for a book this device actually holds something for,
                        and only ever this device's copy: the server keeps its
                        files, so everything removed here is one download away. */}
                    {held.has(key) && (dropping === key ? (
                      <Spinner data-testid="book-dropping"
                               className="mr-3.5 size-3 shrink-0 text-muted-foreground" />
                    ) : armed === key ? (
                      <button
                        data-testid="book-remove-confirm"
                        aria-label={`Confirm removing this device's copy of ${title}`}
                        onClick={() => void drop(key)}
                        title={`Remove this device's copy of ${title} — the words and every downloaded chapter. The server keeps its files.`}
                        className="mr-1.5 h-9 shrink-0 rounded-md px-2.5 text-[11px] text-destructive
                                   transition-colors hover:bg-destructive/10"
                      >
                        remove?
                      </button>
                    ) : (
                      <button
                        data-testid="book-remove"
                        aria-label={`Remove this device's copy of ${title} — the words and every downloaded chapter. The server keeps its files.`}
                        onClick={() => setArmed(key)}
                        title="Remove this device's copy — the words and every downloaded chapter. The server keeps its files."
                        className="mr-1.5 flex size-9 shrink-0 items-center justify-center rounded-md
                                   text-muted-foreground/60 transition-colors hover:text-destructive"
                      >
                        <Trash2 className="size-3.5" />
                      </button>
                    ))}
                  </div>
                );
              })}

              {/* Only when it is actually stale. Every figure on every row above
                  comes from a background scan, and a stale index does not look
                  broken - it looks like a book nobody has rendered, which is the
                  one thing this screen could get wrong in silence. A line that
                  always said "scanned just now" would be noise, so it is not
                  there until the scanner has missed two of its five-minute
                  ticks. */}
              {lib.data && scanStale(lib.data.scanned_ms) && (
                <div
                  data-testid="lib-stale"
                  title="These figures come from a background scan of the server's cache, not from this moment."
                  className="px-4 py-2 text-[10px] text-muted-foreground/60"
                >
                  readiness scanned {scanAge(lib.data.scanned_ms)}
                </div>
              )}
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
        <Diagnostics />
      </SheetContent>
    </Sheet>
  );
}

/** A book's readiness in the chapter rows' icon vocabulary - see StatusIcon.tsx. */
function bookMark(key: LibraryPhase, b: LibraryRow): {mark: Mark; tone: Tone; frac: number} | null {
  switch (key) {
    case 'complete': return {mark: 'cloud', tone: 'ok', frac: 1};
    case 'ready': return {mark: 'cloud', tone: 'done', frac: 0};
    case 'rendered': return {mark: 'check', tone: 'none', frac: 1};
    case 'partial': return {mark: 'ring', tone: 'part', frac: renderedPct(b) / 100};
    case 'none': return null;
  }
}

/**
 * Everything the row used to print, as the row's tooltip: the readiness
 * sentence, where the reader is, what it weighs, and whether the server has it
 * open. Sizes are the *measured* packed bytes when there are any - "12 MB" is
 * what could come down this second - and the estimate for the rest carries its
 * "about". A book with nothing packed and no bitrate from the server says
 * nothing about size rather than a number computed from a guess.
 */
function bookTip(t: {
  title: string; ready?: string; cost?: LibraryCost | null; where?: string | null;
  loaded: boolean; mb?: number;
}): string {
  const lines = [t.title];
  if (t.where) lines.push(t.where);
  if (t.ready) lines.push(t.ready);
  const c = t.cost;
  if (c) {
    if (c.packed > 0) {
      lines.push(c.rest
        ? `${fmtBytes(c.packed)} can be downloaded now; the whole book about ${fmtBytes(c.total)}`
        : `${fmtBytes(c.packed)} — all of it downloadable`);
    } else if (c.rest) {
      lines.push(`nothing packed yet; the whole book would be about ${fmtBytes(c.rest)}`);
    }
  }
  if (t.loaded) lines.push('open on the server');
  if (t.mb) lines.push(`epub ${t.mb} MB`);
  return lines.join('\n');
}

/**
 * The diagnostics corner.
 *
 * "streaming · buffer 15 · RTF 4.7× · 2.5h cached · 0.37/50 GB" used to sit in
 * the top bar, which is the one surface that is on screen while reading - a
 * render-pipeline readout in front of the words. It has a home now, and the
 * home is shut: one line you have to tap. Everything in it answers "is the
 * server keeping up", never "what am I reading", which is why it is here and
 * not there.
 *
 * The text-size slider used to occupy this row. It moved to the top bar's `T`,
 * which is the single place it lives now - on a phone and on a desktop.
 */
function Diagnostics() {
  const n = useNarrator();
  const [open, setOpen] = useState(false);
  const [store, setStore] = useState<{usage: number; quota: number} | null>(null);
  useEffect(() => { if (open) void storageEstimate().then(setStore); }, [open]);
  const s = n.status;
  const lines: string[] = [];
  if (s) {
    if (!s.model_ready) lines.push('the voice model is still loading');
    lines.push(`server ${s.status}${s.rtf ? ` · ${s.rtf}× realtime` : ''}`);
    if (s.building != null) lines.push(`packing chapter ${s.building + 1}`);
    if (s.queue?.length) lines.push(`${s.queue.length} chapter${s.queue.length > 1 ? 's' : ''} queued to render`);
    if (s.done_min != null && s.book_min)
      lines.push(`${(s.done_min / 60).toFixed(1)}h of ${(s.book_min / 60).toFixed(1)}h rendered`);
    if (s.disk_gb != null) lines.push(`server cache ${s.disk_gb}/${s.disk_cap_gb} GB`);
  } else {
    lines.push('the server is not answering');
  }
  if (store) lines.push(`this device ${fmtBytes(store.usage)}${store.quota ? ` of ${fmtBytes(store.quota)}` : ''}`);
  /* What the page actually got of the screen, and what it is spending on the
     insets. One line, because on an installed iOS app the two numbers can
     differ - a viewport shorter than the screen is the whole vocabulary for
     "the app does not reach the bottom", and it is otherwise invisible from
     here. */
  lines.push(viewportLine());
  return (
    <div data-testid="diag" className="px-3 py-1.5">
      <button
        data-testid="diag-toggle"
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-1 rounded py-0.5 text-[10px] uppercase
                   tracking-[0.16em] text-muted-foreground/70 transition-colors hover:text-foreground"
      >
        <ChevronRight className={cn('size-3 transition-transform', open && 'rotate-90')} />
        diagnostics
      </button>
      {open && (
        <div data-testid="diag-lines" className="space-y-0.5 pl-4 pt-1 text-[11px] leading-relaxed text-muted-foreground">
          {lines.map((l) => <div key={l}>{l}</div>)}
        </div>
      )}
    </div>
  );
}

/**
 * "viewport 430x869 of 430x932 · insets 59/34 · fill +63"
 *
 * The one line that can settle an iOS layout question from the phone itself:
 * how tall the page thinks it is, how tall the screen actually is, which strips
 * of it are unsafe, and how much of the difference `--vh-extra` is giving back
 * (see the scaffold comment in index.css). A `fill +0` with the viewport short
 * of the screen is the interesting reading - it means the shortfall was
 * measured and deliberately not believed.
 */
function viewportLine(): string {
  const {top, bottom} = readInsets();
  const fill = Math.round(
    parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--vh-extra')) || 0);
  const {innerWidth: w, innerHeight: h, screen} = window;
  return `viewport ${Math.round(w)}x${Math.round(h)}`
    + ` of ${Math.round(screen.width)}x${Math.round(screen.height)}`
    + ` · insets ${top}/${bottom}`
    + ` · fill +${fill}`;
}
