/**
 * `/api/events`, as the reader's nervous system.
 *
 * Before this, every fact on the screen was something the reader had gone and
 * asked for: the heartbeat once a second, the chapter drawer every two seconds
 * while open, the library on a whim. That is fine for one device and wrong for
 * three - a position saved on the phone reached the laptop whenever the laptop
 * next happened to ask, and a new epub in the vault reached it on a refresh.
 *
 * The server has a live stream instead: named JSON events on one long-lived
 * connection, with heartbeat comments so a dead tunnel is noticed rather than
 * assumed alive. This module is the client half, and it is deliberately thin,
 * because **an event is not state**. Every event means "this changed, go and
 * look", and the looking is TanStack Query's job - which is what makes a dropped
 * event (a slow phone the server lagged, a reconnect) a non-event rather than a
 * lost update.
 *
 * Two things here are not thin, and both are tested:
 *
 *   the parser      the stream is the one part of the API that is not generated
 *                   (its body is `text/event-stream`, so there is nothing for
 *                   openapi-ts to type). Every payload is therefore treated as
 *                   untrusted JSON and validated into a typed event, once, here.
 *   the arbitration what to do when the reading position moves somewhere else.
 *                   Yanking the page out from under someone who is listening is
 *                   the worst thing this feature could do, and dragging it
 *                   *backwards* is the second worst - a laptop waking up and
 *                   healing its session broadcasts a position at its own
 *                   chapter, which is a real write and looks exactly like a real
 *                   move. So the arbitration is identity and recency first (whose
 *                   write is this, and is it newer than mine?) and direction
 *                   second: a position behind this device's high-water mark is
 *                   offered, never taken. See `arbitrate`.
 */
import type {QueryClient} from '@tanstack/react-query';
import {keys} from './api';
import {deviceId, deviceLabel} from './device';

// ---------------------------------------------------------------- the events

export interface HelloEvent {
  heartbeat_s: number;
  book: string | null;
  key: string | null;
  chapter: number;
}

/** A position was written into the vault - by this device or another one. */
export interface PositionEvent {
  /** The book's *file name*, which is how positions are keyed. */
  book: string;
  chapter: number;
  chunk: number;
  chapter_title?: string;
  chunks_total?: number;
  chapters_total?: number;
  updated?: string;
  /** `session` for the loaded book's own playhead, `api` for a named write. */
  source?: string;
  /**
   * The write instant, in epoch milliseconds.
   *
   * `updated` stays the naive local string the vault holds, because that is what
   * goes in the file and it is not this reader's to reinterpret. This is the
   * same instant with the ambiguity taken out, resolved on the server where the
   * zone is actually known, and it is what makes "does this event predate my own
   * last write?" a question with an answer.
   */
  updated_ms?: number;
  /**
   * The device whose report caused this write (lib/device).
   *
   * Absent or empty from a server or a client that does not send one, which is
   * the only reason the chunk-distance backstop still exists.
   */
  device?: string;
  /**
   * That device's own label - "iPhone", "Mac".
   *
   * Only ever shown to a person. The offer line used to read "moved on another
   * device", which is exactly as much as the reader knew; naming it is the
   * difference between a notification you have to go and investigate and one you
   * can act on from across the room.
   */
  device_name?: string;
  /**
   * A monotonic per-server counter, incremented on every position write.
   *
   * The tie-break for two writes inside the same millisecond, which the
   * arbitration does not need — see `arbitrate`, which resolves a tie by
   * ignoring — but which is on the wire so that the ordering is recoverable
   * without trusting a clock at all.
   */
  seq?: number;
}

/** The render worker moved. `progress` is throttled to about one a second. */
export interface RenderEvent {
  kind: 'progress' | 'complete' | 'packed' | 'chapter' | string;
  key?: string | null;
  chapter: number;
  render_idx?: number;
  playhead?: number;
  n?: number;
  status?: string;
  ok?: boolean;
  error?: string;
}

/** An epub appeared in (or left) one of the book directories. */
export interface BooksEvent {
  changed: string[];
  count: number;
}

/** A voice memo was transcribed and filed in the vault. */
export interface NoteEvent {
  file: string;
  book?: string;
  chapter?: number;
  chunk?: number;
  language?: string;
}

export type LiveEvent =
  | {name: 'hello'; data: HelloEvent}
  | {name: 'position'; data: PositionEvent}
  | {name: 'render'; data: RenderEvent}
  | {name: 'books'; data: BooksEvent}
  | {name: 'note'; data: NoteEvent};

const num = (v: unknown, fallback = 0): number => (typeof v === 'number' && Number.isFinite(v) ? v : fallback);
/**
 * A number that stays absent when it is absent.
 *
 * `num` defaults, which is right for a field the contract says is always there
 * and wrong for one that may not be: an `updated_ms` of 0 is the epoch, an
 * `updated_ms` of `undefined` is a server that does not send one, and the
 * arbitration does opposite things with them - the first makes every event older
 * than this device's last write, the second falls back to the old behaviour.
 */
const opt = (v: unknown): number | undefined =>
  (typeof v === 'number' && Number.isFinite(v) ? v : undefined);
const str = (v: unknown): string | undefined => (typeof v === 'string' ? v : undefined);

/**
 * One event off the wire, validated.
 *
 * Returns null for anything unrecognised - an event name this version does not
 * know, a payload that is not an object, JSON that will not parse. A future
 * server is allowed to send events this reader has never heard of, and the right
 * response is to ignore them quietly, not to tear the connection down.
 */
export function parseEvent(name: string, raw: string): LiveEvent | null {
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!v || typeof v !== 'object' || Array.isArray(v)) return null;
  const o = v as Record<string, unknown>;
  switch (name) {
    case 'hello':
      return {name: 'hello', data: {
        heartbeat_s: num(o.heartbeat_s, 15),
        book: str(o.book) ?? null,
        key: str(o.key) ?? null,
        chapter: num(o.chapter),
      }};
    case 'position': {
      const book = str(o.book);
      if (!book) return null;          // a position with no book names nothing
      return {name: 'position', data: {
        book,
        chapter: num(o.chapter),
        chunk: num(o.chunk),
        chapter_title: str(o.chapter_title),
        chunks_total: typeof o.chunks_total === 'number' ? o.chunks_total : undefined,
        chapters_total: typeof o.chapters_total === 'number' ? o.chapters_total : undefined,
        updated: str(o.updated),
        source: str(o.source),
        updated_ms: opt(o.updated_ms),
        device: str(o.device),
        device_name: str(o.device_name),
        seq: opt(o.seq),
      }};
    }
    case 'render':
      return {name: 'render', data: {
        kind: str(o.kind) ?? 'progress',
        key: str(o.key) ?? null,
        chapter: num(o.chapter),
        render_idx: typeof o.render_idx === 'number' ? o.render_idx : undefined,
        playhead: typeof o.playhead === 'number' ? o.playhead : undefined,
        n: typeof o.n === 'number' ? o.n : undefined,
        status: str(o.status),
        ok: typeof o.ok === 'boolean' ? o.ok : undefined,
        error: str(o.error),
      }};
    case 'books': {
      const changed = Array.isArray(o.changed)
        ? o.changed.filter((x): x is string => typeof x === 'string')
        : [];
      return {name: 'books', data: {changed, count: num(o.count, changed.length)}};
    }
    case 'note': {
      const file = str(o.file);
      if (!file) return null;
      return {name: 'note', data: {
        file,
        book: str(o.book),
        chapter: typeof o.chapter === 'number' ? o.chapter : undefined,
        chunk: typeof o.chunk === 'number' ? o.chunk : undefined,
        language: str(o.language),
      }};
    }
    default:
      return null;
  }
}

// ----------------------------------------------------------- the arbitration

/** Where this device is, and whether it is in the middle of using it. */
export interface Here {
  /** The open book's *file name*, to match a position event's `book`. */
  book: string | null;
  chapter: number;
  chunk: number;
  /** Audio is actually playing right now. */
  playing: boolean;
  /**
   * This device has read on past what it managed to tell the server.
   *
   * Set from the position queue (lib/db's `positions`), which by construction
   * holds only what a POST could not deliver. While it is set, nothing the
   * server says about this book can be news: it is describing a session that
   * stopped hearing from the reader some chapters ago.
   */
  undelivered?: boolean;
  /**
   * This device's own id (lib/device), to recognise its own echo exactly.
   *
   * Absent on a device that has none - storage blocked, or a build older than
   * the one that mints them - and the arbitration then falls back to the
   * chunk-distance guess it always used.
   */
  device?: string;
  /**
   * Epoch ms of this device's own last *successful* position write for this
   * book. An event stamped at or before it cannot be telling us anything we did
   * not already know: at best it is our own write coming back.
   */
  lastWriteMs?: number;
  /**
   * The furthest point this device has reached in this book.
   *
   * Not the same thing as where it is now, and the difference is the whole of
   * rule 6: going back to re-read a scene must not make every later chapter look
   * like somewhere new to be dragged to.
   */
  furthest?: {chapter: number; chunk: number};
}

/** A point in a book, for the one comparison that matters. */
export interface Spot {
  chapter: number;
  chunk: number;
}

/**
 * Is `a` strictly before `b`?
 *
 * Chapter first, chunk only as the tie-break - a chunk index is meaningful only
 * within its chapter, so comparing chunk 600 of chapter 10 against chunk 3 of
 * chapter 40 by chunk alone would have the reader going backwards by half a
 * book. Equal is not behind: a position exactly where we already are is handled
 * further up as an echo, and calling it "behind" would turn it into an offer.
 */
export function behind(a: Spot, b: Spot): boolean {
  if (a.chapter !== b.chapter) return a.chapter < b.chapter;
  return a.chunk < b.chunk;
}

export type FollowVerdict =
  /** Not about this device, or already where we are. */
  | {t: 'ignore'}
  /** Move the page there now. */
  | {t: 'follow'; chapter: number; chunk: number}
  /** Offer it: someone is listening and must not be interrupted. */
  | {t: 'offer'; chapter: number; chunk: number};

/**
 * Chunks of slack before a position counts as "somewhere else".
 *
 * This used to be the mechanism: a device's own playhead reports come back as
 * `position` events, by the time one arrives the playhead has moved on a chunk
 * or two, and without slack every device spent its life following its own echo.
 * It was always a proxy for "is this mine?", and a bad one in both directions -
 * it swallows a real two-chunk move made on the laptop, and it lets an echo
 * through the moment the tunnel is slow enough for three chunks to pass.
 *
 * It is now the **backstop**, not the mechanism. `Here.device` against
 * `PositionEvent.device` answers the question exactly, and this covers the case
 * where one side cannot: a device with no id (storage blocked), or a server that
 * does not send one back.
 */
const SLACK = 2;

/**
 * What to do about a position that moved.
 *
 * Seven rules, in order, and the order is the design. Each one is a way an event
 * can fail to be news, and the last two are the only ones that move anything:
 *
 *  1. another book - real, saved, not about the page in front of us;
 *  2. our own id on it - our own echo, exactly rather than approximately;
 *  3. this device has read past what it managed to tell the server;
 *  4. it predates this device's own last write, so it cannot be news;
 *  5. it is within `slack` chunks of here - the echo backstop for 2;
 *  6. it is **behind** the furthest this device has reached - offer, never
 *     follow;
 *  7. anything else: follow if idle, offer if something is playing.
 *
 * Rule 7 is the original one and still the reason the feature exists: following
 * while audio plays means the page jumps mid-sentence and the player restarts
 * somewhere else, so a move becomes a quiet line above the player bar instead.
 * Paused or merely reading, following is the whole point - put the phone down,
 * open the laptop, carry on.
 *
 * Rule 6 is the one this round added, and it is a reported bug rather than a
 * refinement: a laptop waking in a background tab calls `/api/open` to heal the
 * one server-side session, the server writes and broadcasts a position at *its*
 * chapter, and the phone - forty chapters further on, not playing - followed it
 * backwards. The event is indistinguishable from a real move, because it is one:
 * somebody's session really did go there. So the answer is not to detect it but
 * to refuse to act on it unasked. Going back is still reachable, as an offer,
 * because sometimes it is deliberate (he did go back to re-read chapter 5 on the
 * laptop); it is never automatic.
 */
export function arbitrate(ev: PositionEvent, here: Here, slack = SLACK): FollowVerdict {
  // 1. Another book entirely. Its position is real and was saved; it is simply
  //    not about the page in front of us.
  if (!here.book || ev.book !== here.book) return {t: 'ignore'};
  /* 2. Our own report, coming back. This is the exact test `slack` below was
        standing in for: an echo is an echo however far the playhead has drifted
        since, and a move made *here* is not news *here* whatever its distance.
        Only a non-empty id counts - '' is a legacy or unidentified client, and
        two of those must not be mistaken for each other. */
  if (ev.device && ev.device === here.device) return {t: 'ignore'};
  /* 3. This device read while the server could not hear it, and has not caught
        the server up yet. Every position the server can currently produce for
        this book predates that reading - including the ones it writes *now*,
        because `/api/playhead` carries only a chunk and the session's chapter is
        still wherever it was when the network went. Following one of those is
        precisely the "came back online and jumped back three chapters" bug, so:
        ignore the server about this book until it has been told where we are.
        See `healSession` in state.tsx, which is what clears this. */
  if (here.undelivered) return {t: 'ignore'};
  /* 4. The event is older than this device's own last successful write, so
        whatever it describes, this device has already written over it. Equal
        stamps ignore too: two writes inside one millisecond are ordered by the
        server's `seq` and by nothing this reader can see, and of the two wrong
        answers available at a tie, standing still is the recoverable one. */
  if (ev.updated_ms !== undefined && here.lastWriteMs !== undefined
      && ev.updated_ms <= here.lastWriteMs) return {t: 'ignore'};
  // 5. The backstop for rule 2, for a device or a server with no id to compare.
  if (ev.chapter === here.chapter && Math.abs(ev.chunk - here.chunk) <= slack) {
    return {t: 'ignore'};
  }
  const to = {chapter: ev.chapter, chunk: ev.chunk};
  /* 6. Behind the high-water mark. `furthest` rather than the current position
        because a reader who has jumped back to check something has not un-read
        the chapters after it, and an event pointing at any of them would
        otherwise read as forward progress. With no high-water mark recorded -
        an older reader, a book just opened - where we are is the best available
        answer and errs toward offering. */
  if (behind(to, here.furthest ?? {chapter: here.chapter, chunk: here.chunk})) {
    return {t: 'offer', ...to};
  }
  // 7. Forward, and genuinely somewhere else.
  return here.playing ? {t: 'offer', ...to} : {t: 'follow', ...to};
}

// ---------------------------------------------------------- the restart check

/**
 * Has the server forgotten which book this reader is on?
 *
 * `hello` fires on connect *and on every reconnect*, which makes it the one
 * place a restart is distinguishable from a gap: a gap comes back with the same
 * session, a restart comes back with an empty one. That distinction matters
 * because playback is a single server-side session and the per-chunk wav
 * endpoint is scoped to it — with no book loaded, `/api/chunk/{ci}/{i}.wav`
 * 404s, `/api/open` and `/api/playhead` answer 409, and a reader that was
 * mid-chapter stalls on a chunk nobody is rendering. Nothing else the reader
 * does re-loads a book: `/api/load` is what *picking* one does.
 *
 * The server restores its own session from disk now, so this should almost
 * never fire; it is the backstop for the cases it cannot (the epub moved or was
 * edited, a work directory that lost `session.json`).
 *
 * Deliberately narrow: **only** a `hello` naming *no* book counts. A `hello`
 * naming a *different* book is another device having loaded one, which is the
 * existing one-session-at-a-time behaviour and not this reader's to undo —
 * healing that would be two devices taking turns kicking each other's book out.
 */
export function lostSession(ev: HelloEvent, here: {key: string | null}): boolean {
  return !!here.key && ev.key === null;
}

// ------------------------------------------------------------- the connection

export type LiveState = 'connecting' | 'live' | 'down';

export interface LiveOptions {
  /** Every validated event, before the query invalidations are applied. */
  onEvent?: (ev: LiveEvent) => void;
  onState?: (s: LiveState) => void;
  /** The URL, overridable for tests. */
  url?: string;
  /**
   * How to open the stream. Defaults to the browser's own `EventSource`, which
   * is what does the reconnecting - with the server's `retry:` interval, and
   * with no code here to get wrong.
   */
  open?: (url: string) => EventSourceLike;
}

/** The slice of `EventSource` this module uses. */
export interface EventSourceLike {
  addEventListener(type: string, fn: (ev: {data?: string}) => void): void;
  close(): void;
  onopen?: ((ev?: unknown) => void) | null;
  onerror?: ((ev?: unknown) => void) | null;
}

/** The events that carry a name of their own, plus what each one invalidates. */
const NAMES = ['hello', 'position', 'render', 'books', 'note'] as const;

/**
 * Open the live stream and keep the query cache honest from it.
 *
 * Returns the teardown. Reconnection is the browser's: `EventSource` retries on
 * its own (the server sends a `retry:` line), and every reconnect re-opens with
 * a `hello`, which is where the live queries are refetched - because whatever
 * happened while the connection was down was, by definition, not delivered.
 */
/**
 * `/api/events`, with this device named in the query string.
 *
 * The one place the device id cannot be a header. `EventSource` is what gives
 * this reader its reconnection for free — the browser retries on the server's
 * own `retry:` interval, with no code here to get wrong — and the price of that
 * is an API with no way to set a request header. None at all. So the endpoint
 * that most needs to know who is connected is the one endpoint that cannot be
 * told the ordinary way, and the query string is what is left. The server reads
 * the header first and falls back to this, so it stays one concept.
 *
 * The id is a random uuid rather than a credential: it identifies a browser
 * profile to itself and grants nothing, which is what makes it safe in a URL
 * that lands in an access log.
 */
export function eventsUrl(): string {
  const q = new URLSearchParams({device: deviceId(), device_name: deviceLabel()});
  return `/api/events?${q.toString()}`;
}

export function connectLive(qc: QueryClient, opts: LiveOptions = {}): () => void {
  const url = opts.url ?? eventsUrl();
  const open = opts.open
    ?? ((u: string) => new EventSource(u) as unknown as EventSourceLike);
  let closed = false;
  const es = open(url);

  const state = (s: LiveState) => { if (!closed) opts.onState?.(s); };
  state('connecting');

  const invalidate = (key: readonly unknown[]) =>
    void qc.invalidateQueries({queryKey: key as unknown[]});

  const handle = (ev: LiveEvent) => {
    opts.onEvent?.(ev);
    switch (ev.name) {
      case 'hello':
        // A fresh connection has missed everything that happened while it was
        // not connected. Ask for all of it again rather than reason about it.
        state('live');
        invalidate(keys.status);
        invalidate(keys.chapters);
        invalidate(keys.books);
        invalidate(keys.library);
        break;
      case 'position':
        // The vault is the position's home; /api/status carries the session's.
        invalidate(keys.status);
        break;
      case 'render':
        invalidate(keys.chapters);
        invalidate(keys.status);
        /* A pack is the one render event that changes a *library* row, because
           packing is what turns the box's night of work into a file a device can
           hold - which is the question the library list exists to answer. The
           other kinds move a chapter's progress, which that list does not show,
           and invalidating on every throttled `progress` would refetch every
           book in the library once a second. */
        if (ev.data.kind === 'packed') invalidate(keys.library);
        break;
      case 'books':
        invalidate(keys.books);
        invalidate(keys.library);
        break;
      case 'note':
        // Nothing server-side to refetch: the note is in the vault, and the
        // device that recorded it already knows. Listeners may still care.
        break;
    }
  };

  for (const name of NAMES) {
    es.addEventListener(name, (e) => {
      if (closed) return;
      const parsed = parseEvent(name, typeof e?.data === 'string' ? e.data : '');
      if (parsed) handle(parsed);
    });
  }
  es.onopen = () => state('live');
  // A stream that drops is not an error anyone should see: EventSource is
  // already reconnecting, and the reader says "reconnecting…" until it does.
  es.onerror = () => state('down');

  return () => {
    closed = true;
    es.onopen = null;
    es.onerror = null;
    es.close();
  };
}
