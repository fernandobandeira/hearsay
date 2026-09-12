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
  awaitedRetry, bookIndexUrl, chapterManifestUrl, chapterTextUrl, keys, get, loadBook,
  openChapter as tellOpen, reportPlayhead, tellPause, tellResume, textShardUrl, useBookIndex,
  useStatus,
} from './lib/api';
import {loadChapterText, shardOf, type TextSources} from './lib/chaptertext';
import {
  arbitrate, connectLive, lostSession, type HelloEvent, type LiveState, type PositionEvent,
} from './lib/live';
import {isSane, type Manifest} from './lib/manifest';
import {Player, type PlayMode} from './lib/player';
import {
  bookKey, cachedChapters, cachedShards, downloadText, removeBook, removeChapter,
} from './lib/offline';
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
  textShards: Set<number>;
  textBusy: boolean;
  textProgress: TextProgress | null;
  textOptOut: boolean;
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
  const [textShards, setTextShards] = useState<Set<number>>(new Set());
  const [textBusy, setTextBusy] = useState(false);
  const [textProgress, setTextProgress] = useState<TextProgress | null>(null);
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
  ciRef.current = ci;
  idxRef.current = idx;
  bookRef.current = book;
  indexRef.current = index;

  const textOptOut = !!book && optOut.includes(book.key);

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
  const refreshQueued = useCallback(async () => {
    const [notes, positions] = await Promise.all([db.allMemos(), db.allPositions()]);
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
    try {
      await reportPlayhead(b.key, i);
      return;
    } catch { /* refused or unreachable: it goes in the queue */ }
    await db.putPosition({
      book: b.name, chapter: ciRef.current, chunk: i, ts: Date.now(),
      chapter_title: chapterTitle, chunks_total: chunks.length, chapters_total: chapters.length,
    });
    void refreshQueued();
  }, [chapterTitle, chunks.length, chapters.length, refreshQueued]);


  const flush = useCallback(async (manual = false) => {
    const {flushOutbox, flushPositions} = await import('./lib/flush');
    await flushPositions();
    await flushOutbox({online: onlineManager.isOnline(), manual});
    await refreshQueued();
  }, [refreshQueued]);

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
    if (wasDown.current) { wasDown.current = false; void flush(); }
  }, [conn, flush]);

  useEffect(() => {
    const wake = () => void flush();
    const un = onlineManager.subscribe((online) => { if (online) wake(); });
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
    return () => { un(); document.removeEventListener('visibilitychange', vis);
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
    const gone = chaptersToTrim(await cachedChapters(key), furthest, KEEP_BEHIND, [ci]);
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
    setChapterLoading(true);

    /* The endpoint may always be asked now: `/api/chapter/{ci}?book=` is served
       out of that book's own text bundle, whatever book the session holds. The
       old `serverHolds` gate - and the first paint that waited behind /api/load
       for it - is gone. `cacheOnly` still forbids the network entirely, because
       that path exists to paint from this device alone. */
    const text = await loadChapterText(
      textSources(qc, b.key, target), target, shardOf(indexRef.current, b.key, target),
      !opts.cacheOnly, opts.cacheOnly);
    // Another chapter (or book) was asked for while this one was in the air.
    if (bookRef.current !== b || ciRef.current !== target) return false;
    if (!text) {
      // An optimistic miss is not a failure: the real open is still coming.
      if (opts.cacheOnly) return false;
      setChapterLoading(false);
      setMessage('this chapter is not on the device');
      return false;
    }
    setChapterLoading(false);
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
    if (bookRef.current !== b || ciRef.current !== target) return true;
    const downloaded = (await cachedChapters(b.key)).has(target);
    await player?.open({
      key: b.key, ci: target, chunkCount: text.chunks.length, manifest,
      packed: !!manifest, downloaded, startChunk: Math.min(chunk, text.chunks.length - 1),
    });
    player?.setMedia(b.title, text.title);
    // ...then the housekeeping, after it, so the two cannot race to set the same
    // offline state with different answers.
    void refreshOffline().then(() => trimBehind(b.key, target));
    return true;
  }, [qc, player, refreshOffline, trimBehind]);

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
    setChapterLoading(true);
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
        setChapterLoading(false);
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
        return;
      }
    }
    await openChapter(at.chapter, at.chunk);
  }, [qc, openChapter, refreshOffline]);

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
     but 17 MB of it is not something to stare at a blank screen for. */
  useEffect(() => {
    const k = book?.key;
    if (!k || !index || index.shards <= 0 || optOut.includes(k)) return;
    let alive = true;
    setTextBusy(true);
    void downloadText(k, index.shards,
                      (done, total) => { if (alive) setTextProgress({done, total}); },
                      () => !alive)
      .then((have) => { if (alive) setTextShards(have); })
      .catch(() => undefined)
      .finally(() => { if (alive) { setTextBusy(false); setTextProgress(null); } });
    return () => { alive = false; };
  }, [book?.key, index, optOut]);

  const saveText = useCallback(() => {
    const k = bookRef.current?.key;
    if (!k) return;
    const next = readOptOut().filter((x) => x !== k);
    writeOptOut(next);
    setOptOut(next);            // re-runs the download effect
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
  }, [qc]);

  useEffect(() => connectLive(qc, {
    onState: setLive,
    onEvent: (ev) => {
      if (ev.name === 'position') onPosition(ev.data);
      else if (ev.name === 'hello') onHello(ev.data);
    },
  }), [qc, onPosition, onHello]);

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
    message, conn, offlineChapters, textShards, textBusy, textProgress, textOptOut,
    chapterLoading, bookLoading, resumedAt, queued, fontScale,
    status: status.data,
    moved, follow, dismissMoved,
    openBook, openChapter, goChapter, setIdx, toggle,
    nudge: (s: number) => player?.nudge(s),
    setFontScale, refreshOffline, saveText, dropBook, flush, queueNote, player,
  }), [book, chapters, index, ci, chunks, paras, chapterTitle, idx, mode, playing, waiting,
       message, conn, offlineChapters, textShards, textBusy, textProgress, textOptOut,
       chapterLoading, bookLoading, resumedAt, queued, fontScale, moved, follow, dismissMoved,
       status.data, openBook, openChapter, goChapter, setIdx, toggle, setFontScale,
       refreshOffline, saveText, dropBook, flush, queueNote, player]);

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
