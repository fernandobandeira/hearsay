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
 * cannot listen to offline, so download climbs the whole ladder itself: queue
 * the render, wait for it, ask for the pack, wait for it, store the m4a. And
 * picking chapters no longer means hitting a 14 px checkbox: "download…"
 * starts a mode in which whole rows toggle on a tap, with "next 5",
 * "next 20" and "rest" for the case that is actually common, and a confirm bar
 * that says how many and roughly how big before anything happens. Outside a
 * mode a tap on a row does the obvious thing and opens that chapter.
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
import {useQueryClient} from '@tanstack/react-query';
import {fetchChapters, keys, useChapterActions, useChapters} from '@/lib/api';
import {chapterState, textMark, type ChapterStateKey, type Job, type TextMark} from '@/lib/chapterstate';
import {chaptersWithText} from '@/lib/chaptertext';
import {
  anyEstimated, buildVerdict, estimateBytes, isAction, jobFor, needsRender, phaseFor,
  renderAccepted, rowSignature, shouldReask,
} from '@/lib/download';
import {centeredScrollTop, scrollTargetIndex} from '@/lib/drawernav';
import {chosen, idle, rangeAfter, reduce} from '@/lib/selection';
import {cachedChapters, downloadChapter} from '@/lib/offline';
import * as db from '@/lib/db';
import {bytes as fmtBytes} from '@/lib/format';
import {cn} from '@/lib/utils';
import {useNarrator} from '@/state';
import type {ChapRow, RenderResult} from '@/lib/types';

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

/** How long a single chapter may sit on one rung before we give up on it. */
const RUNG_TIMEOUT_MS = 45 * 60_000;

export function ChapterManager({open, active, onPick}: {
  /** the drawer is up: what gates the chapters query */
  open: boolean;
  /** this level of the stack is the one on screen: what triggers the centring */
  active: boolean;
  onPick: (ci: number) => void;
}) {
  const n = useNarrator();
  const qc = useQueryClient();
  /* Every call here names the book. The server refuses (409) anything aimed at a
     book it is not holding, which is what closed the one race this drawer could
     not close itself: the poll says chapter 74 needs rendering, the server picks
     up a new epub, the tap lands on the other novel. */
  const book = n.book?.key ?? null;
  const {data, isPending} = useChapters(open && !!n.book, book);
  const actions = useChapterActions(book);
  const [sel, dispatch] = useReducer(reduce, idle);
  const [filter, setFilter] = useState('');
  const [jobs, setJobs] = useState<Record<number, Job>>({});
  const [err, setErr] = useState<string | null>(null);
  const [running, setRunning] = useState(false);

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
    return hits.filter((r) => !n.offlineChapters.has(r.i)).map((r) => r.i);
  }, [hits, sel.verb, n.offlineChapters]);

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

  // ------------------------------------------------------------------- the run
  /**
   * Climb lib/download.ts's ladder for every picked chapter.
   *
   * The whole selection's renders are queued in one call first, so the server's
   * worker is never idle while this device is busy storing an earlier chapter;
   * then each chapter is walked rung by rung.
   *
   * It does its own polling rather than reading the rows this component
   * rendered with, and that is not a detail: the rungs are minutes apart, the
   * drawer is the thing he closes to go back to reading, and closing it
   * unmounts this component and switches off `useChapters`. The old version
   * closed over `data` and waited on a snapshot that could never change, so it
   * could only ever time out. Writing each poll back into the query cache keeps
   * the list in front of him live for free, and the run survives the drawer.
   */
  async function runDownload(cis: number[]) {
    const key = n.book?.key;
    if (!key) return;
    setErr(null);
    setRunning(true);

    const poll = async (): Promise<ChapRow[]> => {
      const r = await fetchChapters(key);
      qc.setQueryData(keys.chapters, r);
      return r.chapters;
    };
    const job = (ci: number, next: Job | null) => setJobs((j) => {
      if (!next) { const {[ci]: _drop, ...rest} = j; return rest; }
      return j[ci] === next ? j : {...j, [ci]: next};
    });

    try {
      let fresh = await poll().catch(() => rows);
      const held = await cachedChapters(key);
      /* What was asked for, written down before anything is asked of the server.
         This ladder is a foreground process and the phone will suspend it: the
         record in IndexedDB is what lets the next foreground finish the job
         (lib/reconcile.ts), and it has to exist before the first await or a
         selection confirmed as the screen locks is a selection nobody remembers. */
      await db.addDownload({
        key, path: n.book?.path, chapters: cis.filter((ci) => !held.has(ci)), ts: Date.now(),
      }).catch(() => {});

      const toRender = needsRender(fresh.filter((r) => cis.includes(r.i)), held);
      /* The whole selection's renders go up in one call, and the queue that
         comes back is the receipt each chapter's loop starts from. `pack: true`
         is the other half: it makes the call a standing order the server
         finishes on its own, so the rungs below are a *fast path* for an app
         that stays open rather than the only way a chapter ever gets packed. */
      let queued: RenderResult | null = null;
      if (toRender.length) {
        for (const ci of toRender) job(ci, 'queued');
        queued = await actions.render.mutateAsync({chapters: toRender, pack: true})
          .catch((e: unknown) => {
            throw new Error(`could not queue the render: ${msg(e)}`);
          });
      }

      for (const ci of cis) {
        if (held.has(ci)) { job(ci, null); continue; }
        const t0 = Date.now();
        /* Whether each ask has been *acknowledged*, and when this chapter's row
           last changed.

           Both endpoints now say what they did with each chapter - the render
           queue comes back in the render response, and build reports per-chapter
           refusals with a reason. So an acknowledged ask is simply waited on,
           and the repeat is kept for the case it was meant for: a row that has
           not moved at all for ninety seconds, which is what a lost call or a
           restarted server looks like from here. (This used to re-ask every
           twenty seconds, blind, because the responses said nothing.) */
        const ack = {render: renderAccepted(queued, ci), pack: false};
        let sig = rowSignature(fresh.find((r) => r.i === ci));
        let changedAt = Date.now();
        try {
          for (;;) {
            const row = fresh.find((r) => r.i === ci);
            const now = rowSignature(row);
            if (now !== sig) { sig = now; changedAt = Date.now(); }
            const still = {sinceChangeMs: Date.now() - changedAt};
            const phase = phaseFor(row, false);
            job(ci, jobFor(phase));
            if (phase === 'stored') break;
            if (phase === 'store') {
              await downloadChapter(key, ci);
              break;
            }
            if (phase === 'queue-render' && shouldReask({acked: ack.render, ...still})) {
              ack.render = renderAccepted(
                await actions.render.mutateAsync({chapters: [ci], pack: true}), ci);
              changedAt = Date.now();
            }
            if (phase === 'request-pack' && shouldReask({acked: ack.pack, ...still})) {
              const verdict = buildVerdict(await actions.build.mutateAsync([ci]), ci);
              if (verdict.t === 'impossible')
                throw new Error(`the server will not pack it (${verdict.reason})`);
              // "taken" and "rendering" are both an answer: stop asking.
              ack.pack = verdict.t !== 'unknown';
              changedAt = Date.now();
            }
            if (Date.now() - t0 > RUNG_TIMEOUT_MS)
              throw new Error('the server never finished it');
            // A rung where the client just acted is re-read straight away; a
            // rung where the server is working gets the drawer's own cadence.
            await sleep(isAction(phase) ? 300 : 1500);
            fresh = await poll().catch(() => fresh);
          }
        } catch (e) {
          setErr(`chapter ${ci + 1}: ${msg(e)}`);
        } finally {
          job(ci, null);
          await n.refreshOffline();
        }
      }
    } catch (e) {
      setErr(msg(e));
    } finally {
      await n.refreshOffline();
      /* What this run stored comes off the pending record, and anything it did
         not reach stays on it. Through the sweep rather than a second bit of
         bookkeeping here, so there is exactly one answer to "what is still
         wanted" and it is computed from Cache Storage either way. */
      await n.sweepDownloads();
      setRunning(false);
    }
  }

  function confirm() {
    const cis = picks;
    dispatch({t: 'cancel'});
    if (!cis.length || !sel.verb) return;
    void runDownload(cis);
  }

  const loadingRows = isPending && !rows.length;
  /* What a minute of audio weighs, from the server rather than from a constant
     in this file: CHAPTER_BITRATE is the box's to set, and an estimate computed
     at the wrong rate is wrong by exactly that ratio. */
  const perMin = n.status?.bitrate_bytes_per_min;
  const busy = running || Object.keys(jobs).length > 0;

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
            const s = chapterState(r, n.offlineChapters.has(r.i), fmtBytes, jobs[r.i]);
            const Icon = ICON[s.key];
            const mark = textMark(wordsHere ? wordsHere.has(r.i) : null, connected);
            const MarkIcon = mark ? TEXT_ICON[mark.icon] : null;
            const picked = sel.picked.has(r.i);
            const can = !sel.verb || eligible.includes(r.i);
            return (
              <div
                key={r.i}
                data-testid="chapter-row"
                data-ci={r.i}
                data-state={s.key}
                data-picked={picked ? '1' : undefined}
                title={sel.verb
                  ? can ? 'tap to download this chapter' : 'already on this device'
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
          <Button data-testid="start-download" size="sm" variant="outline" disabled={busy}
                  onClick={() => dispatch({t: 'start', verb: 'download'})}
                  title="Pick chapters to keep on this device. The server renders whatever needs it first.">
            {busy ? <Loader2 className="size-3.5 animate-spin" /> : <Download className="size-3.5" />}
            download…
          </Button>
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
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
