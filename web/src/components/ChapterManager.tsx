/**
 * The chapter manager: what replaced "render ahead N hours".
 *
 * Rendering ahead by the clock was a guess. This is the same machinery addressed
 * by name - every chapter's state, multi-select, and three verbs.
 *
 * The two offline tiers are stated as two labelled rows, not as two badges that
 * look alike, because they are different in kind and one of them happens without
 * being asked: the book's *text* is taken whole on first open (cheap, and the
 * only reason the reader works with no network), and its *audio* is per chapter
 * and always explicit. Anything taken without asking needs a way to give back, so
 * the text row carries its own remove - and remembers the refusal.
 */
import {useMemo, useState} from 'react';
import {
  Check, CircleDashed, Clock, Download, FileAudio, HardDriveDownload, Loader2,
  PieChart, Trash2, Type, Waves,
} from 'lucide-react';
import {Button} from '@/components/ui/button';
import {Checkbox} from '@/components/ui/checkbox';
import {Input} from '@/components/ui/input';
import {Progress} from '@/components/ui/progress';
import {ScrollArea} from '@/components/ui/scroll-area';
import {Skeleton} from '@/components/ui/skeleton';
import {useChapterActions, useChapters} from '@/lib/api';
import {chapterState, type ChapterStateKey, type Job} from '@/lib/chapterstate';
import {downloadChapter, removeChapter, storageEstimate} from '@/lib/offline';
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

export function ChapterManager({open, onPick}: {open: boolean; onPick: (ci: number) => void}) {
  const n = useNarrator();
  const {data, isPending} = useChapters(open && !!n.book);
  const actions = useChapterActions();
  const [sel, setSel] = useState<Set<number>>(new Set());
  const [filter, setFilter] = useState('');
  const [jobs, setJobs] = useState<Record<number, Job>>({});
  const [err, setErr] = useState<string | null>(null);
  const [store, setStore] = useState<{usage: number; quota: number} | null>(null);

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

  const toggle = (i: number) => setSel((s) => {
    const next = new Set(s);
    next.has(i) ? next.delete(i) : next.add(i);
    return next;
  });

  const chosen = () => [...sel].sort((a, b) => a - b);

  async function download() {
    const key = n.book?.key;
    if (!key) return;
    setErr(null);
    for (const ci of chosen()) {
      if (n.offlineChapters.has(ci)) continue;
      try {
        const row = rows.find((r) => r.i === ci);
        if (!row?.m4a) {
          setJobs((j) => ({...j, [ci]: 'packing'}));
          await actions.build.mutateAsync([ci]);
          await waitFor(() => !!data?.chapters.find((r) => r.i === ci)?.m4a, 45 * 60_000);
        }
        setJobs((j) => ({...j, [ci]: 'saving'}));
        await downloadChapter(key, ci);
      } catch (e) {
        setErr(`chapter ${ci + 1}: ${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setJobs(({[ci]: _, ...rest}) => rest);
        await n.refreshOffline();
      }
    }
    setStore(await storageEstimate());
  }

  async function remove() {
    const key = n.book?.key;
    if (!key) return;
    for (const ci of chosen()) await removeChapter(key, ci);
    await n.refreshOffline();
    setStore(await storageEstimate());
  }

  const offlineBytes = rows
    .filter((r) => n.offlineChapters.has(r.i))
    .reduce((a, r) => a + (r.bytes ?? 0), 0);
  const shards = n.index?.shards ?? 0;
  const textDone = shards > 0 && n.textShards.size >= shards;
  const textPct = shards > 0 ? Math.round((n.textShards.size / shards) * 100) : 0;
  const loadingRows = isPending && !rows.length;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* the two tiers, named, before anything else */}
      <div data-testid="tiers" className="space-y-1.5 px-4 pb-2.5 pt-0.5">
        <div data-testid="tier-text" data-state={n.textOptOut ? 'off' : textDone ? 'saved' : 'partial'}
             className="rounded-md border border-border bg-card/40 px-2.5 py-2">
          <div className="flex items-center gap-2">
            <Type className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="text-[12px] text-foreground/80">Text</span>
            <span className="ml-auto flex items-center gap-1">
              {textDone && !n.textOptOut && (
                <Button data-testid="text-remove" size="sm" variant="ghost"
                        className="h-6 px-2 text-[11px] text-muted-foreground hover:text-destructive"
                        title="Delete the book's words from this device. The reader then needs the server for every chapter."
                        onClick={() => void n.dropText()}>
                  <Trash2 className="size-3" /> remove
                </Button>
              )}
              {(n.textOptOut || (!textDone && !n.textBusy)) && (
                <Button data-testid="text-save" size="sm" variant="ghost"
                        className="h-6 px-2 text-[11px] text-muted-foreground hover:text-foreground"
                        title="Keep the whole book's words on this device"
                        onClick={() => n.saveText()}>
                  <HardDriveDownload className="size-3" /> save
                </Button>
              )}
            </span>
          </div>
          <div data-testid="text-state" className="mt-0.5 pl-[22px] text-[11px] text-muted-foreground">
            {n.textOptOut
              ? 'not saved — chapters come from the server as you open them'
              : n.textBusy
                ? <span className="flex items-center gap-1.5">
                    <Loader2 className="size-3 animate-spin" />
                    saving for offline reading · {n.textProgress?.done ?? n.textShards.size} of{' '}
                    {n.textProgress?.total ?? shards} parts
                  </span>
                : textDone
                  ? <>saved for offline reading
                      {n.index?.text_bytes ? ` · ${fmtBytes(n.index.text_bytes)}` : ''}</>
                  : shards
                    ? `${n.textShards.size} of ${shards} parts saved`
                    : 'not saved yet'}
          </div>
          {n.textBusy && (
            <Progress data-testid="text-progress" className="mt-1.5 h-1 bg-white/[0.06]"
                      value={n.textProgress
                        ? (n.textProgress.done / Math.max(1, n.textProgress.total)) * 100
                        : textPct} />
          )}
        </div>

        <div data-testid="tier-audio" className="rounded-md border border-border bg-card/40 px-2.5 py-2">
          <div className="flex items-center gap-2">
            <FileAudio className="size-3.5 shrink-0 text-muted-foreground" />
            <span className="text-[12px] text-foreground/80">Audio</span>
            <span className="ml-auto text-[10px] uppercase tracking-wide text-muted-foreground">
              per chapter
            </span>
          </div>
          <div data-testid="audio-state" className="mt-0.5 pl-[22px] text-[11px] text-muted-foreground">
            {n.offlineChapters.size} of {rows.length} chapters downloaded
            {offlineBytes > 0 && ` · ${fmtBytes(offlineBytes)}`}
          </div>
        </div>
      </div>

      <Input
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        placeholder="filter chapters…"
        className="mx-4 mb-2 h-8 w-[calc(100%-2rem)] bg-card text-xs"
      />

      <ScrollArea className="min-h-0 flex-1">
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
            return (
              <div
                key={r.i}
                data-testid="chapter-row"
                data-ci={r.i}
                data-state={s.key}
                title={s.tip}
                onClick={() => onPick(r.i)}
                className={cn(
                  'relative flex cursor-pointer items-center gap-2 px-3 py-1.5 text-[13px]',
                  'text-muted-foreground transition-colors hover:bg-white/5 hover:text-foreground',
                  r.i === n.ci && 'bg-white/[0.06] text-foreground shadow-[inset_2px_0_0_var(--color-ring)]',
                )}
              >
                <Checkbox
                  data-testid="chapter-check"
                  checked={sel.has(r.i)}
                  onClick={(e) => { e.stopPropagation(); toggle(r.i); }}
                  onCheckedChange={() => undefined}
                  className="size-3.5"
                  aria-label={`select ${r.title}`}
                />
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

      <div className="border-t border-border px-3 py-2.5">
        <div className="flex flex-wrap items-center gap-2">
          <Button data-testid="act-render" size="sm" variant="outline" disabled={!sel.size}
                  onClick={() => void actions.render.mutateAsync(chosen())}
                  title={sel.size ? `Queue ${sel.size} chapter${sel.size > 1 ? 's' : ''} for rendering on the server`
                                  : 'Tick some chapters first'}>
            <Waves className="size-3.5" /> render
          </Button>
          <Button data-testid="act-download" size="sm" variant="outline" disabled={!sel.size}
                  onClick={() => void download()}
                  title={sel.size ? 'Pack if needed, then keep a copy on this device'
                                  : 'Tick some chapters first'}>
            <Download className="size-3.5" /> download
          </Button>
          <Button data-testid="act-remove" size="sm" variant="ghost" disabled={!sel.size}
                  onClick={() => void remove()}
                  title={sel.size ? 'Remove the downloaded copy (the server keeps its files)'
                                  : 'Tick some chapters first'}>
            <Trash2 className="size-3.5" /> remove
          </Button>
          <span data-testid="selection" className={cn('ml-auto text-[11px]',
                                                      err ? 'text-destructive' : 'text-muted-foreground')}>
            {err ?? (sel.size
              ? <button className="hover:text-foreground" onClick={() => setSel(new Set())}>
                  {sel.size} selected · clear
                </button>
              : 'nothing selected')}
          </span>
        </div>
        <div data-testid="verbs" className="mt-1.5 text-[10.5px] leading-relaxed text-muted-foreground/80">
          render = the server prepares the audio · download = keeps it on this device
        </div>
      </div>

      <div className="space-y-0.5 px-4 pb-3 pt-2 text-[11px] leading-relaxed text-muted-foreground">
        {store && (
          <div>browser storage <b className="font-normal text-foreground/70">{fmtBytes(store.usage)}</b>
            {store.quota ? ` of ${fmtBytes(store.quota)}` : ''}</div>
        )}
        {n.status?.done_min != null && n.status.book_min ? (
          <div>server has <b className="font-normal text-foreground/70">
            {(n.status.done_min / 60).toFixed(1)}h</b> of {(n.status.book_min / 60).toFixed(1)}h rendered</div>
        ) : null}
      </div>
    </div>
  );
}

async function waitFor(pred: () => boolean, ms: number) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    if (pred()) return;
    await new Promise((r) => setTimeout(r, 1500));
  }
  throw new Error('timed out waiting for the server to pack it');
}
