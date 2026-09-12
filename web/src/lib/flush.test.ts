/**
 * Draining the outbox: what goes on the wire, and how many times.
 *
 * Two productions bugs are pinned here. A memo now carries an id, so the retry
 * that follows a note filed after the phone stopped listening is answered with
 * that note instead of transcribed again - and a drain that is already running
 * is joined rather than duplicated, because returning to the PWA fires
 * visibilitychange *and* focus and that posted the same 135 kB recording twice,
 * a second apart.
 */
import {beforeEach, describe, expect, test, vi} from 'vitest';
import {MAX_TRIES, type Memo} from './outbox';

const store = vi.hoisted(() => ({memos: [] as Memo[], deleted: [] as number[]}));

vi.mock('./db', () => ({
  allMemos: async () => store.memos,
  putMemo: async (m: Memo) => {
    store.memos = store.memos.map((x) => (x.id === m.id ? m : x));
  },
  deleteMemo: async (id: number) => {
    store.deleted.push(id);
    store.memos = store.memos.filter((m) => m.id !== id);
  },
  allPositions: async () => [],
  deletePosition: async () => {},
}));

const {flushOutbox} = await import('./flush');

/** node has no FileReader, and the base64 of the blob is not what is under test. */
class FakeReader {
  result = '';
  onload: (() => void) | null = null;
  readAsDataURL(_blob: Blob) {
    this.result = 'data:audio/webm;base64,YXVkaW8=';
    queueMicrotask(() => this.onload?.());
  }
}

const memo = (over: Partial<Memo> = {}): Memo => ({
  id: 1, uid: '9f3b4c7a-2e11-4f00-9a10-7b1d2c3e4f55', blob: new Blob(['audio']),
  mime: 'audio/webm', book: 'Book (2016).epub', chapter: 3, chunk: 40,
  ts: 1_700_000_000_000, tries: 0, ...over,
});

const filed = (file = 'note.md') => ({
  status: 200,
  json: async () => ({ok: true, file, text: 'a thought', language: 'en'}),
});

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => { resolve = r; });
  return {promise, resolve};
}

const sent = (call: unknown[]) =>
  JSON.parse(String((call[1] as {body: string}).body)) as Record<string, unknown>;

beforeEach(() => {
  store.memos = [];
  store.deleted = [];
  vi.stubGlobal('FileReader', FakeReader);
});

describe('what one memo puts on the wire', () => {
  test('the memo names itself, so a retry cannot file a second note', async () => {
    store.memos = [memo()];
    const fetch = vi.fn(async () => filed());
    vi.stubGlobal('fetch', fetch);

    await flushOutbox({online: true});

    expect(fetch).toHaveBeenCalledTimes(1);
    const body = sent(fetch.mock.calls[0]);
    expect(body.id).toBe('9f3b4c7a-2e11-4f00-9a10-7b1d2c3e4f55');
    expect(body.book).toBe('Book (2016)');
    expect(body.chapter).toBe(3);
    // Delivered means the note came back: only then is the only copy deleted.
    expect(store.deleted).toEqual([1]);
  });

  test('a memo queued before ids existed still sends, with none', async () => {
    store.memos = [memo({uid: undefined})];
    const fetch = vi.fn(async () => filed());
    vi.stubGlobal('fetch', fetch);

    await flushOutbox({online: true});

    expect('id' in sent(fetch.mock.calls[0])).toBe(false);
    expect(store.deleted).toEqual([1]);
  });

  test('anything but a 2xx carrying the note keeps the recording', async () => {
    store.memos = [memo()];
    vi.stubGlobal('fetch', vi.fn(async () => ({status: 500, json: async () => ({error: 'boom'})})));

    await flushOutbox({online: true});

    expect(store.deleted).toEqual([]);
    expect(store.memos[0].tries).toBe(1);
    expect(store.memos[0].err).toBe('boom');
  });
});

describe('two triggers, one drain', () => {
  test('a concurrent flush joins the one in flight instead of posting again', async () => {
    store.memos = [memo()];
    const d = deferred<ReturnType<typeof filed>>();
    const fetch = vi.fn(() => d.promise);
    vi.stubGlobal('fetch', fetch);

    // visibilitychange and focus, a millisecond apart.
    const first = flushOutbox({online: true});
    const second = flushOutbox({online: true});
    expect(second).toBe(first);

    d.resolve(filed());
    await Promise.all([first, second]);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(store.deleted).toEqual([1]);
  });

  test('a manual retry that arrives mid-drain still gets the stalled memo sent', async () => {
    // The intent must not be coalesced away: a hand retry is the only thing that
    // sends a memo that has run out of tries, and the drain already running is
    // skipping exactly that memo.
    store.memos = [memo(), memo({id: 2, uid: 'stalled-memo', chunk: 99, tries: MAX_TRIES})];
    const d = deferred<ReturnType<typeof filed>>();
    const fetch = vi.fn()
      .mockImplementationOnce(() => d.promise)
      .mockImplementation(async () => filed('second.md'));
    vi.stubGlobal('fetch', fetch);

    const auto = flushOutbox({online: true});
    const manual = flushOutbox({online: true, manual: true});
    d.resolve(filed());
    await Promise.all([auto, manual]);

    expect(fetch).toHaveBeenCalledTimes(2);
    expect(sent(fetch.mock.calls[1]).id).toBe('stalled-memo');
    expect(store.deleted).toEqual([1, 2]);
  });

  test('going online during an offline drain drains for real', async () => {
    // navigator.onLine flips while a drain that believed it was offline is still
    // running: without the follow-up the memo would wait for the next trigger,
    // and there may not be one.
    store.memos = [memo()];
    const fetch = vi.fn(async () => filed());
    vi.stubGlobal('fetch', fetch);

    const offline = flushOutbox({online: false});
    const online = flushOutbox({online: true});
    await Promise.all([offline, online]);

    expect(fetch).toHaveBeenCalledTimes(1);
    expect(store.deleted).toEqual([1]);
  });

  test('a drain that has finished does not swallow the next one', async () => {
    store.memos = [memo()];
    const fetch = vi.fn(async () => filed());
    vi.stubGlobal('fetch', fetch);

    await flushOutbox({online: true});
    store.memos = [memo({id: 7, uid: 'a-later-memo'})];
    await flushOutbox({online: true});

    expect(fetch).toHaveBeenCalledTimes(2);
    expect(store.deleted).toEqual([1, 7]);
  });
});
