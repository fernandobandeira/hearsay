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
  ApiError, awaitedRetry, bookIndexUrl, chapterManifestUrl, chapterTextUrl, fetchChapters, keys,
  get, loadBook, openChapter as tellOpen, reportPlayhead, tellPause, tellResume, textShardUrl,
  useBookIndex, useStatus,
} from './lib/api';
import {loadChapterText, shardOf, type ChapterWords, type TextSources} from './lib/chaptertext';
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
import {deviceId} from './lib/device';
import {afterFastOpen, clampResume, resolveResume, type Resume} from './lib/resume';
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
  moved: {chapter: number; chunk: number; device?: string} | null;
  /** Take the offer. */
  follow: () => void;
  /** Turn it down; it comes back if the other device moves again. */
  dismissMoved: () => void;

  openBook: (b: BookFile & {key?: string}) => Promise<void>;
  /**
   * Open a chapter. Called from outside (the drawer, the chapter list) it is the
   * reader choosing, so it takes the server's session for this book if another
   * book holds it (`claim`, on by default) - see `claimOpen`.
   */
  openChapter: (ci: number, chunk?: number,
                opts?: {cacheOnly?: boolean; claim?: boolean}) => Promise<boolean>;
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
  const [moved, setMoved] =
    useState<{chapter: number; chunk: number; device?: string} | null>(null);
  const [live, setLive] = useState<LiveState>('connecting');
  /* Wrapped like every other storage read here: with site data blocked, iOS
     throws on the *access*, and an initializer that throws is a blank app. */
  const [fontScale, setFontScaleState] = useState(() => {
    let saved = 1;
    try { saved = Number(localStorage.getItem('narrator.font')) || 1; } catch { /* blocked */ }
    return Math.min(1.8, Math.max(0.7, saved));
  });

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
  /** Which `openBook` is the latest - see there. */
  const bookSeqRef = useRef(0);
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
  /* The player is built once, for the life of the provider, so everything it
     calls back into goes through a ref. Calling `report` directly was the bug:
     the closure it kept was the first render's, where the chapter title was ''
     and the book had no chapters, and every position it queued carried that into
     the vault - an empty title and zero totals in `Reading Log.md`. */
  const reportRef = useRef<(i: number) => Promise<void>>(async () => {});
  const chapterEndRef = useRef<() => void>(() => {});
  const advanceRef = useRef<(ci: number) => void>(() => {});
  if (!playerRef.current && typeof window !== 'undefined') {
    playerRef.current = new Player({
      onChunk: (i) => { setIdxState(i); idxRef.current = i; void reportRef.current(i); },
      onMode: setMode,
      onPlaying: setPlaying,
      onWaiting: setWaiting,
      onMessage: setMessage,
      onChapterEnd: () => chapterEndRef.current(),
      onAdvance: (ci) => advanceRef.current(ci),
    });
  }
  const player = playerRef.current;

  /* What a queued position says about the chapter, read at the moment it is
     written rather than at the last render: a chapter the player carried on
     into has a new index long before React has painted its title. */
  const chapterMetaRef = useRef({title: '', chunks: 0});
  const chaptersRef = useRef<ChapMeta[]>([]);
  chaptersRef.current = chapters;
  /* This device painted a position it has not told the server, because the
     server's record was being offered instead (see `openBook`). Until it says
     something, `/api/playhead` would file the next chunk under the server
     session's chapter - so the first report goes as an `/api/open`. */
  const untoldRef = useRef<string | null>(null);

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
     both safer than the guess and one fewer thing to be stale. A 409 is a
     reason to take the position somewhere else - but not to the outbox, which
     only drains on a reconnect. A 409 comes from a server that is right there
     and holding another book, and queueing on it is what left "position queued"
     on screen, online, for as long as that book stayed loaded. `/api/position`
     names its book and takes it whatever the session holds, so that is where it
     goes, now. The outbox is for a server that did not answer.

     Passive on purpose: this never loads the book back. A report is not the
     reader choosing anything, and two devices that each re-took the session on
     their own reports would take turns kicking each other's book out. Pressing
     play and opening a chapter are choices, and those do (`claimOpen`). */
  const report = useCallback(async (i: number) => {
    const b = bookRef.current;
    if (!b) return;
    const ci = ciRef.current;
    rememberDevicePos(b.path, ci, i);
    /* Stamped before the call, not after it. `lastWriteMs` exists to answer
       "could this event predate what I just told the server?", and the honest
       answer has to cover the write that is in flight right now: on a tunnel to
       a box this slow, the `position` event for a report routinely arrives
       before the POST that caused it has resolved here. Stamping late would let
       this device's own echo through in exactly that window. */
    lastWriteRef.current = Date.now();
    /* The first report after reading offline cannot be a playhead report.
       `/api/playhead` carries only the chunk, so the server would file it under
       whatever chapter its session still holds - the one this device left when
       the network went - and then broadcast that as a position, which is the
       reader watching itself get dragged back three chapters. `/api/open` names
       the chapter too, which is the whole of what is out of date. */
    const healing = undeliveredRef.current.has(b.name) || untoldRef.current === b.key;
    const pos = {
      book: b.name, chapter: ci, chunk: i, ts: Date.now(),
      chapter_title: chapterMetaRef.current.title, chunks_total: chapterMetaRef.current.chunks,
      chapters_total: chaptersRef.current.length,
    };
    const delivered = async () => {
      if (untoldRef.current === b.key) untoldRef.current = null;
      if (!undeliveredRef.current.has(b.name)) return;
      undeliveredRef.current.delete(b.name);
      // Superseded by construction: same device, same book, newer position.
      await db.deletePosition(b.name).catch(() => {});
      void refreshQueued();
    };
    let answered = false;
    try {
      if (healing) await tellOpen(b.key, ci, i);
      else await reportPlayhead(b.key, i);
      await delivered();
      return;
    } catch (e) {
      // Any status at all is a server that answered; only a transport failure
      // (status 0) is one that did not.
      answered = e instanceof ApiError && e.status !== 0;
    }
    if (answered) {
      const {deliverPosition} = await import('./lib/flush');
      if (await deliverPosition(pos)) { await delivered(); return; }
    }
    undeliveredRef.current.add(b.name);
    await db.putPosition(pos);
    void refreshQueued();
  }, [refreshQueued]);
  reportRef.current = report;

  /**
   * `/api/open`, taking the server's session for this book if another holds it.
   *
   * Only for what the reader does on purpose - pressing play, opening a
   * chapter. The one server-side session answers 409 when another device has
   * loaded another book, and for a passive report that is the right answer to
   * leave alone (see `report`). For a tap on play it is not: the reader has just
   * chosen this book, here, and a renderer left working on the other one means
   * the chapter they are waiting for is never made. So the book is loaded again
   * - which is exactly what picking it from the library would do - and the open
   * repeated. Anything but a 409 is left to the usual paths.
   */
  const claimOpen = useCallback(async (b: OpenBook, ci: number, chunk: number) => {
    lastWriteRef.current = Date.now();   // /api/open force-writes a position
    try {
      await tellOpen(b.key, ci, chunk);
    } catch (e) {
      if (!(e instanceof ApiError) || e.status !== 409) throw e;
      await loadBook(b.path);
      void qc.invalidateQueries({queryKey: keys.chapters});
      void qc.invalidateQueries({queryKey: keys.status});
      if (bookRef.current?.key !== b.key) return;       // moved on meanwhile
      lastWriteRef.current = Date.now();
      await tellOpen(b.key, ciRef.current, idxRef.current);
    }
    if (untoldRef.current === b.key) untoldRef.current = null;
  }, [qc]);


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
  /* A chapter just landed in Cache Storage. If it is the one playing, the
     player moves onto the stored copy where it stands (`Player.useDownloaded`)
     instead of streaming on until somebody re-opens it; if it is the next one,
     the hand-off at the end of this chapter is worked out again, from the file. */
  const adoptStoredRef = useRef(async (_key: string, _ci: number) => {});
  adoptStoredRef.current = async (key: string, ci: number) => {
    const b = bookRef.current;
    if (!b || b.key !== key) return;
    if (ci === ciRef.current + 1) { void prepareNextRef.current(b, ciRef.current); return; }
    if (ci !== ciRef.current || !chapterMetaRef.current.chunks) return;
    const manifest = await loadManifest(qc, key, ci, chapterMetaRef.current.chunks);
    if (bookRef.current?.key === key && ciRef.current === ci)
      playerRef.current?.useDownloaded(key, ci, manifest);
  };
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
          onStored: (key, ci) => {
            touchChapter(key, ci);
            void refreshOffline();
            void adoptStoredRef.current(key, ci);
          },
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
  /** Put a chapter's words on the page. */
  const showWords = useCallback((text: ChapterWords) => {
    setChunks(text.chunks);
    setParas(text.paras ?? null);
    setChapterTitle(text.title);
    setMessage(null);
    chapterMetaRef.current = {title: text.title, chunks: text.chunks.length};
  }, []);

  /**
   * Open a chapter. `cacheOnly` is the optimistic first paint: it may use only
   * what this device already holds, because the server is still parsing the EPUB
   * and `/api/chapter/{ci}` would answer for whichever book it has open.
   * Resolves to whether this open is the one on the screen *and* in the player:
   * false for a miss, and false for an open another one overtook, because a
   * caller that then presses play would be playing the other open's audio - or,
   * worse, the chapter that just ended, from its first word.
   */
  const openChapter = useCallback(async (
    target: number, chunk = 0, opts: {cacheOnly?: boolean; claim?: boolean} = {},
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
    showWords(text);
    /* Remember it here, not only when the playhead reports: a position resolved
       from the server is the one this device must open at when the server is
       gone next time. Without this an offline open falls back to page one. */
    rememberDevicePos(b.path, target, chunk);

    /* Start the renderer here. Named, so the server refuses it outright if it
       holds another book rather than dragging that book's frontier along - which
       is why this no longer waits to be sure. Not for the optimistic first paint
       of a book, though: that position is only this device's localStorage, and
       telling the server it before `/api/load` has said what the server knows
       wrote it straight over a newer one from another device. `openBook` tells
       the server once it knows which of the two to believe. */
    if (!opts.cacheOnly) {
      if (opts.claim ?? true) void claimOpen(b, target, chunk).catch(() => {});
      else {
        lastWriteRef.current = Date.now();   // /api/open force-writes a position
        void tellOpen(b.key, target, chunk).catch(() => {});
      }
    }

    const manifest = await loadManifest(qc, b.key, target, text.chunks.length);
    if (!attempt.current()) return false;
    const downloaded = (await cachedChapters(b.key)).has(target);
    // The await above is a gap another open can land in; the words it painted
    // are the ones the audio has to match.
    if (!attempt.current()) return false;
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
    void prepareNextRef.current(b, target);
    return true;
  }, [qc, player, refreshOffline, trimBehind, openSeq, claimOpen, showWords]);

  /**
   * Work out, while this chapter plays, what the next one plays from - so the
   * player can carry on into it inside the `ended` event, on the same element,
   * with no network in between. See `Player.prepareNext`, and iOS, which is the
   * reason. Its words are kept too, so the page can follow the audio without a
   * fetch either.
   *
   * Asked again whenever the answer may have changed: the next chapter was
   * packed on the server, or finished downloading here.
   */
  const nextWordsRef = useRef<{key: string; ci: number; text: ChapterWords} | null>(null);
  const prepareNext = useCallback(async (b: OpenBook, ci: number) => {
    const next = ci + 1;
    const still = () => bookRef.current?.key === b.key && ciRef.current === ci;
    if (next >= chaptersRef.current.length) { player?.prepareNext(null); return; }
    const text = await loadChapterText(
      textSources(qc, b.key, next), next, shardOf(indexRef.current, b.key, next), true);
    if (!text || !still()) return;
    const manifest = await loadManifest(qc, b.key, next, text.chunks.length);
    const downloaded = (await cachedChapters(b.key)).has(next);
    if (!still()) return;
    nextWordsRef.current = {key: b.key, ci: next, text};
    player?.prepareNext({
      key: b.key, ci: next, chunkCount: text.chunks.length, manifest,
      packed: !!manifest, downloaded, startChunk: 0,
    });
  }, [qc, player]);
  const prepareNextRef = useRef(prepareNext);
  prepareNextRef.current = prepareNext;

  /**
   * The player has already carried on into chapter `target`; catch the page up.
   *
   * Everything `openChapter` does except the one thing that must not happen
   * here, which is `player.open()`: the audio is playing, on the element that
   * holds the audio session, and resetting it is what used to stop the book at
   * every chapter boundary with the screen off.
   */
  const onAdvance = useCallback(async (target: number) => {
    const b = bookRef.current;
    if (!b) return;
    setCi(target);
    ciRef.current = target;
    setIdxState(0);
    idxRef.current = 0;
    const attempt = openSeq.begin();
    const held = nextWordsRef.current;
    const text = held && held.key === b.key && held.ci === target
      ? held.text
      : await loadChapterText(
        textSources(qc, b.key, target), target, shardOf(indexRef.current, b.key, target), true);
    if (!attempt.current()) return;
    attempt.settle();
    if (!text) { setMessage('this chapter is not on the device'); return; }
    showWords(text);
    rememberDevicePos(b.path, target, idxRef.current);
    player?.setMedia(b.title, text.title);
    // Not a claim: nobody chose this, the book simply went on.
    lastWriteRef.current = Date.now();
    void tellOpen(b.key, target, idxRef.current).catch(() => {});
    touchChapter(b.key, target);
    void refreshOffline().then(() => trimBehind(b.key, target));
    void prepareNext(b, target);
  }, [qc, player, openSeq, refreshOffline, trimBehind, prepareNext, showWords]);
  advanceRef.current = (ci) => void onAdvance(ci);

  const goChapter = useCallback((d: number) => {
    const next = ciRef.current + d;
    if (next < 0 || next >= chaptersRef.current.length) return;
    const wasPlaying = player?.playing ?? false;
    const b = bookRef.current;
    // Forward, playing, and already worked out: the same hand-off the end of a
    // chapter makes, so a lock-screen "next" works with the screen off too.
    if (d === 1 && wasPlaying && b && player?.preparedFor(b.key, next) && player.advance()) return;
    void openChapter(next, 0).then((ok) => { if (ok && wasPlaying) void player?.play(); });
  }, [openChapter, player]);
  const goChapterRef = useRef(goChapter);
  goChapterRef.current = goChapter;

  /* The end of a chapter that nothing was prepared for. At the last chapter
     there is nowhere to go, and the book has finished: say so, rather than
     leaving the bar on "pause" over silence. */
  chapterEndRef.current = () => {
    const next = ciRef.current + 1;
    if (next >= chaptersRef.current.length) { player?.pause(); return; }
    // The slow way, and not a claim: the book going on is nobody's choice.
    const wasPlaying = player?.playing ?? false;
    void openChapter(next, 0, {claim: false})
      .then((ok) => { if (ok && wasPlaying) void player?.play(); });
  };

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
    /* And the book itself has a turn. `attempt` cannot say whether another book
       was tapped meanwhile - the fast open below takes it over by design - so
       without this, tapping two books quickly left the reader on whichever
       `/api/load` answered last, the first one as often as not. */
    const mine = ++bookSeqRef.current;
    const stale = () => mine !== bookSeqRef.current;
    setChunks([]);
    setParas(null);
    setMessage(null);
    setMoved(null);
    untoldRef.current = null;

    const queuedFor = async (name: string) =>
      (await db.allPositions()).find((p) => p.book === name) ?? null;

    /* Fast open: a book this device has already read is on the device - table of
       contents, words and last position. Show it now rather than staring at a
       skeleton while the server parses. The server's record still arrives, and
       still wins if it is newer; it only moves the page if the reader has not
       already moved it themselves. */
    let fast: Resume | null = null;
    let deviceMs: number | null = null;
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
      if (stale()) return;
      deviceMs = readDevicePos(known.path)?.at ?? null;
      const want = resolveResume({
        queued: await queuedFor(known.name), device: readDevicePos(known.path),
      });
      if (stale()) return;
      // Only if this device knows where he was. Opening a book it has never read
      // at chapter one, to yank it to chapter 576 ten seconds later, would be a
      // worse thing to look at than the skeleton.
      if (want.from !== 'none') {
        const f = clampResume(want, entry.chapters.length,
                              entry.chapters.find((c) => c.i === want.chapter)?.n);
        fast = f;
        if (await openChapter(f.chapter, f.chunk, {cacheOnly: true})) setResumedAt(f);
        // A miss - unless another open took over, which is the reader moving,
        // and the check below needs to know they did.
        else if (ciRef.current === f.chapter && idxRef.current === f.chunk) fast = null;
      }
      if (stale()) return;
    }

    try {
      const r = await loadBook(b.path);
      if (stale()) return;
      entry = {path: b.path, name: b.name, key: r.key, title: r.title,
               chapters: r.chapters, shards: entry?.shards ?? 0};
      lib[r.key] = entry;
      writeLib(lib);
      server = r.position ?? null;
      void qc.invalidateQueries({queryKey: keys.chapters});
    } catch {
      if (stale()) return;
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
    if (stale()) return;

    /* Where he stopped. The server's record is the cross-device truth; a position
       still sitting in the outbox is one it cannot know about yet. */
    const want = resolveResume({
      server, queued: await queuedFor(open.name), device: readDevicePos(open.path),
    });
    if (stale()) return;
    const at = clampResume(want, entry.chapters.length,
                           entry.chapters.find((c) => c.i === want.chapter)?.n);

    /* The fast open already put the reader somewhere, and said nothing to the
       server about it. Now there are two answers, and the rule for them is in
       lib/resume.ts: the page never jumps by itself. Either this device's is
       the one to keep - tell the server, or it renders somewhere nobody is - or
       the server knows of reading done elsewhere, and that is *offered*, the
       same quiet line a live move gets. Until he answers, the server is told
       nothing: its record is the other device's, and it stays that way until
       he either follows it or carries on here (`untoldRef`). */
    if (fast) {
      const here = {chapter: ciRef.current, chunk: idxRef.current};
      const verdict = afterFastOpen({
        fast, here, at, serverMs: server?.updated_ms ?? null, deviceMs,
      });
      if (verdict === 'offer') {
        untoldRef.current = open.key;
        setMoved({chapter: at.chapter, chunk: at.chunk});
      } else {
        lastWriteRef.current = Date.now();   // /api/open force-writes a position
        void tellOpen(open.key, here.chapter, here.chunk).catch(() => {});
      }
      // A no-op: the fast open superseded this attempt and already painted.
      // Here so that every exit from this function releases the wait.
      attempt.settle();
      return;
    }
    setResumedAt(at);
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
    if (!player.playing) {
      // Through the ref, so a lock-screen "next" an hour from now is today's
      // goChapter and not the one this render happened to hold.
      player.armMediaSession({prev: () => goChapterRef.current(-1),
                              next: () => goChapterRef.current(1)});
    }
    player.toggle();
    if (player.playing) {
      tellResume();
      /* Pressing play is the reader choosing this book, here - so the server's
         one session is taken for it if another device's book holds it, and put
         where the page is if it was never told (see `claimOpen`). Only then:
         every other report is passive and leaves the session alone. */
      const b = bookRef.current;
      if (b) void claimOpen(b, ciRef.current, idxRef.current).catch(() => {});
    } else tellPause();
  }, [player, claimOpen]);

  // ------------------------------------------------------------- live updates
  /* The live stream. One connection, opened once for the life of the app: every
     event invalidates the query that owns the thing it changed (lib/live.ts),
     which is why nothing here holds server state of its own. The exception is
     the reading position, which is not a query - it is the page the reader is
     looking at, and moving that is a decision rather than a refetch. */
  /* When this device last wrote a position, in epoch milliseconds.
     Read by `arbitrate`: a `position` event stamped at or before this cannot be
     news, because everything the server knew before that moment is something
     this device told it. Deliberately not persisted — it is about this page's
     own writes, and a reload has nothing in flight to disbelieve. */
  const lastWriteRef = useRef(0);

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
      device: deviceId(),
      lastWriteMs: lastWriteRef.current || undefined,
      /* The high-water mark, which is what stops a device that is *behind* from
         pulling the page backwards on its own. Chapter-granular because that is
         what the trim already keeps (`narrator.furthest:`) and it is the
         conservative side of the rounding: `chunk: 0` means a position earlier
         in the same chapter still reads as "not behind" and can be followed,
         while anything in an earlier chapter is offered rather than taken. */
      furthest: b ? {chapter: readFurthest(b.key), chunk: 0} : undefined,
    });
    if (verdict.t === 'ignore') return;
    if (verdict.t === 'offer') {
      /* The label rides along so the line can say *which* device. "moved on
         another device" is exactly as much as the reader used to know, and it
         made the offer something to go and investigate rather than something to
         act on from across the room. Display only - the id did the deciding. */
      setMoved({chapter: verdict.chapter, chunk: verdict.chunk, device: ev.device_name});
      return;
    }
    // Not playing: just go there. This is the phone-down, laptop-up case, and
    // it is the whole reason the feature exists.
    setMoved(null);
    // Not a claim: following another device is not this one choosing a book.
    void openChapterRef.current(verdict.chapter, verdict.chunk, {claim: false});
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
    if (ev.kind !== 'packed') return;
    void sweepDownloads();
    // The next chapter can now be carried on into as a file, not chunk by chunk.
    const b = bookRef.current;
    if (b && (ev.key == null || ev.key === b.key) && ev.chapter === ciRef.current + 1)
      void prepareNextRef.current(b, ciRef.current);
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

  /* Turning the offer down is choosing to stay, and if the offer came from
     opening the book the server has not been told where "here" is yet. */
  const dismissMoved = useCallback(() => {
    setMoved(null);
    const b = bookRef.current;
    if (b && untoldRef.current === b.key)
      void claimOpen(b, ciRef.current, idxRef.current).catch(() => {});
  }, [claimOpen]);

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

/**
 * The last position this device opened or reported, and when it got there.
 *
 * `at` is what lets a book opened here tell "the server has heard from another
 * device since" from "the server is behind me" (lib/resume.ts `afterFastOpen`).
 * It moves only when the position does, so re-painting the same spot - the fast
 * open does exactly that - does not make this device look newer than it is.
 */
function rememberDevicePos(path: string, chapter: number, chunk: number): void {
  try {
    const had = readDevicePos(path);
    const at = had && had.chapter === chapter && had.chunk === chunk && had.at
      ? had.at : Date.now();
    localStorage.setItem(`narrator.pos:${path}`, JSON.stringify({ci: chapter, idx: chunk, at}));
  } catch { /* private mode */ }
}

function readDevicePos(path: string): {chapter: number; chunk: number; at?: number} | null {
  try {
    const raw = localStorage.getItem(`narrator.pos:${path}`);
    if (!raw) return null;
    const p = JSON.parse(raw) as {ci?: number; idx?: number; at?: number};
    if (p?.ci == null) return null;
    return {chapter: p.ci, chunk: p.idx ?? 0,
            at: typeof p.at === 'number' && Number.isFinite(p.at) ? p.at : undefined};
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
