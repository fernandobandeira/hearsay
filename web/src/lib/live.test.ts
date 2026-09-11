import {QueryClient} from '@tanstack/react-query';
import {describe, expect, test, vi} from 'vitest';
import {
  arbitrate, connectLive, parseEvent,
  type EventSourceLike, type Here, type LiveEvent, type LiveState, type PositionEvent,
} from './live';

const pos = (p: Partial<PositionEvent> = {}): PositionEvent => ({
  book: '01 - Lord of Mysteries.epub', chapter: 576, chunk: 40, source: 'session', ...p,
});
const here = (p: Partial<Here> = {}): Here => ({
  book: '01 - Lord of Mysteries.epub', chapter: 576, chunk: 40, playing: false, ...p,
});

describe('arbitrate - what to do when the position moves somewhere else', () => {
  test('a position for another book is not about this page', () => {
    expect(arbitrate(pos({book: 'Sapiens (2011).epub'}), here())).toEqual({t: 'ignore'});
    expect(arbitrate(pos(), here({book: null}))).toEqual({t: 'ignore'});
  });

  test("this device's own echo is ignored, a chunk or two of drift included", () => {
    expect(arbitrate(pos({chunk: 40}), here({chunk: 40}))).toEqual({t: 'ignore'});
    expect(arbitrate(pos({chunk: 40}), here({chunk: 42}))).toEqual({t: 'ignore'});
    expect(arbitrate(pos({chunk: 42}), here({chunk: 40}))).toEqual({t: 'ignore'});
  });

  test('idle here, moved there: follow it - that is the whole feature', () => {
    expect(arbitrate(pos({chunk: 61}), here({chunk: 40})))
      .toEqual({t: 'follow', chapter: 576, chunk: 61});
    expect(arbitrate(pos({chapter: 577, chunk: 0}), here({chapter: 576, chunk: 40})))
      .toEqual({t: 'follow', chapter: 577, chunk: 0});
  });

  test('a new chapter is a move even at the same chunk index', () => {
    expect(arbitrate(pos({chapter: 12, chunk: 40}), here({chapter: 576, chunk: 40})))
      .toEqual({t: 'follow', chapter: 12, chunk: 40});
  });

  test('playing here: offer it, never yank the page out from under a listener', () => {
    expect(arbitrate(pos({chunk: 61}), here({chunk: 40, playing: true})))
      .toEqual({t: 'offer', chapter: 576, chunk: 61});
    // ...and an echo is still an echo, so no offer appears for one.
    expect(arbitrate(pos({chunk: 41}), here({chunk: 40, playing: true})))
      .toEqual({t: 'ignore'});
  });

  test('the slack is a parameter, so a device that wants none can say so', () => {
    expect(arbitrate(pos({chunk: 41}), here({chunk: 40}), 0))
      .toEqual({t: 'follow', chapter: 576, chunk: 41});
  });
});

describe('parseEvent - the one part of the API that is not generated', () => {
  test('a position event', () => {
    const ev = parseEvent('position', JSON.stringify(
      {book: 'A.epub', chapter: 3, chunk: 9, source: 'api', updated: '2026-09-11T20:00:00'}));
    expect(ev).toEqual({name: 'position', data: {
      book: 'A.epub', chapter: 3, chunk: 9, source: 'api',
      updated: '2026-09-11T20:00:00',
      chapter_title: undefined, chunks_total: undefined, chapters_total: undefined,
    }});
  });

  test('a render event keeps its kind, which is what a listener switches on', () => {
    const ev = parseEvent('render', JSON.stringify({kind: 'packed', chapter: 4, ok: true}));
    expect(ev?.name).toBe('render');
    expect((ev as {data: {kind: string; ok?: boolean}}).data.kind).toBe('packed');
  });

  test('a books event survives a payload with junk in the list', () => {
    const ev = parseEvent('books', JSON.stringify({changed: ['a.epub', 7, null], count: 3}));
    expect(ev).toEqual({name: 'books', data: {changed: ['a.epub'], count: 3}});
  });

  test('nonsense is dropped, not thrown', () => {
    expect(parseEvent('position', 'not json')).toBeNull();
    expect(parseEvent('position', '[]')).toBeNull();
    expect(parseEvent('position', '{"chapter":1}')).toBeNull();   // no book
    expect(parseEvent('note', '{"language":"en"}')).toBeNull();   // no file
    // An event this version has never heard of is simply not ours.
    expect(parseEvent('weather', '{"sun":true}')).toBeNull();
  });
});

/** An EventSource that never touches the network. */
class FakeSource implements EventSourceLike {
  handlers = new Map<string, (ev: {data?: string}) => void>();
  closed = false;
  onopen: ((ev?: unknown) => void) | null = null;
  onerror: ((ev?: unknown) => void) | null = null;
  addEventListener(type: string, fn: (ev: {data?: string}) => void) {
    this.handlers.set(type, fn);
  }
  close() { this.closed = true; }
  send(name: string, data: unknown) {
    this.handlers.get(name)?.({data: JSON.stringify(data)});
  }
}

describe('connectLive - events become refetches', () => {
  const wire = () => {
    const qc = new QueryClient();
    const invalidate = vi.spyOn(qc, 'invalidateQueries').mockResolvedValue();
    const src = new FakeSource();
    const seen: LiveEvent[] = [];
    const states: LiveState[] = [];
    const stop = connectLive(qc, {
      open: () => src,
      onEvent: (e) => seen.push(e),
      onState: (s) => states.push(s),
    });
    const keysOf = () => invalidate.mock.calls
      .map((c) => JSON.stringify((c[0] as {queryKey: unknown[]}).queryKey));
    return {src, seen, states, stop, keysOf, invalidate};
  };

  test('hello refetches everything: a reconnect missed whatever happened', () => {
    const w = wire();
    w.src.send('hello', {heartbeat_s: 15, book: 'A.epub', key: 'A', chapter: 0});
    expect(w.keysOf()).toEqual(['["status"]', '["chapters"]', '["books"]']);
    expect(w.states).toEqual(['connecting', 'live']);
    w.stop();
  });

  test('each event invalidates the query that owns what it changed', () => {
    const w = wire();
    w.src.send('position', {book: 'A.epub', chapter: 1, chunk: 2});
    expect(w.keysOf()).toEqual(['["status"]']);

    const w2 = wire();
    w2.src.send('render', {kind: 'progress', chapter: 3});
    expect(w2.keysOf()).toEqual(['["chapters"]', '["status"]']);

    const w3 = wire();
    w3.src.send('books', {changed: ['New.epub'], count: 1});
    expect(w3.keysOf()).toEqual(['["books"]']);

    // A filed note is already in the vault; there is nothing to refetch.
    const w4 = wire();
    w4.src.send('note', {file: '202609112000 a thought.md'});
    expect(w4.keysOf()).toEqual([]);
    expect(w4.seen.map((e) => e.name)).toEqual(['note']);
    [w, w2, w3, w4].forEach((x) => x.stop());
  });

  test('a dropped stream is a state, not an error - EventSource is reconnecting', () => {
    const w = wire();
    w.src.onerror?.();
    expect(w.states).toEqual(['connecting', 'down']);
    w.src.onopen?.();
    expect(w.states).toEqual(['connecting', 'down', 'live']);
    w.stop();
  });

  test('teardown closes the stream and stops delivering', () => {
    const w = wire();
    w.stop();
    expect(w.src.closed).toBe(true);
    w.src.send('books', {changed: [], count: 0});
    expect(w.seen).toEqual([]);
    expect(w.keysOf()).toEqual([]);
  });
});
