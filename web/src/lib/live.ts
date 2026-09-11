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
 *                   the worst thing this feature could do, so it does not: see
 *                   `arbitrate`.
 */
import type {QueryClient} from '@tanstack/react-query';
import {keys} from './api';

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
 * This device's own playhead reports come back as `position` events, and by the
 * time one arrives the playhead has usually moved on a chunk or two. Without
 * slack, every device would spend its life following its own echo.
 */
const SLACK = 2;

/**
 * What to do about a position that moved.
 *
 * The rule that matters is the last one. Following a position while audio is
 * playing means the page jumps mid-sentence and the player restarts somewhere
 * else - the reader is *using* the device, and a sync that overrides a person in
 * the act of listening is not a feature. So while playing, the move becomes an
 * offer: a quiet line saying where the other device went, and a tap to take it.
 * Paused or merely reading, following is the whole point - pick up the phone,
 * put it down, open the laptop, carry on.
 */
export function arbitrate(ev: PositionEvent, here: Here, slack = SLACK): FollowVerdict {
  // Another book entirely. Its position is real and was saved; it is simply not
  // about the page in front of us.
  if (!here.book || ev.book !== here.book) return {t: 'ignore'};
  const sameChapter = ev.chapter === here.chapter;
  if (sameChapter && Math.abs(ev.chunk - here.chunk) <= slack) return {t: 'ignore'};
  const to = {chapter: ev.chapter, chunk: ev.chunk};
  return here.playing ? {t: 'offer', ...to} : {t: 'follow', ...to};
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
export function connectLive(qc: QueryClient, opts: LiveOptions = {}): () => void {
  const url = opts.url ?? '/api/events';
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
        break;
      case 'position':
        // The vault is the position's home; /api/status carries the session's.
        invalidate(keys.status);
        break;
      case 'render':
        invalidate(keys.chapters);
        invalidate(keys.status);
        break;
      case 'books':
        invalidate(keys.books);
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
