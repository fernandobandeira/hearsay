/**
 * The chapter manager: what replaced "render ahead N hours".
 *
 * Two things changed after the phone review, and both were about the list being
 * the point and everything around it being in the way.
 *
 * **The two tiers are one line.** They used to be two bordered cards with a
 * label, a sentence and a badge each - 133 px of a 852 px screen, restating per
 * book what every chapter row already says per chapter. They are now one line
 * of small print: what the text copy costs, its remove/save affordance, and how
 * many chapters are on the device. The caption explaining the verbs, the
 * "server has 2.5h of 331.3h rendered" line and the text-size row all went with
 * them; text size lives in the top bar's `T` now, and the pipeline figures live
 * in the diagnostics corner at the bottom of the Books level.
 *
 * **Download is one action and selection is a mode.** "Render" is gone from the
 * UI entirely - see lib/download.ts. Nobody wants a rendered chapter they
 * cannot listen to offline, so download climbs the whole ladder itself: queue
 * the render, wait for it, ask for the pack, wait for it, store the m4a. And
 * picking chapters no longer means hitting a 14 px checkbox: "download…" or
 * "remove…" starts a mode in which whole rows toggle on a tap, with "next 5",
 * "next 20" and "rest" for the case that is actually common, and a confirm bar
 * that says how many and roughly how big before anything happens. Outside a
 * mode a tap on a row does the obvious thing and opens that chapter.
 */
import {useEffect, useLayoutEffect, useMemo, useReducer, useRef, useState} from 'react';
import {
  Check, CircleDashed, Clock, Download, FileAudio, HardDriveDownload, Loader2,
  PieChart, Trash2, Type,
} from 'lucide-react';
import {Button} from '@/components/ui/button';
import {Input} from '@/components/ui/input';
import {Progress} from '@/components/ui/progress';
import {ScrollArea} from '@/components/ui/scroll-area';
import {Skeleton} from '@/components/ui/skeleton';
import {useQueryClient} from '@tanstack/react-query';
import {get, keys, useChapterActions, useChapters} from '@/lib/api';
import {chapterState, type ChapterStateKey, type Job} from '@/lib/chapterstate';
import {
  anyEstimated, estimateBytes, isAction, jobFor, needsRender, phaseFor,
} from '@/lib/download';
import {centeredScrollTop, scrollTargetIndex} from '@/lib/drawernav';
import {chosen, idle, rangeAfter, reduce} from '@/lib/selection';
import {cachedChapters, downloadChapter, removeChapter} from '@/lib/offline';
import {bytes as fmtBytes} from '@/lib/format';
import {cn} from '@/lib/utils';
import {useNarrator} from '@/state';
import type {ChapRow, ChaptersResult} from '@/lib/types';

/** One icon per state, so a glance down the list reads as a picture. */
const ICON: Record<ChapterStateKey, typeof Check> = {
  packing: Loader2, saving: Loader2, downloaded: HardDriveDownload, 'to-pack': Clock,
  ready: FileAudio, rendered: Check, queued: Clock, partial: PieChart, none: CircleDashed,
};
const TONE = {
  ok: 'text-ok', work: 'text-work', part: 'text-part',
  done: 'text-foreground/50', none: 'text-muted-foreground/80',
} as const;

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
  const {data, isPending} = useChapters(open && !!n.book);
  const actions = useChapterActions();
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
    });
  }, [data, n.chapters]);

  const hits = useMemo(
    () => rows.filter((r) => !filter || r.title.toLowerCase().includes(filter.toLowerCase())),
    [rows, filter]);

  /* Which rows the current mode may act on. Download skips what is already
     here; remove can only touch what is. Threaded into every selection event so
     a row that finished downloading mid-selection cannot stay picked. */
  const eligible = useMemo(() => {
    if (!sel.verb) return [];
    return hits
      .filter((r) => sel.verb === 'remove'
        ? n.offlineChapters.has(r.i)
        : !n.offlineChapters.has(r.i))
      .map((r) => r.i);
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
      const r = await get<ChaptersResult>('/api/chapters');
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
      const toRender = needsRender(fresh.filter((r) => cis.includes(r.i)), held);
      if (toRender.length) {
        for (const ci of toRender) job(ci, 'queued');
        await actions.render.mutateAsync(toRender).catch((e: unknown) => {
          throw new Error(`could not queue the render: ${msg(e)}`);
        });
      }

      for (const ci of cis) {
        if (held.has(ci)) { job(ci, null); continue; }
        const t0 = Date.now();
        /* When each ask was last sent. Neither endpoint says what it refused -
           a build for a half-rendered chapter is dropped in silence, and the
           up-front render queue call can be swallowed the same way (RUST-NOTES
           items 7 and 9) - so an ask that has not moved the row within twenty
           seconds is simply repeated. Both are idempotent, so a repeat costs a
           request and nothing else, and the alternative is a row that sits on
           one rung for forty-five minutes waiting for a call nobody made. */
        const asked = {render: Date.now(), pack: 0};
        try {
          for (;;) {
            const phase = phaseFor(fresh.find((r) => r.i === ci), false);
            job(ci, jobFor(phase));
            if (phase === 'stored') break;
            if (phase === 'store') {
              await downloadChapter(key, ci);
              break;
            }
            if (phase === 'queue-render' && Date.now() - asked.render > 20_000) {
              asked.render = Date.now();
              await actions.render.mutateAsync([ci]);
            }
            if (phase === 'request-pack' && Date.now() - asked.pack > 20_000) {
              asked.pack = Date.now();
              await actions.build.mutateAsync([ci]);
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
      setRunning(false);
    }
  }

  async function runRemove(cis: number[]) {
    const key = n.book?.key;
    if (!key) return;
    setErr(null);
    setRunning(true);
    try {
      for (const ci of cis) await removeChapter(key, ci);
      await n.refreshOffline();
    } finally {
      setRunning(false);
    }
  }

  function confirm() {
    const verb = sel.verb;
    const cis = picks;
    dispatch({t: 'cancel'});
    if (!cis.length || !verb) return;
    void (verb === 'download' ? runDownload(cis) : runRemove(cis));
  }

  // ------------------------------------------------------------------ the line
  const offlineBytes = rows
    .filter((r) => n.offlineChapters.has(r.i))
    .reduce((a, r) => a + (r.bytes ?? 0), 0);
  const shards = n.index?.shards ?? 0;
  const textDone = shards > 0 && n.textShards.size >= shards;
  const loadingRows = isPending && !rows.length;
  const busy = running || Object.keys(jobs).length > 0;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* The two tiers, as one line of small print. */}
      <div data-testid="tiers"
           className="flex items-center gap-1.5 px-4 pb-1.5 text-[11px] text-muted-foreground">
        <Type className="size-3 shrink-0" />
        <span data-testid="text-state" className="min-w-0 truncate">
          {n.textOptOut
            ? 'text not saved'
            : n.textBusy
              ? `text ${n.textProgress?.done ?? n.textShards.size}/${n.textProgress?.total ?? shards}`
              : textDone
                ? `text ${n.index?.text_bytes ? fmtBytes(n.index.text_bytes) : 'saved'}`
                : shards ? `text ${n.textShards.size}/${shards}` : 'text not saved'}
        </span>
        {textDone && !n.textOptOut ? (
          <button data-testid="text-remove" onClick={() => void n.dropText()}
                  title="Delete the book's words from this device. The reader then needs the server for every chapter."
                  className="shrink-0 rounded p-1 text-muted-foreground/70 transition-colors hover:text-destructive">
            <Trash2 className="size-3" />
          </button>
        ) : (n.textOptOut || (!textDone && !n.textBusy)) ? (
          <button data-testid="text-save" onClick={() => n.saveText()}
                  title="Keep the whole book's words on this device"
                  className="shrink-0 rounded p-1 text-muted-foreground/70 transition-colors hover:text-foreground">
            <HardDriveDownload className="size-3" />
          </button>
        ) : null}
        <span data-testid="audio-state"
              className="ml-auto flex shrink-0 items-center gap-1 tabular-nums">
          <FileAudio className="size-3" />
          {n.offlineChapters.size}/{rows.length}
          {offlineBytes > 0 && ` · ${fmtBytes(offlineBytes)}`}
        </span>
      </div>
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
                  ? can ? `tap to ${sel.verb === 'remove' ? 'remove' : 'download'} this chapter`
                        : sel.verb === 'remove' ? 'not on this device' : 'already on this device'
                  : s.tip}
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
                <span data-testid="chapter-state"
                      className={cn('flex shrink-0 items-center gap-1 text-[10px] tabular-nums tracking-wide',
                                    TONE[s.tone])}>
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
              tap rows to {sel.verb === 'remove' ? 'remove' : 'download'}
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
                    {sizeOf(pickedRows) && ` · ${sizeOf(pickedRows)}`}</>
                : 'none picked'}
            </span>
            <Button data-testid="sel-confirm" size="sm"
                    variant={sel.verb === 'remove' ? 'outline' : 'default'}
                    disabled={!picks.length} onClick={confirm}>
              {sel.verb === 'remove' ? <Trash2 className="size-3.5" /> : <Download className="size-3.5" />}
              {sel.verb === 'remove' ? 'remove' : 'download'}
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
          <Button data-testid="start-remove" size="sm" variant="ghost"
                  disabled={busy || !n.offlineChapters.size}
                  onClick={() => dispatch({t: 'start', verb: 'remove'})}
                  title="Give back the downloaded copies (the server keeps its files)">
            <Trash2 className="size-3.5" /> remove…
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
function sizeOf(rows: ChapRow[]): string {
  if (!rows.length) return '';
  const n = estimateBytes(rows);
  if (!n) return '';
  return `${anyEstimated(rows) ? '~' : ''}${fmtBytes(n)}`;
}

const msg = (e: unknown) => (e instanceof Error ? e.message : String(e));
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
