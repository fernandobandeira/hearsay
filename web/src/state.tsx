/**
 * The reader's state, in one place.
 *
 * Server data belongs to TanStack Query; this holds what Query cannot: which book
 * and chapter are open, the chunk the eyes or the playhead are on, and the player
 * instance itself. Everything here is deliberately boring - the interesting logic
 * lives in lib/ where it can be tested without a DOM.
 *
 * Two rules about opening a book are worth stating here, because both were bugs:
 *
 *   open where he stopped   `/api/load` returns the vault's saved position and it
 *                           is the *first* thing consulted, not a fallback. The
 *                           rule that reconciles it with an undelivered local one
 *                           lives in lib/resume.ts.
 *   never wait on the copy  the whole-book text is 17 MB on the big novel. First
 *                           paint asks for one chapter and shows it; the bundle
 *                           downloads behind, with a visible count.
 */
import {
  createContext, useCallback, useContext, useEffect, useMemo, useRef, useState,
  type ReactNode,
} from 'react';
import {onlineManager, useQueryClient} from '@tanstack/react-query';
import {
  awaitedRetry, bookIndexUrl, chapterManifestUrl, chapterTextUrl, fetchChapters, keys, get,
  loadBook, openChapter as tellOpen, reportPlayhead, tellPause, tellResume, textShardUrl,
  useBookIndex, useStatus,
} from './lib/api';
import {loadChapterText, shardOf, type TextSources} from './lib/chaptertext';
import {
  arbitrate, connectLive, lostSession, type HelloEvent, type LiveState, type PositionEvent,
  type RenderEvent,
} from './lib/live';
import {isSane, type Manifest} from './lib/manifest';
import {openSequence} from './lib/opening';
import {Player, type PlayMode} from './lib/player';
import {
  bookKey, cachedChapters, cachedShards, downloadChapter, downloadText, removeBook, removeChapter,
  requestPersistence,
} from './lib/offline';
import {reconcile, SWEEP_EVERY_MS, type PendingDownload} from './lib/reconcile';
import {buildChapters, cancelChapters, renderChapters} from './lib/api';
import {chaptersToTrim, furthestReached, KEEP_BEHIND} from './lib/autotrim';
import {clampResume, resolveResume, type Resume} from './lib/resume';
import * as db from './lib/db';
import type {
  BookFile, BookIndex, ChapMeta, ChapterText, LoadResult, TextShard,
} from './lib/types';

export type ConnState = 'online' | 'reconnecting' | 'offline';

export interface OpenBook {
  path: string;
  name: string;
  key: string;
  title: string;
}

export interface TextProgress {done: number; total: number}

interface Ctx {
  book: OpenBook | null;
  chapters: ChapMeta[];
  index: BookIndex | undefined;
  ci: number;
  chunks: string[];
  paras: number[] | null;
  chapterTitle: string;
  idx: number;
  mode: PlayMode;
  playing: boolean;
  waiting: boolean;
  message: string | null;
  conn: ConnState;
  offlineChapters: Set<number>;
  /**
   * The download queue as it stands on this device, for the open book: every
   * chapter asked for and not yet stored, whoever asked and however long ago.
   *
   * Read straight off the durable record in IndexedDB rather than from whatever
   * run placed it, which is the whole point - a selection confirmed last night
   * has no run any more, and the rows still have to say it is coming.
   */
  queuedChapters: Set<number>;
  /** The chapters being copied onto the device right now. */
  savingChapters: Set<number>;
  textShards: Set<number>;
  textBusy: boolean;
  textProgress: TextProgress | null;
  textOptOut: boolean;
  /**
   * Shards the whole-book download could not save, after every retry it is
   * allowed. Empty while it is still working, and empty for a run that was
   * abandoned - see lib/offline.ts. It is what lets the drawer say a book is
   * only partly here rather than looking like it is still counting.
   */
  textMissing: number[];
  /** true while the chapter's words are on their way: the page shows a skeleton */
  chapterLoading: boolean;
  /** true while the book itself is being opened */
  bookLoading: boolean;
  resumedAt: Resume | null;
  queued: {notes: number; positions: number; stalled: number};
  fontScale: number;
  status: ReturnType<typeof useStatus>['data'];
  /**
   * Where another device moved the reading position to, while this one is
   * playing. Null unless there is something to offer - see lib/live.ts's
   * arbitration: a sync that interrupts someone mid-sentence is not a feature,
   * so it waits behind a tap.
   */
  moved: {chapter: number; chunk: number} | null;
  /** Take the offer. */
  follow: () => void;
  /** Turn it down; it comes back if the other device moves again. */
  dismissMoved: () => void;

  openBook: (b: BookFile & {key?: string}) => Promise<void>;
  openChapter: (ci: number, chunk?: number, opts?: {cacheOnly?: boolean}) => Promise<boolean>;
  goChapter: (d: number) => void;
  setIdx: (i: number, report?: boolean) => void;
  toggle: () => void;
  nudge: (s: number) => void;
  setFontScale: (n: number) => void;
  refreshOffline: () => Promise<void>;
  /** Put chapters in the download queue and start working on it straight away. */
  queueDownload: (cis: number[]) => Promise<void>;
  /** Take chapters back out of it. */
  unqueueDownload: (cis: number[]) => Promise<void>;
  /**
   * Catch up on downloads the server finished while this device was away - see
   * lib/reconcile.ts. Runs itself on every way back into the app; exposed so a
   * finished download run can settle its own pending record through the same
   * code path rather than a second one.
   */
  sweepDownloads: () => Promise<void>;
  saveText: () => void;
  /** Give a whole book back: its words and every chapter downloaded for it. */
  dropBook: (key: string) => Promise<void>;
  flush: (manual?: boolean) => Promise<void>;
  queueNote: (blob: Blob) => Promise<void>;
  player: Player | null;
}

const NarratorContext = createContext<Ctx | null>(null);

export function useNarrator(): Ctx {
  const c = useContext(NarratorContext);
  if (!c) throw new Error('useNarrator outside the provider');
  return c;
}

const LIB_KEY = 'narrator.lib';
type LibEntry = OpenBook & {chapters: ChapMeta[]; shards: number};
const readLib = (): Record<string, LibEntry> => {
  try { return JSON.parse(localStorage.getItem(LIB_KEY) ?? '{}'); } catch { return {}; }
};
const writeLib = (l: Record<string, LibEntry>) => {
  try { localStorage.setItem(LIB_KEY, JSON.stringify(l)); } catch { /* private mode */ }
};

/* How far into each book this device has ever got, by cache key. It anchors the
   auto-trim (lib/autotrim.ts) and nothing else, which is why it lives in
   localStorage rather than in the position record the vault owns: losing it costs
   one chapter's worth of housekeeping, not a reading position. */
const FURTHEST_KEY = 'narrator.furthest:';
const readFurthest = (key: string): number => {
  try { return Number(localStorage.getItem(FURTHEST_KEY + key)) || 0; } catch { return 0; }
};
const writeFurthest = (key: string, ci: number) => {
  try { localStorage.setItem(FURTHEST_KEY + key, String(ci)); } catch { /* private mode */ }
};

/* When this device last touched each downloaded chapter, by cache key.
   "Touched" is read *or* stored, which is the distinction that matters: a chapter
   downloaded ahead and not reached yet has been touched, so a jump forward and
   back does not cost the chapters in between (lib/autotrim.ts). Pruned to what
   is actually downloaded on every trim, so it stays the size of the wake rather
   than the size of the book, and it lives in localStorage for the same reason
   the anchor does - losing it costs one round of housekeeping, not a position. */
const TOUCH_KEY = 'narrator.touched:';
const readTouched = (key: string): Map<number, number> => {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(TOUCH_KEY + key) ?? '{}');
    if (!raw || typeof raw !== 'object') return new Map();
    return new Map(Object.entries(raw as Record<string, number>)
      .map(([ci, at]) => [Number(ci), Number(at)] as const)
      .filter(([ci, at]) => Number.isInteger(ci) && Number.isFinite(at)));
  } catch { return new Map(); }
};
const writeTouched = (key: string, m: ReadonlyMap<number, number>) => {
  try {
    localStorage.setItem(TOUCH_KEY + key,
                         JSON.stringify(Object.fromEntries([...m].map(([ci, at]) => [ci, at]))));
  } catch { /* private mode */ }
};
const touchChapter = (key: string, ci: number) => {
  const m = readTouched(key);
  m.set(ci, Date.now());
  writeTouched(key, m);
};

/* Books whose offline text he removed on purpose. Removing it and having it
   silently come back on the next open would be the same surprise twice. */
const OPTOUT_KEY = 'narrator.notext';
const readOptOut = (): string[] => {
  try { return JSON.parse(localStorage.getItem(OPTOUT_KEY) ?? '[]'); } catch { return []; }
};
const writeOptOut = (l: string[]) => {
  try { localStorage.setItem(OPTOUT_KEY, JSON.stringify(l)); } catch { /* private mode */ }
};

/* The shortest gap between two attempts to put a lost server session back. A
   `hello` arrives on every reconnect, and a tunnel that flaps would otherwise be
   a `/api/load` per flap. */
const HEAL_MIN_MS = 10_000;

/**
 * How often the outbox is drained while the app is open, on top of every edge
 * that already drains it.
 *
 * A minute rather than the queue's twenty seconds: a memo that is not going
 * anywhere is not going anywhere faster for being asked more often, and each
 * pass that finds a stalled memo re-uploads nothing - it asks the server whether
 * the note was filed, which is a few hundred bytes.
 */
const OUTBOX_EVERY_MS = 60_000;

export function NarratorProvider({children}: {children: ReactNode}) {
  const qc = useQueryClient();
  const [book, setBook] = useState<OpenBook | null>(null);
  const [chapters, setChapters] = useState<ChapMeta[]>([]);
  const [ci, setCi] = useState(0);
  const [chunks, setChunks] = useState<string[]>([]);
  const [paras, setParas] = useState<number[] | null>(null);
  const [chapterTitle, setChapterTitle] = useState('');
  const [idx, setIdxState] = useState(0);
  const [mode, setMode] = useState<PlayMode>('none');
  const [playing, setPlaying] = useState(false);
  const [waiting, setWaiting] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [offlineChapters, setOfflineChapters] = useState<Set<number>>(new Set());
  const [pendingDownloads, setPendingDownloads] = useState<PendingDownload[]>([]);
  const [saving, setSaving] = useState<Set<string>>(new Set());
  const [textShards, setTextShards] = useState<Set<number>>(new Set());
  const [textBusy, setTextBusy] = useState(false);
  const [textProgress, setTextProgress] = useState<TextProgress | null>(null);
  const [textMissing, setTextMissing] = useState<number[]>([]);
  /* Bumped when the connection comes back with parts of the book still missing.
     The download effect keys on it, because nothing else would ever restart it:
     its other deps are the book, its index and the opt-out, none of which move
     when a plane lands. */
  const [textRetry, setTextRetry] = useState(0);
  const [optOut, setOptOut] = useState<string[]>(() => readOptOut());
  const [chapterLoading, setChapterLoading] = useState(false);
  const [bookLoading, setBookLoading] = useState(false);
  const [resumedAt, setResumedAt] = useState<Resume | null>(null);
  const [queued, setQueued] = useState({notes: 0, positions: 0, stalled: 0});
  const [moved, setMoved] = useState<{chapter: number; chunk: number} | null>(null);
  const [live, setLive] = useState<LiveState>('connecting');
  const [fontScale, setFontScaleState] = useState(
    () => Math.min(1.8, Math.max(0.7, Number(localStorage.getItem('narrator.font')) || 1)));

  const status = useStatus();
  const index = useBookIndex(book?.key ?? null).data;
  const playerRef = useRef<Player | null>(null);
  const ciRef = useRef(0);
  const idxRef = useRef(0);
  const bookRef = useRef<OpenBook | null>(null);
  const indexRef = useRef<BookIndex | undefined>(undefined);
  const textMissingRef = useRef<number[]>([]);
  textMissingRef.current = textMissing;
  /* Which open owns `chapterLoading`. One sequence for the life of the provider;
     the rule, and the stuck skeleton it exists to prevent, is in lib/opening.ts. */
  const openSeqRef = useRef<ReturnType<typeof openSequence> | null>(null);
  if (!openSeqRef.current) openSeqRef.current = openSequence(setChapterLoading);
  const openSeq = openSeqRef.current;
  ciRef.current = ci;
  idxRef.current = idx;
  bookRef.current = book;
  indexRef.current = index;

  const textOptOut = !!book && optOut.includes(book.key);

  /* The queue, narrowed to the book on screen. Chapters already on the device
     are dropped here rather than in the record: the record is the *order* and it
     is settled against Cache Storage by the sweep, so this is only ever asking
     "what is still coming". */
  const queuedChapters = useMemo(() => {
    const mine = pendingDownloads.find((p) => p.key === book?.key);
    return new Set((mine?.chapters ?? []).filter((ci) => !offlineChapters.has(ci)));
  }, [pendingDownloads, book?.key, offlineChapters]);
  const savingChapters = useMemo(() => {
    const out = new Set<number>();
    for (const k of saving) {
      const at = k.lastIndexOf('/');
      if (k.slice(0, at) === book?.key) out.add(Number(k.slice(at + 1)));
    }
    return out;
  }, [saving, book?.key]);

  /* The connection indicator, derived rather than tracked - and now derived from
     the live stream first, because the stream is the honest signal: it is a
     connection that is actually open, with the server's own heartbeat proving it
     from the other end. The heartbeat query is the fallback (a browser with no
     EventSource, a stream the proxy will not pass) and the tiebreaker while the
     stream is still opening. "reconnecting" is the middle state: something is
     being retried, nothing is wrong yet, so the UI says so quietly. */
  const conn: ConnState =
    !onlineManager.isOnline() || status.isError || status.failureCount >= 2
      ? 'offline'
      // Either signal being healthy is enough to be connected: the stream proves
      // it from the server's end, and the heartbeat covers the case where the
      // stream itself cannot be established (an old browser, a proxy that will
      // not pass text/event-stream). Only "neither is working yet" is the middle
      // state, and it is the only one that says reconnecting.
      : live === 'live' || (status.isSuccess && !status.failureCount)
        ? 'online'
        : 'reconnecting';

  // ---------------------------------------------------------------- the player
  const goChapterRef = useRef<(d: number) => void>(() => {});
  if (!playerRef.current && typeof window !== 'undefined') {
    playerRef.current = new Player({
      onChunk: (i) => { setIdxState(i); void report(i); },
      onMode: setMode,
      onPlaying: setPlaying,
      onWaiting: setWaiting,
      onMessage: setMessage,
      onChapterEnd: () => goChapterRef.current(1),
    });
  }
  const player = playerRef.current;

  // ------------------------------------------------------- positions, outbox
  /* The books this device has read past what it managed to tell the server.
     Mirrors the position queue in IndexedDB, in a ref because three things that
     are not renders consult it: the playhead report, the reconnect flush, and
     the live stream's arbitration. */
  const undeliveredRef = useRef<Set<string>>(new Set());

  const refreshQueued = useCallback(async () => {
    const [notes, positions] = await Promise.all([db.allMemos(), db.allPositions()]);
    undeliveredRef.current = new Set(positions.map((p) => p.book));
    setQueued({
      notes: notes.length,
      positions: positions.length,
      stalled: notes.filter((n) => (n.tries ?? 0) >= 5).length,
    });
  }, []);

  /* Report the playhead, or queue it.
     The reader used to guess whether the report would land - "does /api/status
     say the server holds my book?" - because a report aimed at the wrong book
     moved that book's render frontier. It does not have to guess any more: the
     call names its book and the server answers 409 if it holds another, which is
     both safer than the guess and one fewer thing to be stale. A 409 is just
     another reason to use the outbox, which is where a position belongs when the
     server cannot take it. */
  const report = useCallback(async (i: number) => {
    const b = bookRef.current;
    if (!b) return;
    rememberDevicePos(b.path, ciRef.current, i);
    /* The first report after reading offline cannot be a playhead report.
       `/api/playhead` carries only the chunk, so the server would file it under
       whatever chapter its session still holds - the one this device left when
       the network went - and then broadcast that as a position, which is the
       reader watching itself get dragged back three chapters. `/api/open` names
       the chapter too, which is the whole of what is out of date. */
    const healing = undeliveredRef.current.has(b.name);
    try {
      if (healing) await tellOpen(b.key, ciRef.current, i);
      else await reportPlayhead(b.key, i);
      if (healing) {
        undeliveredRef.current.delete(b.name);
        // Superseded by construction: same device, same book, newer position.
        await db.deletePosition(b.name).catch(() => {});
        void refreshQueued();
      }
      return;
    } catch { /* refused or unreachable: it goes in the queue */ }
    undeliveredRef.current.add(b.name);
    await db.putPosition({
      book: b.name, chapter: ciRef.current, chunk: i, ts: Date.now(),
      chapter_title: chapterTitle, chunks_total: chunks.length, chapters_total: chapters.length,
    });
    void refreshQueued();
  }, [chapterTitle, chunks.length, chapters.length, refreshQueued]);


  /**
   * Tell the server where this device actually got to, chapter included.
   *
   * The other half of the offline queue, and the half that was missing. A
   * position that could not be delivered is kept in IndexedDB and pushed to
   * `/api/position` on reconnect - which fixes the *vault record* and nothing
   * else. The session is untouched: its chapter is still the one this device
   * left when the network went, and the next thing that saves from it (a
   * `/api/playhead` a chunk later, a `/api/pause`, the 15 s throttle expiring)
   * overwrites the record that was just healed and broadcasts the old chapter as
   * a `position` event. This device then follows it, because a position from
   * somewhere else is exactly what that event means.
   *
   * So: `/api/open`, which is the only call that carries a chapter, before
   * anything else is said. It also puts the render frontier where the reader is,
   * which for the same reason had been left on a chapter nobody is reading.
   */
  const healSession = useCallback(async () => {
    const b = bookRef.current;
    if (!b || !undeliveredRef.current.has(b.name)) return;
    try {
      await tellOpen(b.key, ciRef.current, idxRef.current);
    } catch { return; }        // still unreachable, or another book: it keeps
    undeliveredRef.current.delete(b.name);
    await db.deletePosition(b.name).catch(() => {});
  }, []);

  const flush = useCallback(async (manual = false) => {
    const {flushOutbox, flushPositions} = await import('./lib/flush');
    // Before the vault post, not after: /api/open writes the record itself, and
    // a stale session left running would undo whatever /api/position wrote.
    await healSession();
    await flushPositions();
    await flushOutbox({online: onlineManager.isOnline(), manual});
    await refreshQueued();
  }, [refreshQueued, healSession]);

  const queueNote = useCallback(async (blob: Blob) => {
    const b = bookRef.current;
    await db.putMemo({blob, mime: blob.type, book: b?.name ?? null,
                      chapter: ciRef.current, chunk: idx, ts: Date.now(), tries: 0});
    await refreshQueued();
    void flush();
  }, [idx, flush, refreshQueued]);

  useEffect(() => { void refreshQueued(); }, [refreshQueued]);

  /* Every way back from a gap drains the queues - and there are two kinds of gap.
     The browser fires online/offline for a wifi-to-LTE handover, but a tunnel that
     dies quietly never does: navigator.onLine stays true while nothing answers.
     So the heartbeat recovering is the other trigger, and the important one. */
  const wasDown = useRef(false);
  useEffect(() => {
    if (conn !== 'online') { wasDown.current = true; return; }
    if (!wasDown.current) return;
    wasDown.current = false;
    void flush();
    /* And the other thing a gap leaves behind: parts of the book that could not
       be saved while it was down. Nothing else would ever ask for them again -
       the download runs off the book and its index, and neither moves when the
       network comes back - so a book opened on a plane would stay half here
       until it was closed and opened again. Only when something is actually
       missing, so a healthy reconnect is not a download. */
    if (textMissingRef.current.length) setTextRetry((n) => n + 1);
  }, [conn, flush]);

  useEffect(() => {
    const wake = () => void flush();
    const un = onlineManager.subscribe((online) => { if (online) wake(); });
    /* And a floor under all of it, for the same reason the download queue has
       one. Every other trigger here is an *edge* - the network returned, the app
       came back, the heartbeat recovered - and a memo can outlive all of them
       without one firing: on this box a one-minute recording is half an hour of
       whisper, so the POST that carries it routinely dies with the screen and
       the app is simply open, in the foreground, when the note finally lands.
       Without a tick, the only thing that asks "did it get filed?" is the user
       switching apps and coming back. `nextAction` still decides whether a memo
       is uploaded again; a stalled one costs a few hundred bytes to ask about. */
    const tick = setInterval(wake, OUTBOX_EVERY_MS);
    // Coming back to the app re-asks the server immediately rather than waiting
    // for the next tick of a poll that may have been suspended while hidden.
    const vis = () => {
      if (document.visibilityState !== 'visible') return;
      void qc.invalidateQueries({queryKey: keys.status});
      wake();
    };
    document.addEventListener('visibilitychange', vis);
    window.addEventListener('pagehide', wake);
    window.addEventListener('focus', vis);
    return () => { un(); clearInterval(tick);
                   document.removeEventListener('visibilitychange', vis);
                   window.removeEventListener('focus', vis);
                   window.removeEventListener('pagehide', wake); };
  }, [flush, qc]);

  // ------------------------------------------------------------ offline state
  const refreshOffline = useCallback(async () => {
    const k = bookRef.current?.key ?? null;
    setOfflineChapters(await cachedChapters(k));
    setTextShards(await cachedShards(k));
  }, []);

  /**
   * The download queue's runner - lib/reconcile.ts, bound to this app.
   *
   * A download is two halves and only one of them can happen here. The server
   * takes the order (`pack: true`) and finishes it whatever happens to this
   * device, restart included; copying the m4a into Cache Storage is the device's
   * half, and iOS suspends the device's half within seconds of the screen going
   * off. So this asks, on every trigger there is: of the chapters still pending,
   * which does the server still need telling about, and which are packed and not
   * here yet?
   *
   * It is the *only* thing that downloads a chapter now. The drawer writes the
   * selection to IndexedDB and calls this; everything after that - the order to
   * the server, the wait, the copy, the retry - happens here, where it survives
   * the drawer being closed and the app being killed.
   *
   * Guarded against overlapping runs rather than queued: two sweeps would fetch
   * the same chapters twice, and the second one's answer is the same as the
   * first's by the time it lands. A trigger that arrives during a run sets a
   * flag instead, so a `packed` event landing mid-sweep is not simply dropped.
   */
  const sweeping = useRef(false);
  const again = useRef(false);
  /* What was last ordered per book, and when. In a ref because it is neither
     render state nor worth persisting: the order itself is on disk at both ends,
     and forgetting this only costs one extra POST after a reload. */
  const orderedRef = useRef(new Map<string, {sig: string; at: number}>());

  const readPending = useCallback(async () => {
    const all = await db.allDownloads().catch(() => [] as PendingDownload[]);
    setPendingDownloads(all);
    return all;
  }, []);

  const sweepDownloads = useCallback(async () => {
    if (sweeping.current) { again.current = true; return; }
    sweeping.current = true;
    try {
      do {
        again.current = false;
        const out = await reconcile({
          pending: readPending,
          rows: async (key) => (await fetchChapters(key)).chapters,
          stored: cachedChapters,
          fetchChapter: downloadChapter,
          save: db.putDownload,
          drop: db.deleteDownload,
          order: (key, o) => Promise.all([
            o.render.length ? renderChapters(key, o.render, true) : null,
            o.build.length ? buildChapters(key, o.build) : null,
          ]),
          lastOrder: (key) => {
            const had = orderedRef.current.get(key);
            return {sig: had?.sig ?? null, sinceMs: had ? Date.now() - had.at : Infinity};
          },
          onOrdered: (key, sig) => orderedRef.current.set(key, {sig, at: Date.now()}),
          /* Which chapters are moving right now, so the row can say "saving"
             rather than sitting on "ready" for the two minutes a 6 MB chapter
             takes. Keyed by book too: a sweep walks every pending book. */
          onFetching: (key, ci, active) => setSaving((had) => {
            const next = new Set(had);
            if (active) next.add(`${key}/${ci}`); else next.delete(`${key}/${ci}`);
            return next;
          }),
          /* Each chapter as it lands, not the whole sweep at the end. A
             twenty-chapter order is an hour of sweeping, and a list that says
             nothing for an hour and then everything at once is indistinguishable
             from one that is stuck. One Cache Storage scan per chapter. */
          onStored: (key, ci) => { touchChapter(key, ci); void refreshOffline(); },
        });
        await readPending();
        if (out.some((b) => b.fetched.length)) {
          await refreshOffline();
          void qc.invalidateQueries({queryKey: keys.chapters});
        }
      } while (again.current);
    } catch {
      // A sweep that cannot run is a sweep that runs on the next trigger. There
      // is nothing to report: the pending record is untouched.
    } finally {
      sweeping.current = false;
    }
  }, [refreshOffline, readPending, qc]);

  /** Add to the queue, write it down, and start on it now. */
  const queueDownload = useCallback(async (cis: number[]) => {
    const b = bookRef.current;
    if (!b || !cis.length) return;
    const held = await cachedChapters(b.key);
    const want = cis.filter((ci) => !held.has(ci));
    if (!want.length) return;
    /* Written down *before* anything is asked of the server. The queue is what
       finishes the job; a selection confirmed as the screen locks has to be one
       the next launch remembers. */
    await db.addDownload({key: b.key, path: b.path, chapters: want, ts: Date.now()})
      .catch(() => {});
    await readPending();
    /* A fresh selection is a changed order, so the sweep posts it on this pass
       rather than waiting out the repeat interval. */
    orderedRef.current.delete(b.key);
    await sweepDownloads();
  }, [readPending, sweepDownloads]);

  /**
   * Take chapters back out of the queue. What is already stored stays stored.
   *
   * Both halves, because only doing the device half would be a lie: the order is
   * on the server too, and a box left rendering seventy chapters nobody wants is
   * the whole night on two ARM cores. The server's refusal is not fatal - this
   * device has stopped waiting either way, and the record here is what decides
   * whether anything gets stored.
   */
  const unqueueDownload = useCallback(async (cis: number[]) => {
    const b = bookRef.current;
    if (!b || !cis.length) return;
    const drop = new Set(cis);
    const had = (await db.allDownloads().catch(() => [])).find((p) => p.key === b.key);
    const left = (had?.chapters ?? []).filter((ci) => !drop.has(ci));
    if (had) {
      if (left.length) await db.putDownload({...had, chapters: left});
      else await db.deleteDownload(b.key);
    }
    orderedRef.current.delete(b.key);
    await readPending();
    await cancelChapters(b.key, [...drop]).catch(() => {});
    void qc.invalidateQueries({queryKey: keys.chapters});
  }, [readPending, qc]);

  /* Every way back in, and a floor under them.
     `visibilitychange` and `focus` are the app being opened or switched to, the
     online manager is the network returning, and the call below is the app
     *starting* - a cold launch after the phone killed the tab mid-download,
     which is the case this whole thing exists for. The interval is what makes
     the queue a queue rather than a catch-up pass: chapters finish packing on
     the server minutes apart, and the app being open is not a reason to wait for
     the next time it is backgrounded. The other triggers are `hello` and a
     `packed` render event off the live stream - both in the stream's handler,
     because a reconnect is a gap whether or not the tab was ever hidden. */
  useEffect(() => {
    const go = () => {
      if (document.visibilityState === 'visible') void sweepDownloads();
    };
    document.addEventListener('visibilitychange', go);
    window.addEventListener('focus', go);
    const un = onlineManager.subscribe((online) => { if (online) void sweepDownloads(); });
    const tick = setInterval(go, SWEEP_EVERY_MS);
    void readPending();
    void sweepDownloads();
    /* Ask for storage that survives pressure. Never granted on iOS today and
       never assumed - every read falls back to the network and the chapter list
       is rebuilt from what Cache Storage still holds - but on a device that does
       grant it, it is the difference between a night of downloads and a morning
       of them being gone. Asked once, silently: a refusal is not news. */
    void requestPersistence();
    return () => {
      document.removeEventListener('visibilitychange', go);
      window.removeEventListener('focus', go);
      clearInterval(tick);
      un();
    };
  }, [sweepDownloads, readPending]);

  /**
   * Give back the chapters he has left behind.
   *
   * Reading is sequential and downloading is ahead of it, so without this the
   * device accumulates every chapter of a 1433-chapter novel it ever played. The
   * rule, and the reason it is safe, is in lib/autotrim.ts: the window is anchored
   * to the furthest chapter ever reached, so going back to re-read never widens
   * it, and the chapter in hand is never a candidate. Silent on purpose - it is
   * housekeeping, not news - but the drawer's audio row says it happens.
   */
  const trimBehind = useCallback(async (key: string, ci: number) => {
    const furthest = furthestReached(readFurthest(key), ci);
    writeFurthest(key, furthest);
    const held = await cachedChapters(key);
    const touched = readTouched(key);
    const gone = chaptersToTrim(held, furthest, KEEP_BEHIND, [ci], touched);
    /* Prune the log to what is actually on the device, whether or not anything
       is being given back. Otherwise it grows one entry per chapter ever read
       and, on a 1433-chapter novel, becomes the largest thing in localStorage. */
    for (const c of [...touched.keys()]) if (!held.has(c) || gone.includes(c)) touched.delete(c);
    writeTouched(key, touched);
    if (!gone.length) return;
    for (const c of gone) await removeChapter(key, c);
    await refreshOffline();
  }, [refreshOffline]);

  // ---------------------------------------------------------------- opening
  /**
   * Open a chapter. `cacheOnly` is the optimistic first paint: it may use only
   * what this device already holds, because the server is still parsing the EPUB
   * and `/api/chapter/{ci}` would answer for whichever book it has open.
   * Resolves to whether the words made it onto the screen.
   */
  const openChapter = useCallback(async (
    target: number, chunk = 0, opts: {cacheOnly?: boolean} = {},
  ): Promise<boolean> => {
    const b = bookRef.current;
    if (!b) return false;
    setCi(target);
    ciRef.current = target;
    setIdxState(chunk);
    idxRef.current = chunk;
    setChunks([]);
    setParas(null);
    const attempt = openSeq.begin();

    /* The endpoint may always be asked now: `/api/chapter/{ci}?book=` is served
       out of that book's own text bundle, whatever book the session holds. The
       old `serverHolds` gate - and the first paint that waited behind /api/load
       for it - is gone. `cacheOnly` still forbids the network entirely, because
       that path exists to paint from this device alone. */
    const text = await loadChapterText(
      textSources(qc, b.key, target), target, shardOf(indexRef.current, b.key, target),
      !opts.cacheOnly, opts.cacheOnly);
    // Another chapter (or book) was asked for while this one was in the air. It
    // owns the wait now, so this one paints nothing and releases nothing.
    if (!attempt.current()) return false;
    if (!text) {
      // An optimistic miss is not a failure: the real open is still coming, and
      // it is the one that owns the flag - see lib/opening.ts.
      if (opts.cacheOnly) return false;
      attempt.settle();
      setMessage('this chapter is not on the device');
      return false;
    }
    attempt.settle();
    setChunks(text.chunks);
    setParas(text.paras ?? null);
    setChapterTitle(text.title);
    setMessage(null);
    /* Remember it here, not only when the playhead reports: a position resolved
       from the server is the one this device must open at when the server is
       gone next time. Without this an offline open falls back to page one. */
    rememberDevicePos(b.path, target, chunk);

    // Start the renderer here. Named, so the server refuses it outright if it
    // holds another book rather than dragging that book's frontier along - which
    // is why this no longer waits to be sure.
    void tellOpen(b.key, target, chunk).catch(() => {});

    const manifest = await loadManifest(qc, b.key, target, text.chunks.length);
    if (!attempt.current()) return true;
    const downloaded = (await cachedChapters(b.key)).has(target);
    await player?.open({
      key: b.key, ci: target, chunkCount: text.chunks.length, manifest,
      packed: !!manifest, downloaded, startChunk: Math.min(chunk, text.chunks.length - 1),
    });
    player?.setMedia(b.title, text.title);
    // ...then the housekeeping, after it, so the two cannot race to set the same
    // offline state with different answers. The touch goes first, or the trim
    // that follows could give back the chapter just opened.
    touchChapter(b.key, target);
    void refreshOffline().then(() => trimBehind(b.key, target));
    return true;
  }, [qc, player, refreshOffline, trimBehind, openSeq]);

  const goChapter = useCallback((d: number) => {
    const next = ciRef.current + d;
    if (next < 0 || next >= chapters.length) return;
    const wasPlaying = player?.playing ?? false;
    void openChapter(next, 0).then(() => { if (wasPlaying) void player?.play(); });
  }, [chapters.length, openChapter, player]);
  goChapterRef.current = goChapter;

  const openBook = useCallback(async (b: BookFile & {key?: string}) => {
    const lib = readLib();
    const guessKey = bookKey(b);
    let entry: LibEntry | undefined = lib[guessKey];
    let server: LoadResult['position'] = null;
    setBookLoading(true);
    /* Opening a book is an open too, and it has to be in the same sequence: it
       holds the wait until one of its own `openChapter` calls takes it over, and
       every way out of here has to release it or the reading view keeps its
       skeleton. */
    const attempt = openSeq.begin();
    setChunks([]);
    setParas(null);
    setMessage(null);
    setMoved(null);

    const queuedFor = async (name: string) =>
      (await db.allPositions()).find((p) => p.book === name) ?? null;

    /* Fast open: a book this device has already read is on the device - table of
       contents, words and last position. Show it now rather than staring at a
       skeleton while the server parses. The server's record still arrives, and
       still wins if it is newer; it only moves the page if the reader has not
       already moved it themselves. */
    let fast: Resume | null = null;
    if (entry) {
      const known: OpenBook = {path: entry.path, name: entry.name, key: entry.key,
                               title: entry.title};
      setBook(known);
      bookRef.current = known;
      setChapters(entry.chapters);
      // The shard map, from Cache Storage if this device has it - scoped by
      // ?book=, like everything cacheable.
      // Awaited, with the rest of the fast open behind it: offline this must
      // settle on the first failure rather than pause until the network is back.
      indexRef.current = await qc.fetchQuery({
        queryKey: keys.bookIndex(known.key),
        queryFn: () => get<BookIndex>(bookIndexUrl(known.key)),
        staleTime: Infinity,
        retry: awaitedRetry,
      }).catch(() => undefined);
      const want = resolveResume({
        queued: await queuedFor(known.name), device: readDevicePos(known.path),
      });
      // Only if this device knows where he was. Opening a book it has never read
      // at chapter one, to yank it to chapter 576 ten seconds later, would be a
      // worse thing to look at than the skeleton.
      if (want.from !== 'none') {
        fast = clampResume(want, entry.chapters.length,
                           entry.chapters.find((c) => c.i === want.chapter)?.n);
        if (await openChapter(fast.chapter, fast.chunk, {cacheOnly: true})) setResumedAt(fast);
        else fast = null;
      }
    }

    try {
      const r = await loadBook(b.path);
      entry = {path: b.path, name: b.name, key: r.key, title: r.title,
               chapters: r.chapters, shards: entry?.shards ?? 0};
      lib[r.key] = entry;
      writeLib(lib);
      server = r.position ?? null;
      void qc.invalidateQueries({queryKey: keys.chapters});
    } catch {
      if (!entry) {
        setMessage('offline, and this book was never opened here');
        setBookLoading(false);
        attempt.settle();
        return;
      }
    }
    const open: OpenBook = {path: entry.path, name: entry.name, key: entry.key, title: entry.title};
    setBook(open);
    bookRef.current = open;
    setChapters(entry.chapters);
    setBookLoading(false);
    await refreshOffline();

    /* Where he stopped. The server's record is the cross-device truth; a position
       still sitting in the outbox is one it cannot know about yet. */
    const want = resolveResume({
      server, queued: await queuedFor(open.name), device: readDevicePos(open.path),
    });
    const at = clampResume(want, entry.chapters.length,
                           entry.chapters.find((c) => c.i === want.chapter)?.n);
    setResumedAt(at);

    // The fast open already put the reader somewhere. Move them only if the
    // server knows better *and* they have not moved themselves since - but tell
    // the server where they are either way, or it renders ahead of chapter one.
    if (fast) {
      const untouched = ciRef.current === fast.chapter && idxRef.current === fast.chunk;
      if (!untouched || (at.chapter === fast.chapter && at.chunk === fast.chunk)) {
        void tellOpen(open.key, ciRef.current, idxRef.current).catch(() => {});
        // A no-op: the fast open superseded this attempt and already painted.
        // Here so that every exit from this function releases the wait.
        attempt.settle();
        return;
      }
    }
    await openChapter(at.chapter, at.chunk);
  }, [qc, openChapter, refreshOffline, openSeq]);

  /* The table of contents arrives on its own schedule; when it does it carries the
     shard map, which is what makes the rest of the book readable offline. */
  useEffect(() => {
    if (!book || !index) return;
    setChapters(index.chapters);
    const lib = readLib();
    const e = lib[book.key];
    if (e) { lib[book.key] = {...e, chapters: index.chapters, shards: index.shards};
             writeLib(lib); }
  }, [book, index]);

  /* The book's words, taken whole - behind the reading view, never in front of it.
     Cheap next to the audio and the only reason the reader works with no network,
     but 17 MB of it is not something to stare at a blank screen for.

     What comes back is a verdict, not just a count: a run that finished short
     says which shards it is short of, and that is what the drawer states. Taken
     without being asked means it has to be honest about what it actually holds. */
  useEffect(() => {
    const k = book?.key;
    if (!k || !index || index.shards <= 0 || optOut.includes(k)) return;
    let alive = true;
    setTextBusy(true);
    void downloadText(k, index.shards, {
      onProgress: (done, total) => { if (alive) setTextProgress({done, total}); },
      stop: () => !alive,
    })
      .then((r) => {
        if (!alive) return;
        setTextShards(r.have);
        // Abandoned is not a verdict - another book is being opened.
        setTextMissing(r.stopped ? [] : r.missing);
      })
      .catch(() => undefined)
      .finally(() => { if (alive) { setTextBusy(false); setTextProgress(null); } });
    return () => { alive = false; };
  }, [book?.key, index, optOut, textRetry]);

  /* Save the words, and the retry for a book that is only partly here: they are
     the same act, so they are the same button. `optOut` is set to a fresh array
     whether or not its contents change, because the identity is what re-runs the
     download effect - and the effect resumes on the missing shards by itself. */
  const saveText = useCallback(() => {
    const k = bookRef.current?.key;
    if (!k) return;
    const next = readOptOut().filter((x) => x !== k);
    writeOptOut(next);
    setTextMissing([]);
    setOptOut(next);            // re-runs the download effect
    setTextRetry((n) => n + 1); // ...even when the opt-out list did not change
  }, []);

  /**
   * Remove everything this device holds for one book - the words and every
   * downloaded chapter - from the Books list, for any book, open or not.
   *
   * The opt-out goes with it. Removing a book and watching its 17 MB of text come
   * straight back on the next open would make the act look like it did nothing,
   * and the drawer's text row still says "not saved" and offers to save it for
   * whoever wants it back. In-memory copies of what was just evicted go too, or
   * Query would keep serving words this device no longer has.
   */
  const dropBook = useCallback(async (key: string) => {
    const next = [...new Set([...readOptOut(), key])];
    writeOptOut(next);
    setOptOut(next);
    await removeBook(key);
    for (const k of ['shard', 'chapter-text', 'manifest'])
      qc.removeQueries({queryKey: [k, key]});
    await refreshOffline();
  }, [qc, refreshOffline]);

  const setIdx = useCallback((i: number, doReport = true) => {
    setIdxState(i);
    player?.seekChunk(i, false);
    if (doReport) void report(i);
  }, [player, report]);

  const setFontScale = useCallback((n: number) => {
    setFontScaleState(n);
    try { localStorage.setItem('narrator.font', String(n)); } catch { /* ignore */ }
  }, []);

  const toggle = useCallback(() => {
    if (!player) return;
    if (!player.playing) player.armMediaSession({prev: () => goChapter(-1), next: () => goChapter(1)});
    player.toggle();
    if (player.playing) tellResume();
    else tellPause();
  }, [player, goChapter]);

  // ------------------------------------------------------------- live updates
  /* The live stream. One connection, opened once for the life of the app: every
     event invalidates the query that owns the thing it changed (lib/live.ts),
     which is why nothing here holds server state of its own. The exception is
     the reading position, which is not a query - it is the page the reader is
     looking at, and moving that is a decision rather than a refetch. */
  const openChapterRef = useRef(openChapter);
  openChapterRef.current = openChapter;

  const onPosition = useCallback((ev: PositionEvent) => {
    const b = bookRef.current;
    const verdict = arbitrate(ev, {
      book: b?.name ?? null,
      chapter: ciRef.current,
      chunk: idxRef.current,
      playing: playerRef.current?.playing ?? false,
      undelivered: !!b && undeliveredRef.current.has(b.name),
    });
    if (verdict.t === 'ignore') return;
    if (verdict.t === 'offer') {
      setMoved({chapter: verdict.chapter, chunk: verdict.chunk});
      return;
    }
    // Not playing: just go there. This is the phone-down, laptop-up case, and
    // it is the whole reason the feature exists.
    setMoved(null);
    void openChapterRef.current(verdict.chapter, verdict.chunk);
  }, []);

  /* The other event that is not a refetch: a `hello` that names no book.
     The server restarted and could not put its session back, and this reader is
     sitting mid-chapter asking a session-scoped endpoint for audio it will not
     serve. Nothing else here would ever re-load the book - loading is what
     picking one from the library does - so without this the reader waits on a
     404 that will never become a 200, silently, until someone re-opens the book
     by hand. That is the stall Fernando saw around a deploy.

     Re-loading is cheap (the server's parse cache makes it a plan read) and
     idempotent, and the player's own once-a-second retry then simply succeeds:
     nothing has to be restarted, the chunk it is waiting for finally arrives.
     Rate-limited because `hello` fires on every reconnect, and a flapping tunnel
     must not turn into a load per reconnect. */
  const healedAt = useRef(0);
  const onHello = useCallback((ev: HelloEvent) => {
    /* A `hello` is the stream saying "this connection is new", which on a
       reconnect means a gap - and a gap is exactly when the server finished
       packing chapters this device asked for and nobody was here to store them.
       Unconditional, and cheap when there is nothing pending: one IndexedDB
       read. */
    void sweepDownloads();
    const b = bookRef.current;
    if (!b || !lostSession(ev, {key: b.key})) return;
    const now = Date.now();
    if (now - healedAt.current < HEAL_MIN_MS) return;
    healedAt.current = now;
    void (async () => {
      try { await loadBook(b.path); } catch { return; }
      void qc.invalidateQueries({queryKey: keys.chapters});
      void qc.invalidateQueries({queryKey: keys.status});
      // And where he actually is, or it renders ahead of chapter one.
      void tellOpen(b.key, ciRef.current, idxRef.current).catch(() => {});
    })();
  }, [qc, sweepDownloads]);

  /* A chapter finished packing on the server: the one moment the queue can
     actually act on. Without this the device only learned about it the next time
     the app was backgrounded and re-opened, which is why a night of rendering
     used to produce nothing on the phone until it was picked up in the morning.
     Cheap when nothing is pending (one IndexedDB read), and the sweep coalesces
     a burst of them into one pass. */
  const onRender = useCallback((ev: RenderEvent) => {
    if (ev.kind === 'packed') void sweepDownloads();
  }, [sweepDownloads]);

  useEffect(() => connectLive(qc, {
    onState: setLive,
    onEvent: (ev) => {
      if (ev.name === 'position') onPosition(ev.data);
      else if (ev.name === 'hello') onHello(ev.data);
      else if (ev.name === 'render') onRender(ev.data);
    },
  }), [qc, onPosition, onHello, onRender]);

  const follow = useCallback(() => {
    const to = moved;
    if (!to) return;
    setMoved(null);
    const wasPlaying = player?.playing ?? false;
    void openChapterRef.current(to.chapter, to.chunk).then((ok) => {
      if (ok && wasPlaying) void player?.play();
    });
  }, [moved, player]);

  const dismissMoved = useCallback(() => setMoved(null), []);

  useEffect(() => () => player?.destroy(), [player]);

  const value = useMemo<Ctx>(() => ({
    book, chapters, index, ci, chunks, paras, chapterTitle, idx, mode, playing, waiting,
    message, conn, offlineChapters, queuedChapters, savingChapters,
    textShards, textBusy, textProgress, textOptOut,
    textMissing, chapterLoading, bookLoading, resumedAt, queued, fontScale,
    status: status.data,
    moved, follow, dismissMoved,
    openBook, openChapter, goChapter, setIdx, toggle,
    nudge: (s: number) => player?.nudge(s),
    setFontScale, refreshOffline, queueDownload, unqueueDownload, sweepDownloads,
    saveText, dropBook, flush, queueNote, player,
  }), [book, chapters, index, ci, chunks, paras, chapterTitle, idx, mode, playing, waiting,
       message, conn, offlineChapters, queuedChapters, savingChapters,
       textShards, textBusy, textProgress, textOptOut,
       textMissing, chapterLoading, bookLoading, resumedAt, queued, fontScale, moved,
       follow, dismissMoved,
       status.data, openBook, openChapter, goChapter, setIdx, toggle, setFontScale,
       refreshOffline, queueDownload, unqueueDownload, sweepDownloads,
       saveText, dropBook, flush, queueNote, player]);

  // A handle for the dev console and for driving the reader from a headless
  // browser. Dev only: the production bundle has no such door.
  if (import.meta.env.DEV && typeof window !== 'undefined')
    (window as unknown as {narrator?: Ctx}).narrator = value;

  return <NarratorContext.Provider value={value}>{children}</NarratorContext.Provider>;
}

/** The last position this device opened or reported. */
function rememberDevicePos(path: string, chapter: number, chunk: number): void {
  try { localStorage.setItem(`narrator.pos:${path}`, JSON.stringify({ci: chapter, idx: chunk})); }
  catch { /* private mode */ }
}

function readDevicePos(path: string): {chapter: number; chunk: number} | null {
  try {
    const raw = localStorage.getItem(`narrator.pos:${path}`);
    if (!raw) return null;
    const p = JSON.parse(raw) as {ci?: number; idx?: number};
    if (p?.ci == null) return null;
    return {chapter: p.ci, chunk: p.idx ?? 0};
  } catch { return null; }
}

/**
 * The three routes of lib/chaptertext.ts, bound to this Query client.
 *
 * Both fetches carry `awaitedRetry`, and that is the whole point of the adapter:
 * these are awaited with a fallback behind them, so a retry that *pauses*
 * because the browser is offline is a promise that never settles and a chapter
 * that never paints - with its shard sitting in Cache Storage. See
 * lib/backoff.ts. Everything else here is the default policy.
 */
function textSources(
  qc: ReturnType<typeof useQueryClient>, key: string, ci: number,
): TextSources {
  const oneChapter = () => qc.fetchQuery({
    queryKey: keys.chapterText(key, ci),
    queryFn: () => get<ChapterText>(chapterTextUrl(key, ci)),
    staleTime: Infinity,
    retry: awaitedRetry,
  }).catch(() => null);
  return {
    heldShard: (shard) => qc.getQueryData<TextShard>(keys.shard(key, shard)),
    oneChapter,
    fetchShard: (shard) => qc.fetchQuery({
      queryKey: keys.shard(key, shard),
      queryFn: () => get<TextShard>(textShardUrl(key, shard)),
      staleTime: Infinity,
      retry: awaitedRetry,
    }).catch(() => null),
  };
}

async function loadManifest(
  qc: ReturnType<typeof useQueryClient>, key: string, ci: number, chunkCount: number,
): Promise<Manifest | null> {
  const m = await qc.fetchQuery({
    queryKey: keys.manifest(key, ci),
    queryFn: () => get<Manifest>(chapterManifestUrl(key, ci)),
    staleTime: Infinity,
    retry: 0,
  }).catch(() => null);
  // A manifest built from a different chunking would seek to the wrong words.
  return isSane(m, chunkCount) ? m : null;
}
