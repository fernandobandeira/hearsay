import {QueryClient} from '@tanstack/react-query';
import {describe, expect, test, vi} from 'vitest';
import {
  arbitrate, behind, connectLive, lostSession, parseEvent,
  type EventSourceLike, type HelloEvent, type Here, type LiveEvent, type LiveState,
  type PositionEvent,
} from './live';

const pos = (p: Partial<PositionEvent> = {}): PositionEvent => ({
  book: '01 - Lord of Mysteries.epub', chapter: 576, chunk: 40, source: 'session', ...p,
});
const here = (p: Partial<Here> = {}): Here => ({
  book: '01 - Lord of Mysteries.epub', chapter: 576, chunk: 40, playing: false, ...p,
});

const hello = (p: Partial<HelloEvent> = {}): HelloEvent =>
  ({heartbeat_s: 15, book: '01 - Lord of Mysteries.epub', key: '01 - Lord of Mysteries',
    chapter: 576, ...p});

describe('lostSession - telling a restart from a gap', () => {
  test('a hello naming no book, with a book open here, is a lost session', () => {
    expect(lostSession(hello({book: null, key: null}), {key: '01 - Lord of Mysteries'}))
      .toBe(true);
  });

  test('nothing open here: there is nothing to put back', () => {
    expect(lostSession(hello({book: null, key: null}), {key: null})).toBe(false);
  });

  test('the same book is an ordinary reconnect', () => {
    expect(lostSession(hello(), {key: '01 - Lord of Mysteries'})).toBe(false);
  });

  test('another book is another device, and not this one to undo', () => {
    // Two readers healing a mismatch would take turns kicking each other's
    // book out of the one server-side session.
    expect(lostSession(hello({book: 'Sapiens (2011).epub', key: 'Sapiens (2011)'}),
                       {key: '01 - Lord of Mysteries'})).toBe(false);
  });
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
    expect(arbitrate(pos({chapter: 577, chunk: 40}), here({chapter: 576, chunk: 40})))
      .toEqual({t: 'follow', chapter: 577, chunk: 40});
    /* Backwards it is a move too, and still worth saying - but it is offered
       rather than taken. That is rule 6, and it changed this case's verdict:
       before, a chapter 564 chapters behind was followed silently. */
    expect(arbitrate(pos({chapter: 12, chunk: 40}), here({chapter: 576, chunk: 40})))
      .toEqual({t: 'offer', chapter: 12, chunk: 40});
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

  /* Reading on a plane and landing. The server's session stopped hearing from
     this device at chapter 570; everything it can say about this book until it
     is told otherwise describes that, including the position it writes *after*
     the network comes back - `/api/playhead` carries only a chunk, so a report
     from chapter 576 is filed under 570 and broadcast. Following it is the bug.
     See `healSession` in state.tsx, which is what takes the flag down. */
  test('a position this device has out-run is not news, wherever it points', () => {
    const read = here({chapter: 576, chunk: 40, undelivered: true});
    expect(arbitrate(pos({chapter: 570, chunk: 12}), read)).toEqual({t: 'ignore'});
    // Not even while playing, where it would otherwise be an offer.
    expect(arbitrate(pos({chapter: 570, chunk: 12}), {...read, playing: true}))
      .toEqual({t: 'ignore'});
  });

  /* The contrast with the case above: an undelivered device says nothing at all
     about this event, a delivered one still hears about it. What it does *not*
     do any more is go there by itself - the event points behind us, which is
     rule 6. Before rule 6 existed this expectation was `follow`. */
  test('...and a delivered device is at least offered the same event', () => {
    expect(arbitrate(pos({chapter: 570, chunk: 12}), here({chapter: 576, chunk: 40})))
      .toEqual({t: 'offer', chapter: 570, chunk: 12});
  });
});

describe('behind - which of two points in a book comes first', () => {
  /* Chunk indices only mean anything inside their chapter, so comparing them
     across a chapter boundary is how a reader ends up half a book back. */
  test('chapter first, chunk only as the tie-break', () => {
    expect(behind({chapter: 10, chunk: 600}, {chapter: 40, chunk: 3})).toBe(true);
    expect(behind({chapter: 40, chunk: 3}, {chapter: 10, chunk: 600})).toBe(false);
  });

  test('inside one chapter it is the chunk', () => {
    expect(behind({chapter: 40, chunk: 3}, {chapter: 40, chunk: 4})).toBe(true);
    expect(behind({chapter: 40, chunk: 4}, {chapter: 40, chunk: 3})).toBe(false);
  });

  /* Equal is not behind. Where we already are is handled further up as an echo,
     and calling it "behind" would turn standing still into an offer. */
  test('the same spot is not behind itself', () => {
    expect(behind({chapter: 40, chunk: 3}, {chapter: 40, chunk: 3})).toBe(false);
  });
});

describe('arbitrate - identity and recency, now that the event carries them', () => {
  const MINE = 'cf0e7a1e-0000-4000-8000-000000000001';
  const THEIRS = 'cf0e7a1e-0000-4000-8000-000000000002';

  /* The exact test that `SLACK` was a proxy for. A phone behind a tunnel reports
     a playhead, reads on for ten chunks while the event is in flight, and gets
     its own report back: distance says "somewhere else", the id says "yours". */
  test("this device's own id is an echo however far it has moved", () => {
    const mine = here({chapter: 576, chunk: 40, device: MINE});
    expect(arbitrate(pos({chunk: 300, device: MINE}), mine)).toEqual({t: 'ignore'});
    expect(arbitrate(pos({chapter: 1200, chunk: 0, device: MINE}), mine)).toEqual({t: 'ignore'});
    // ...and with no slack at all, because the slack is not what is deciding.
    expect(arbitrate(pos({chunk: 300, device: MINE}), mine, 0)).toEqual({t: 'ignore'});
  });

  /* An empty id is a legacy or unidentified client. Two of those must not be
     mistaken for each other, or one anonymous device would mute another. */
  test('an empty id matches nothing, including another empty one', () => {
    expect(arbitrate(pos({chapter: 577, chunk: 0, device: ''}), here({device: ''})))
      .toEqual({t: 'follow', chapter: 577, chunk: 0});
  });

  test('another device is not an echo, and is answered normally', () => {
    expect(arbitrate(pos({chapter: 577, chunk: 0, device: THEIRS}), here({device: MINE})))
      .toEqual({t: 'follow', chapter: 577, chunk: 0});
  });

  /* Rule 4. The event describes a write this device has already written over -
     at best it is our own position taking the long way round through the server,
     and acting on it would be undoing our own last save. */
  test('an event older than this device\'s own last write is not news', () => {
    const mine = here({chapter: 576, chunk: 40, device: MINE, lastWriteMs: 1_700_000_000_000});
    expect(arbitrate(pos({chapter: 700, chunk: 5, device: THEIRS, updated_ms: 1_699_999_000_000}),
                     mine)).toEqual({t: 'ignore'});
    // A tie goes the same way: `seq` orders it on the server and nothing here
    // can see that, so standing still is the recoverable wrong answer.
    expect(arbitrate(pos({chapter: 700, chunk: 5, device: THEIRS, updated_ms: 1_700_000_000_000}),
                     mine)).toEqual({t: 'ignore'});
    // One millisecond newer is a different thing entirely.
    expect(arbitrate(pos({chapter: 700, chunk: 5, device: THEIRS, updated_ms: 1_700_000_000_001}),
                     mine)).toEqual({t: 'follow', chapter: 700, chunk: 5});
  });

  /* Rule 6, the reported bug. Newer than anything we hold, from another device,
     and pointing backwards - which is exactly what a session heal produces. */
  test('newer but behind the furthest reached is an offer, never a follow', () => {
    const mine = here({
      chapter: 40, chunk: 2, playing: false, device: MINE,
      lastWriteMs: 1_700_000_000_000, furthest: {chapter: 40, chunk: 2},
    });
    expect(arbitrate(pos({chapter: 10, chunk: 3, device: THEIRS, updated_ms: 1_700_000_100_000}),
                     mine)).toEqual({t: 'offer', chapter: 10, chunk: 3});
  });

  test('...and the same event ahead of the furthest reached is a follow', () => {
    const mine = here({
      chapter: 40, chunk: 2, playing: false, device: MINE,
      lastWriteMs: 1_700_000_000_000, furthest: {chapter: 40, chunk: 2},
    });
    expect(arbitrate(pos({chapter: 41, chunk: 0, device: THEIRS, updated_ms: 1_700_000_100_000}),
                     mine)).toEqual({t: 'follow', chapter: 41, chunk: 0});
  });

  /* Went back to re-read chapter 5 on the laptop, then forward again on the
     phone. The anchor is the high-water mark rather than where we are, so the
     chapters in between do not read as forward progress the moment the laptop
     says something about one of them. */
  test('the anchor is the furthest reached, not where this device is now', () => {
    const backTracked = here({
      chapter: 5, chunk: 0, device: MINE, furthest: {chapter: 40, chunk: 2},
    });
    expect(arbitrate(pos({chapter: 20, chunk: 0, device: THEIRS}), backTracked))
      .toEqual({t: 'offer', chapter: 20, chunk: 0});
    expect(arbitrate(pos({chapter: 41, chunk: 0, device: THEIRS}), backTracked))
      .toEqual({t: 'follow', chapter: 41, chunk: 0});
  });

  /* The whole reported incident, end to end. The laptop wakes in a background
     tab, heals the one server-side session with `/api/open` at *its* chapter,
     and the server writes and broadcasts a position there, stamped now. The
     phone is forty chapters on and not playing - and before rule 6 it followed,
     silently, backwards. */
  test('the laptop-reconnect scenario: ch 40 here, ch 10 broadcast, fresh stamp', () => {
    const phone = here({
      chapter: 40, chunk: 12, playing: false, device: 'phone',
      lastWriteMs: 1_700_000_000_000, furthest: {chapter: 40, chunk: 12},
    });
    const laptopHeal = pos({
      chapter: 10, chunk: 4, device: 'laptop', updated_ms: 1_700_000_300_000,
      seq: 9812, source: 'session',
    });
    expect(arbitrate(laptopHeal, phone)).toEqual({t: 'offer', chapter: 10, chunk: 4});
  });

  /* A server that has not been updated sends none of the three fields, and this
     reader must behave exactly as it did before they existed. The only verdict
     that legitimately differs is a backwards move, which is the bug. */
  test('a legacy server with no device or stamp falls back to the old behaviour', () => {
    expect(arbitrate(pos({chunk: 40}), here({chunk: 41}))).toEqual({t: 'ignore'});
    expect(arbitrate(pos({chunk: 61}), here({chunk: 40})))
      .toEqual({t: 'follow', chapter: 576, chunk: 61});
    expect(arbitrate(pos({chunk: 61}), here({chunk: 40, playing: true})))
      .toEqual({t: 'offer', chapter: 576, chunk: 61});
    expect(arbitrate(pos({chunk: 41}), here({chunk: 40}), 0))
      .toEqual({t: 'follow', chapter: 576, chunk: 41});
    expect(arbitrate(pos(), here({undelivered: true}))).toEqual({t: 'ignore'});
    expect(arbitrate(pos({book: 'Sapiens (2011).epub'}), here())).toEqual({t: 'ignore'});
  });

  /* One half of the contract present and the other absent must not be read as
     zero: an `updated_ms` of 0 is the epoch, and defaulting to it would make
     every event look older than this device's last write. */
  test('half a recency comparison is no comparison', () => {
    const mine = here({chapter: 576, chunk: 40, lastWriteMs: 1_700_000_000_000});
    expect(arbitrate(pos({chapter: 577, chunk: 0}), mine))
      .toEqual({t: 'follow', chapter: 577, chunk: 0});
    expect(arbitrate(pos({chapter: 577, chunk: 0, updated_ms: 1_699_000_000_000}), here()))
      .toEqual({t: 'follow', chapter: 577, chunk: 0});
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

  test('a position event carrying the device and the instant', () => {
    const ev = parseEvent('position', JSON.stringify({
      book: 'A.epub', chapter: 3, chunk: 9, device: 'dev-1',
      updated_ms: 1_700_000_000_000, seq: 42,
    }));
    expect((ev as {data: PositionEvent}).data).toMatchObject({
      device: 'dev-1', updated_ms: 1_700_000_000_000, seq: 42,
    });
  });

  /* Absent and zero are different answers here: a missing `updated_ms` means a
     server that does not send one and the recency rule stands down, while 0 is
     the epoch and would make every event older than this device's last write.
     So the new fields are read with a guard that keeps them undefined. */
  test('a position event from a server that sends none of them', () => {
    const ev = parseEvent('position', JSON.stringify({book: 'A.epub', chapter: 3, chunk: 9}));
    const d = (ev as {data: PositionEvent}).data;
    expect(d.updated_ms).toBeUndefined();
    expect(d.device).toBeUndefined();
    expect(d.seq).toBeUndefined();
    // ...and junk in those fields reads as absent, not as a value.
    const junk = parseEvent('position', JSON.stringify(
      {book: 'A.epub', chapter: 3, chunk: 9, updated_ms: 'soon', seq: null, device: 7}));
    expect((junk as {data: PositionEvent}).data).toMatchObject({});
    expect((junk as {data: PositionEvent}).data.updated_ms).toBeUndefined();
    expect((junk as {data: PositionEvent}).data.seq).toBeUndefined();
    expect((junk as {data: PositionEvent}).data.device).toBeUndefined();
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
