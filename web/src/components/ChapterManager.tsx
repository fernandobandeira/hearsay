/**
 * The chapter manager: what replaced "render ahead N hours".
 *
 * Two things changed after the phone review, and both were about the list being
 * the point and everything around it being in the way.
 *
 * **Nothing above the list but the filter.** It used to carry two bordered
 * cards, then one line of small print - what the text copy weighs, how many
 * chapters are on the device, and a caption saying that chapters far behind the
 * playhead are trimmed - and every one of those restated per book what a
 * chapter row already says per chapter, on the one screen that is *about* those
 * rows. They are gone: book name, filter, list. The hairline under the filter
 * while the words are downloading is what is left, because it is the only one of
 * them that says something is happening right now. Text size lives in the top
 * bar's `T`, the pipeline figures live in the diagnostics corner at the bottom
 * of the Books level, and giving a copy back - the words and every downloaded
 * chapter in one act, for any book this device holds, open or not - is the Books
 * list's job.
 *
 * **Download is one action and selection is a mode.** "Render" is gone from the
 * UI entirely - see lib/download.ts. Nobody wants a rendered chapter they
 * cannot listen to offline, so a download means the whole pipeline: render the
 * chapter, pack it, put the file on this device. And picking chapters no longer
 * means hitting a 14 px checkbox: "download…" starts a mode in which whole rows
 * toggle on a tap, with "next 5", "next 20" and "rest" for the case that is
 * actually common, and a confirm bar that says how many and roughly how big
 * before anything happens. Outside a mode a tap on a row does the obvious thing
 * and opens that chapter.
 *
 * **What this component does not do any more is run the download.** It used to:
 * confirming a selection started a foreground ladder here, per chapter, in
 * component state - queue the render, poll, ask for the pack, poll, store, next
 * chapter. Everything wrong with that was a consequence of where it lived. It
 * died with the drawer and with the app, so a selection outlived its driver; it
 * walked chapters one at a time, so a 6 MB file had the whole link to itself and
 * used a fraction of it; and a chapter that failed set one shared `err` string
 * and was dropped, with the next chapter's failure overwriting the message - so
 * a failed chapter looked *skipped*, which is exactly how he reported it.
 *
 * Confirming now writes the selection to the durable queue and returns.
 * lib/reconcile.ts drives it from there, on a timer, on a live `packed` event
 * and on every cold launch, three chapters at a time. The rows below read that
 * queue rather than a local job map, which is why a chapter still says "queued"
 * after the app has been closed and re-opened.
 */
import {useEffect, useLayoutEffect, useMemo, useReducer, useRef, useState} from 'react';
import {
  Check, CircleDashed, Clock, CloudOff, Download, FileAudio, HardDriveDownload, Loader2,
  PieChart, Type,
} from 'lucide-react';
import {Button} from '@/components/ui/button';
import {Input} from '@/components/ui/input';
import {Progress} from '@/components/ui/progress';
import {ScrollArea} from '@/components/ui/scroll-area';
import {Skeleton} from '@/components/ui/skeleton';
import {useChapters} from '@/lib/api';
import {chapterState, textMark, type ChapterStateKey, type TextMark} from '@/lib/chapterstate';
import {chaptersWithText} from '@/lib/chaptertext';
import {anyEstimated, estimateBytes, queueJob} from '@/lib/download';
import {centeredScrollTop, scrollTargetIndex} from '@/lib/drawernav';
import {chosen, idle, rangeAfter, reduce} from '@/lib/selection';
import {bytes as fmtBytes} from '@/lib/format';
import {cn} from '@/lib/utils';
import {useNarrator} from '@/state';
import type {ChapRow} from '@/lib/types';

/** One icon per state, so a glance down the list reads as a picture. */
const ICON: Record<ChapterStateKey, typeof Check> = {
  packing: Loader2, saving: Loader2, downloaded: HardDriveDownload, 'to-pack': Clock,
  ready: FileAudio, rendered: Check, queued: Clock, partial: PieChart, none: CircleDashed,
};
const TONE = {
  ok: 'text-ok', work: 'text-work', part: 'text-part',
  done: 'text-foreground/50', none: 'text-muted-foreground/80',
} as const;
/** The text tier's two marks - see lib/chapterstate.ts's `textMark`. */
const TEXT_ICON: Record<TextMark['icon'], typeof Type> = {text: Type, 'no-network': CloudOff};

export function ChapterManager({open, active, onPick}: {
  /** the drawer is up: what gates the chapters query */
  open: boolean;
  /** this level of the stack is the one on screen: what triggers the centring */
  active: boolean;
  onPick: (ci: number) => void;
}) {
  const n = useNarrator();
  /* Every call here names the book. The server refuses (409) anything aimed at a
     book it is not holding, which is what closed the one race this drawer could
     not close itself: the poll says chapter 74 needs rendering, the server picks
     up a new epub, the tap lands on the other novel. */
  const book = n.book?.key ?? null;
  const {data, isPending} = useChapters(open && !!n.book, book);
  const [sel, dispatch] = useReducer(reduce, idle);
  const [filter, setFilter] = useState('');
  const [err, setErr] = useState<string | null>(null);

  const rows = useMemo(() => {
    const byIndex = new Map<number, ChapRow>();
    for (const r of data?.chapters ?? []) byIndex.set(r.i, r);
    return n.chapters.map<ChapRow>((c) => byIndex.get(c.i) ?? {
      ...c, rendered: 0, m4a: false, bytes: null, duration: null,
      // No row from the server yet: nothing is rendered, nothing is queued, and
      // the size is whatever the estimate says once a row arrives.
      est_bytes: null, queued: false, packing: false, pack_queued: false,
    });
  }, [data, n.chapters]);

  const hits = useMemo(
    () => rows.filter((r) => !filter || r.title.toLowerCase().includes(filter.toLowerCase())),
    [rows, filter]);

  /* Which chapters' words are actually on this device.
     This half of the row comes from the device alone - the index in Cache
     Storage and the shards beside it - which is the point: offline the server
     half of a row is simply absent, and "not rendered" is the only thing the
     old row could say about a chapter that cannot even be read. */
  const wordsHere = useMemo(
    () => chaptersWithText(n.index, book ?? '', n.textShards), [n.index, book, n.textShards]);
  const connected = n.conn !== 'offline';

  /* Which rows the mode may act on: what is not already here. Threaded into
     every selection event so a row that finished downloading mid-selection
     cannot stay picked. */
  const eligible = useMemo(() => {
    if (!sel.verb) return [];
    // Not what is already here, and not what is already on order: picking a
    // chapter the queue is working on would be a second ask for the same file,
    // and the row already says it is coming.
    return hits
      .filter((r) => !n.offlineChapters.has(r.i) && !n.queuedChapters.has(r.i))
      .map((r) => r.i);
  }, [hits, sel.verb, n.offlineChapters, n.queuedChapters]);
  const eligibleSet = useMemo(() => new Set(eligible), [eligible]);

  /* Centring the chapter being read.
     Once per visit to this level, never while he is scrolling: `done` is armed
     again when the drawer closes, when the stack goes back to Books, and when
     another book is picked - that last one because this component is not
     remounted by a book switch, so without it the new book's list inherits the
     old one's scroll offset. The row is measured against the scroller rather
     than trusted to be `i * rowHeight` - the rows are one line of text and could
     wrap - and the first attempt can land before the sheet's slide-in has given
     the viewport a height, so it retries on a few frames rather than silently
     scrolling nowhere. */
  const scroller = useRef<HTMLDivElement>(null);
  const done = useRef(false);
  useEffect(() => {
    if (!open || !active) done.current = false;
  }, [open, active]);
  useEffect(() => { done.current = false; }, [n.book?.key]);
  useLayoutEffect(() => {
    if (!open || !active || done.current) return;
    let frames = 0;
    let raf = 0;
    const tick = () => {
      const vp = scroller.current?.querySelector<HTMLElement>('[data-slot="scroll-area-viewport"]');
      const at = scrollTargetIndex(hits.map((r) => r.i), n.ci);
      const row = at == null
        ? null
        : vp?.querySelector<HTMLElement>(`[data-testid="chapter-row"][data-ci="${n.ci}"]`);
      if (vp && row && vp.clientHeight > 0) {
        const box = row.getBoundingClientRect();
        vp.scrollTop = centeredScrollTop({
          rowTop: box.top - vp.getBoundingClientRect().top + vp.scrollTop,
          rowHeight: box.height,
          viewportHeight: vp.clientHeight,
          scrollHeight: vp.scrollHeight,
        });
        done.current = true;
        return;
      }
      // Nothing to centre on. If the filter box is what removed it, that was his
      // doing and the list stays where it is; otherwise the row is simply not
      // rendered yet (a book switch lands here for a frame or two) and it waits.
      if (at == null && vp && filter) { done.current = true; return; }
      if (++frames < 10) raf = requestAnimationFrame(tick);
    };
    tick();
    return () => cancelAnimationFrame(raf);
  }, [open, active, hits, filter, n.ci]);

  const picks = chosen(sel);
  const pickedRows = useMemo(
    () => rows.filter((r) => sel.picked.has(r.i)), [rows, sel.picked]);

  // --------------------------------------------------------------- the order
  /**
   * Confirming a selection places an order and returns.
   *
   * That is the whole of it now. The queue in lib/reconcile.ts tells the server
   * what to render and pack, waits however many hours that takes, copies each
   * chapter onto the device three at a time, and retries what fails - on a
   * timer, on the live stream's `packed` event and on every cold launch, none of
   * which need this drawer, this component, or the app to still be open.
   *
   * The one thing worth reporting here is the order being refused outright (a
   * 409: the server is holding another book), because that is the only failure
   * a tap can cause. Everything after it is the queue's, and the rows say it.
   */
  async function order(cis: number[]) {
    setErr(null);
    try {
      await n.queueDownload(cis);
    } catch (e) {
      setErr(msg(e));
    }
  }

  function confirm() {
    const cis = picks;
    dispatch({t: 'cancel'});
    if (!cis.length || !sel.verb) return;
    void order(cis);
  }

  const loadingRows = isPending && !rows.length;
  /* What a minute of audio weighs, from the server rather than from a constant
     in this file: CHAPTER_BITRATE is the box's to set, and an estimate computed
     at the wrong rate is wrong by exactly that ratio. */
  const perMin = n.status?.bitrate_bytes_per_min;
  /* The button spins while this device has anything queued for this book - not
     while some run in this component is alive, because there is no run any more
     and the queue outlives every component that ever touched it. */
  const busy = n.queuedChapters.size > 0;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {n.textBusy && (
        <Progress data-testid="text-progress" className="mx-4 mb-1.5 h-px bg-white/[0.06]"
                  value={n.textProgress
                    ? (n.textProgress.done / Math.max(1, n.textProgress.total)) * 100
                    : 0} />
      )}

      <Input
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        placeholder="filter chapters…"
        className="mx-4 mb-2 h-8 w-[calc(100%-2rem)] bg-card text-xs"
      />

      <ScrollArea ref={scroller} data-testid="chapter-scroller" className="min-h-0 flex-1">
        <div className="pb-2">
          {loadingRows && (
            <div data-testid="chapters-skeleton" className="space-y-2 px-3 py-2">
              {['70%', '84%', '62%', '78%', '55%', '81%'].map((w, i) => (
                <Skeleton key={i} className="h-4 bg-white/[0.05]" style={{width: w}} />
              ))}
            </div>
          )}
          {hits.map((r) => {
            /* The row's badge comes from the durable queue and the row the server
               reported, in that order - never from component state. A chapter
               asked for last night still says "queued" on a phone that has been
               closed and re-opened since, which is the thing the old job map
               could not do. */
            const s = chapterState(
              r, n.offlineChapters.has(r.i), fmtBytes,
              queueJob(r, n.queuedChapters.has(r.i), n.savingChapters.has(r.i)) ?? undefined);
            const Icon = ICON[s.key];
            const mark = textMark(wordsHere ? wordsHere.has(r.i) : null, connected);
            const MarkIcon = mark ? TEXT_ICON[mark.icon] : null;
            const picked = sel.picked.has(r.i);
            const can = !sel.verb || eligibleSet.has(r.i);
            return (
              <div
                key={r.i}
                data-testid="chapter-row"
                data-ci={r.i}
                data-state={s.key}
                data-picked={picked ? '1' : undefined}
                title={sel.verb
                  ? can ? 'tap to download this chapter'
                        : n.queuedChapters.has(r.i) ? 'already in the download queue'
                        : 'already on this device'
                  // Whichever tier is the news. A chapter whose words are not
                  // here outranks anything the audio state has to say about it.
                  : mark ? `${mark.tip}\n${s.tip}` : s.tip}
                onClick={() => sel.verb
                  ? dispatch({t: 'toggle', ci: r.i, eligible})
                  : onPick(r.i)}
                className={cn(
                  // py-2 rather than py-1.5: in selection mode the row *is* the
                  // control, so it has to be worth aiming a thumb at.
                  'relative flex items-center gap-2 px-3 py-2 text-[13px]',
                  'text-muted-foreground transition-colors',
                  can ? 'cursor-pointer hover:bg-white/5 hover:text-foreground'
                      : 'cursor-default opacity-35',
                  r.i === n.ci && !picked
                    && 'bg-white/[0.06] text-foreground shadow-[inset_2px_0_0_var(--color-ring)]',
                  picked && 'bg-white/[0.1] text-foreground shadow-[inset_3px_0_0_var(--color-ok)]',
                )}
              >
                {sel.verb && (
                  <span data-testid="pick-mark"
                        className={cn('flex size-4 shrink-0 items-center justify-center',
                                      picked ? 'text-ok' : 'text-muted-foreground/30')}>
                    {picked ? <Check className="size-3.5" /> : <CircleDashed className="size-3" />}
                  </span>
                )}
                <span className="min-w-0 flex-1 truncate font-light">{r.title}</span>
                {/* The text tier, and only when it has something to say: the
                    words of this chapter are not in the copy on this device. */}
                {mark && MarkIcon && (
                  <span data-testid="chapter-text-mark"
                        data-mark={mark.icon}
                        className={cn('flex shrink-0 items-center', TONE[mark.tone])}>
                    <MarkIcon className="size-3" aria-label={mark.tip} />
                  </span>
                )}
                <span data-testid="chapter-state"
                      className={cn('flex shrink-0 items-center gap-1 text-[10px] tabular-nums tracking-wide',
                                    TONE[s.tone])}>
                  {/* A transient running under a row that is already on this
                      device - dimmer than the state itself, because the news is
                      still that the chapter is here. */}
                  {s.note && (
                    <span data-testid="chapter-note" className="text-work/70">{s.note}</span>
                  )}
                  {s.text}
                  <Icon className={cn('size-3', s.spin && 'animate-spin')} aria-label={s.tip} />
                </span>
                {r.rendered > 0 && r.rendered < r.n && (
                  <span className="absolute bottom-0 left-0 h-px bg-work"
                        style={{width: `${(r.rendered / r.n) * 100}%`}} />
                )}
              </div>
            );
          })}
          {!loadingRows && !hits.length && (
            <div className="px-4 py-3 text-xs text-muted-foreground">no match</div>
          )}
        </div>
      </ScrollArea>

      {/* Sticky: the last row of the drawer's own column, so it never scrolls
          away from the list it is about. */}
      {sel.verb ? (
        <div data-testid="confirm-bar" className="space-y-2 border-t border-border px-3 py-2">
          <div className="flex items-center gap-1">
            <span className="mr-auto text-[11px] text-muted-foreground">
              tap rows to download
            </span>
            {([['next 5', 5], ['next 20', 20], ['rest', null]] as const).map(([label, count]) => (
              <button
                key={label}
                data-testid={`bulk-${count ?? 'rest'}`}
                onClick={() => dispatch({t: 'pick', cis: rangeAfter(eligible, n.ci, count), eligible})}
                title={count == null
                  ? 'every chapter after the one being read'
                  : `the next ${count} chapters after the one being read`}
                className="rounded border border-border px-1.5 py-0.5 text-[10.5px] text-muted-foreground
                           transition-colors hover:border-ring hover:text-foreground"
              >
                {label}
              </button>
            ))}
          </div>
          <div className="flex items-center gap-2">
            <Button data-testid="sel-cancel" size="sm" variant="ghost"
                    onClick={() => dispatch({t: 'cancel'})}>
              cancel
            </Button>
            <span data-testid="sel-count" className="ml-auto text-[11px] text-muted-foreground tabular-nums">
              {picks.length
                ? <>{picks.length} chapter{picks.length > 1 ? 's' : ''}
                    {sizeOf(pickedRows, perMin) && ` · ${sizeOf(pickedRows, perMin)}`}</>
                : 'none picked'}
            </span>
            <Button data-testid="sel-confirm" size="sm"
                    disabled={!picks.length} onClick={confirm}>
              <Download className="size-3.5" />
              download
            </Button>
          </div>
        </div>
      ) : (
        <div className="flex items-center gap-2 border-t border-border px-3 py-2">
          {/* Never disabled while the queue runs. The queue is a queue: adding
              to it is the obvious thing to do while it is working, and the old
              button - disabled for as long as anything was in flight - made a
              night's download feel like a mode he had to wait out. */}
          <Button data-testid="start-download" size="sm" variant="outline"
                  onClick={() => dispatch({t: 'start', verb: 'download'})}
                  title="Pick chapters to keep on this device. The server renders whatever needs it first.">
            {busy ? <Loader2 className="size-3.5 animate-spin" /> : <Download className="size-3.5" />}
            download…
          </Button>
          {/* The queue, said out loud. It outlives this drawer and this app, so
              it has to be visible somewhere that is not a row you have scrolled
              past - and it has to be possible to change your mind. */}
          {busy && (
            <span data-testid="queue-count"
                  className="min-w-0 truncate text-[11px] text-muted-foreground tabular-nums">
              {n.queuedChapters.size} queued
              {n.savingChapters.size > 0 && ` · ${n.savingChapters.size} saving`}
              <button
                data-testid="queue-cancel"
                onClick={() => void n.unqueueDownload([...n.queuedChapters])}
                title="Stop waiting for these. Chapters already on this device stay."
                className="ml-1.5 underline decoration-dotted underline-offset-2
                           transition-colors hover:text-foreground"
              >
                cancel
              </button>
            </span>
          )}
          {err && (
            <span data-testid="chapter-error" className="ml-auto min-w-0 truncate text-[11px] text-destructive">
              {err}
            </span>
          )}
        </div>
      )}
    </div>
  );
}

/** "~45 MB" while anything in the selection has yet to be made. */
function sizeOf(rows: ChapRow[], bytesPerMin?: number): string {
  if (!rows.length) return '';
  const n = estimateBytes(rows, bytesPerMin);
  if (!n) return '';
  return `${anyEstimated(rows) ? '~' : ''}${fmtBytes(n)}`;
}

const msg = (e: unknown) => (e instanceof Error ? e.message : String(e));
