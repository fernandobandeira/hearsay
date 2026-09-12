/**
 * Cache Storage, faked just enough.
 *
 * These are the rules about *where* things are filed, which is the half of the
 * offline story Workbox does not own - and the half that broke: two rules
 * sharing one cache name shared one 400-entry expiry budget, so reading enough
 * chapters of a long novel evicted the shards the offline reader depends on.
 */
import {afterEach, beforeEach, describe, expect, test} from 'vitest';
import {
  AUDIO_CACHE, CHAPTER_CACHE, TEXT_CACHE,
  audioBytes, bookKey, cachedChapters, cachedShards, downloadChapter, downloadText,
  heldBooks, removeBook, removeChapter, removeText,
} from './offline';
import {MAX_RETRIES} from './backoff';

const ORIGIN = 'https://reader.test';
const strip = (u: string) => u.replace(ORIGIN, '');
const urlOf = (r: RequestInfo | URL) =>
  strip(typeof r === 'string' ? r : r instanceof URL ? r.href : (r as Request).url);

class FakeCache {
  store = new Map<string, Response>();
  async put(req: RequestInfo | URL, res: Response) { this.store.set(urlOf(req), res); }
  async match(req: RequestInfo | URL) { return this.store.get(urlOf(req)); }
  async delete(req: RequestInfo | URL) { return this.store.delete(urlOf(req)); }
  // The real thing hands back Requests, whose url is absolute.
  async keys() { return [...this.store.keys()].map((u) => ({url: ORIGIN + u}) as Request); }
}

let store: Map<string, FakeCache>;
const cache = (name: string) => {
  if (!store.has(name)) store.set(name, new FakeCache());
  return store.get(name) as FakeCache;
};
const paths = (name: string) => [...cache(name).store.keys()].sort();
/** Seed an entry the way a previous visit would have left it. */
const held = (name: string, url: string) => cache(name).put(url, new Response('{}'));

/** What the network hands back, per URL. Anything unlisted is a 404. */
let served: Map<string, () => Response>;
const serve = (url: string, body: string, headers: Record<string, string> = {}) =>
  served.set(url, () => new Response(body, {status: 200, headers}));
/** A URL that answers badly, or not at all - `status` 0 is a dead tunnel. */
const fail = (url: string, status = 503, times = Infinity) => {
  let left = times;
  const was = served.get(url);
  served.set(url, () => {
    if (left-- <= 0 && was) return was();
    if (status === 0) throw new TypeError('Failed to fetch');
    return new Response('no', {status});
  });
};

/** How many times each URL was asked for. */
let hits: Map<string, number>;

beforeEach(() => {
  store = new Map();
  served = new Map();
  (globalThis as {caches?: unknown}).caches = {
    open: async (name: string) => {
      if (!store.has(name)) store.set(name, new FakeCache());
      return store.get(name) as unknown as Cache;
    },
  };
  hits = new Map();
  (globalThis as {fetch?: unknown}).fetch = async (input: RequestInfo | URL) => {
    const u = urlOf(input);
    hits.set(u, (hits.get(u) ?? 0) + 1);
    const made = served.get(u);
    return made ? made() : new Response('nope', {status: 404});
  };
});

afterEach(() => {
  delete (globalThis as {caches?: unknown}).caches;
});

describe('the chapter endpoint has its own cache', () => {
  test("a downloaded chapter's words are filed where its Workbox rule reads", async () => {
    serve('/api/chapters/3.m4a?book=lom', 'audio', {'content-length': '4096'});
    serve('/api/chapters/3.json?book=lom', '{"chunks":3}', {'content-length': '12'});
    serve('/api/chapter/3?book=lom', '{"chunks":[]}', {'content-length': '13'});

    expect(await downloadChapter('lom', 3)).toBe(4096);
    expect(paths(AUDIO_CACHE))
      .toEqual(['/api/chapters/3.json?book=lom', '/api/chapters/3.m4a?book=lom']);
    expect(paths(CHAPTER_CACHE)).toEqual(['/api/chapter/3?book=lom']);
    expect(paths(TEXT_CACHE)).toEqual([]);           // never the shard budget again
    expect(await cachedChapters('lom')).toEqual(new Set([3]));
  });

  test('removing a chapter takes the old layout with it', async () => {
    serve('/api/chapters/3.m4a?book=lom', 'audio', {'content-length': '10'});
    serve('/api/chapters/3.json?book=lom', '{}', {'content-length': '2'});
    serve('/api/chapter/3?book=lom', '{}', {'content-length': '2'});
    await downloadChapter('lom', 3);
    // A device that read this chapter before the split has it here instead.
    await cache(TEXT_CACHE).put('/api/chapter/3?book=lom', new Response('old'));

    await removeChapter('lom', 3);
    expect(paths(AUDIO_CACHE)).toEqual([]);
    expect(paths(CHAPTER_CACHE)).toEqual([]);
    expect(paths(TEXT_CACHE)).toEqual([]);
  });

  test('giving the words back sweeps both caches, and only this book', async () => {
    await held(TEXT_CACHE, '/api/book.json?book=lom');
    await held(TEXT_CACHE, '/api/text/0.json?book=lom');
    await held(TEXT_CACHE, '/api/chapter/7?book=lom');          // the old layout
    await held(CHAPTER_CACHE, '/api/chapter/8?book=lom');
    await held(TEXT_CACHE, '/api/text/0.json?book=sapiens');
    await held(CHAPTER_CACHE, '/api/chapter/1?book=sapiens');

    await removeText('lom');
    expect(paths(TEXT_CACHE)).toEqual(['/api/text/0.json?book=sapiens']);
    expect(paths(CHAPTER_CACHE)).toEqual(['/api/chapter/1?book=sapiens']);
  });
});

describe('storing a response the python server gzipped', () => {
  const shard = JSON.stringify({shard: 0, from: 0, to: 9, chapters: []});

  test('the encoding headers come off, and the size is the body, not the wire', async () => {
    // fetch decodes the body but leaves the compressed length on the headers.
    serve('/api/book.json?book=lom', '{"shards":1}',
          {'content-encoding': 'gzip', 'content-length': '31', 'vary': 'Accept-Encoding'});
    serve('/api/text/0.json?book=lom', shard,
          {'content-encoding': 'gzip', 'content-length': '31', 'vary': 'Accept-Encoding'});

    await downloadText('lom', 1);
    expect(await cachedShards('lom')).toEqual(new Set([0]));
    const stored = await cache(TEXT_CACHE).match('/api/text/0.json?book=lom');
    expect(stored?.headers.get('content-encoding')).toBeNull();
    expect(stored?.headers.get('content-length')).toBeNull();
    expect(await stored?.text()).toBe(shard);
  });

  test('audio is never gzipped, so it is measured from the header and not read', async () => {
    serve('/api/chapters/1.m4a?book=lom', 'x'.repeat(10), {'content-length': '12345678'});
    serve('/api/chapters/1.json?book=lom', '{}', {'content-length': '2'});
    serve('/api/chapter/1?book=lom', '{}', {'content-length': '2'});
    expect(await downloadChapter('lom', 1)).toBe(12345678);
    expect(await audioBytes('lom')).toBe(12345678);
  });
});

/**
 * The bug this suite was written for.
 *
 * `downloadText` was a plain loop with an unguarded `await put(...)` in it, so
 * one shard failing threw out of the whole download and every later shard was
 * simply never asked for. Nothing surfaced it, every later open stopped at the
 * same shard, and on the 1433-chapter novel that left most of the book
 * unreadable offline - while the chapters either side of the reader, whose shard
 * was saved long ago, kept working. That asymmetry is what it looked like from
 * the outside: the navbar arrows worked and the chapter drawer did not.
 */
describe('taking the whole book, one bad shard at a time', () => {
  const index = '/api/book.json?book=lom';
  const shardUrl = (s: number) => `/api/text/${s}.json?book=lom`;
  const body = (s: number) => JSON.stringify({shard: s, from: s, to: s, chapters: []});
  /** A book of `n` shards, every one of them answerable. */
  const book = (n: number) => {
    serve(index, JSON.stringify({shards: n}));
    for (let s = 0; s < n; s++) serve(shardUrl(s), body(s));
  };
  /** No waiting in a test, but remember what the curve was asked for. */
  const noWait = () => {
    const attempts: number[] = [];
    return {attempts, wait: async (a: number) => { attempts.push(a); }};
  };

  test('a shard that fails does not take the rest of the book with it', async () => {
    book(5);
    fail(shardUrl(2), 503);                       // and never recovers
    const {wait} = noWait();

    const r = await downloadText('lom', 5, {wait});
    expect([...r.have].sort((a, b) => a - b)).toEqual([0, 1, 3, 4]);
    expect(r.missing).toEqual([2]);
    expect(r.stopped).toBe(false);
    // The point: shards 3 and 4 are on the device, which is what the old loop
    // could never manage.
    expect(await cachedShards('lom')).toEqual(new Set([0, 1, 3, 4]));
  });

  test('a book that saved whole says so, with nothing missing', async () => {
    book(3);
    const r = await downloadText('lom', 3, {wait: noWait().wait});
    expect(r.missing).toEqual([]);
    expect(r.have).toEqual(new Set([0, 1, 2]));
    expect(paths(TEXT_CACHE)).toContain(index);
  });

  test('the retry is the reader\'s one curve: five of them, then it is missing', async () => {
    book(2);
    fail(shardUrl(1), 503);
    const {attempts, wait} = noWait();

    const r = await downloadText('lom', 2, {wait});
    // delayFor is called with the attempt index, 0 first - the same argument
    // TanStack Query hands it. MAX_RETRIES waits means MAX_RETRIES+1 tries.
    expect(attempts).toEqual([...Array(MAX_RETRIES).keys()]);
    expect(hits.get(shardUrl(1))).toBe(MAX_RETRIES + 1);
    expect(r.missing).toEqual([1]);
  });

  test('a blip is a blip: the second round gets it', async () => {
    book(3);
    fail(shardUrl(1), 0, 1);                      // one dead-tunnel failure
    const {attempts, wait} = noWait();

    const r = await downloadText('lom', 3, {wait});
    expect(r.missing).toEqual([]);
    expect(attempts).toEqual([0]);                // one wait, then done
    expect(hits.get(shardUrl(1))).toBe(2);
    // The good shards were never re-fetched to get there.
    expect(hits.get(shardUrl(2))).toBe(1);
  });

  test('an answer is an answer: a 404 shard is not asked five more times', async () => {
    book(3);
    fail(shardUrl(1), 404);
    const {attempts, wait} = noWait();

    const r = await downloadText('lom', 3, {wait});
    expect(hits.get(shardUrl(1))).toBe(1);
    expect(attempts).toEqual([]);
    expect(r.missing).toEqual([1]);               // still reported, still honest
  });

  test('the next open resumes on the missing shards and re-asks for nothing else', async () => {
    book(4);
    fail(shardUrl(2), 503);
    await downloadText('lom', 4, {wait: noWait().wait});

    // The blip is over.
    hits.clear();
    served.set(shardUrl(2), () => new Response(body(2), {status: 200}));
    const r = await downloadText('lom', 4, {wait: noWait().wait});

    expect(r.missing).toEqual([]);
    expect(hits.get(shardUrl(2))).toBe(1);
    for (const s of [0, 1, 3]) expect(hits.get(shardUrl(s))).toBeUndefined();
  });

  test('the index is retried too - without it no chapter knows its shard', async () => {
    book(2);
    fail(index, 503, 1);
    const {wait} = noWait();

    const r = await downloadText('lom', 2, {wait});
    expect(hits.get(index)).toBe(2);
    // ...and its failure never stopped the shards on the first pass.
    expect(r.have).toEqual(new Set([0, 1]));
    expect(paths(TEXT_CACHE)).toContain(index);
  });

  test('stop() still abandons promptly, and abandoning is not a verdict', async () => {
    book(6);
    let seen = 0;
    const r = await downloadText('lom', 6, {
      wait: noWait().wait,
      onProgress: () => { seen++; },
      stop: () => seen > 2,
    });
    expect(r.stopped).toBe(true);
    expect(r.have.size).toBeLessThan(6);
    // Everything it never got to is named, so a caller can resume - but the
    // caller is told this was abandoned rather than short.
    expect(r.missing.length).toBeGreaterThan(0);
    expect(hits.get(shardUrl(5))).toBeUndefined();
  });

  /* The backoff is the one place an abandoned download could keep a book it is
     no longer reading alive for fifteen seconds. It is checked before the wait,
     not only inside the loop. */
  test('stop() between rounds does not sit through the backoff', async () => {
    book(2);
    fail(shardUrl(0), 503);
    fail(shardUrl(1), 503);
    const {attempts, wait} = noWait();
    let calls = 0;                     // the opening report, then one per shard
    const r = await downloadText('lom', 2, {
      wait, stop: () => calls >= 3, onProgress: () => { calls++; },
    });
    expect(hits.get(shardUrl(1))).toBe(1);        // the round was finished
    expect(attempts).toEqual([]);                 // ...and then abandoned, unwaited
    expect(r.stopped).toBe(true);
    expect(r.missing).toEqual([0, 1]);
  });

  test('progress is reported against what Cache Storage holds, not what was asked', async () => {
    book(3);
    fail(shardUrl(0), 503);
    const seen: [number, number][] = [];
    await downloadText('lom', 3, {
      wait: noWait().wait, onProgress: (done, total) => seen.push([done, total]),
    });
    expect(seen[0]).toEqual([0, 3]);
    expect(seen[seen.length - 1]).toEqual([2, 3]);   // never 3 of 3
  });
});

describe('the name a book is filed under', () => {
  test('matches the server: the file stem, truncated to 50', () => {
    expect(bookKey({path: '/books/Lord of Mysteries.epub'})).toBe('Lord of Mysteries');
    expect(bookKey({name: 'Lord of Mysteries.epub'})).toBe('Lord of Mysteries');
    expect(bookKey({path: '/books/x/Some Book.EPUB'})).toBe('Some Book');
  });

  test('only the last extension goes, and a dotfile is all stem', () => {
    expect(bookKey({name: 'archive.tar.gz'})).toBe('archive.tar');
    expect(bookKey({name: '.hidden'})).toBe('.hidden');
    expect(bookKey({name: 'no-extension'})).toBe('no-extension');
  });

  test('truncated at 50 characters, as the cache directory is', () => {
    expect(bookKey({name: `${'a'.repeat(80)}.epub`})).toBe('a'.repeat(50));
  });

  test('a key the server already gave us wins over the guess', () => {
    expect(bookKey({path: '/books/Whatever.epub', key: 'canonical'})).toBe('canonical');
  });
});

/**
 * What the Books list needs to know before it offers to remove anything: which
 * books this device is holding, and that removing one takes *everything* -
 * both tiers, both text caches, and the halves a quota eviction leaves behind.
 */
describe('what this device holds, and giving a book back', () => {
  test('every book with anything in any cache is named once', async () => {
    await held(TEXT_CACHE, '/api/book.json?book=lom');
    await held(TEXT_CACHE, '/api/text/0.json?book=lom');
    await held(CHAPTER_CACHE, '/api/chapter/3?book=sapiens');
    await held(AUDIO_CACHE, '/api/chapters/3.m4a?book=Dune%20(1965)');
    await held(AUDIO_CACHE, '/sw.js');            // not book-scoped: not a book
    expect([...await heldBooks()].sort()).toEqual(['Dune (1965)', 'lom', 'sapiens']);
  });

  test('no Cache Storage at all is an empty answer, not a throw', async () => {
    delete (globalThis as {caches?: unknown}).caches;
    expect((await heldBooks()).size).toBe(0);
  });

  test('removing a book takes both tiers and leaves every other book alone', async () => {
    await held(TEXT_CACHE, '/api/book.json?book=lom');
    await held(TEXT_CACHE, '/api/text/0.json?book=lom');
    await held(TEXT_CACHE, '/api/text/1.json?book=lom');
    await held(TEXT_CACHE, '/api/chapter/7?book=lom');        // the old layout
    await held(CHAPTER_CACHE, '/api/chapter/8?book=lom');
    await held(AUDIO_CACHE, '/api/chapters/8.m4a?book=lom');
    await held(AUDIO_CACHE, '/api/chapters/8.json?book=lom');
    await held(AUDIO_CACHE, '/api/chapters/2.m4a?book=sapiens');
    await held(TEXT_CACHE, '/api/book.json?book=sapiens');

    await removeBook('lom');
    expect(paths(AUDIO_CACHE)).toEqual(['/api/chapters/2.m4a?book=sapiens']);
    expect(paths(TEXT_CACHE)).toEqual(['/api/book.json?book=sapiens']);
    expect(paths(CHAPTER_CACHE)).toEqual([]);
    expect([...await heldBooks()]).toEqual(['sapiens']);
  });

  /** The final sweep's reason to exist: `cachedChapters` never names a half. */
  test('a half of a chapter left by a quota eviction goes too', async () => {
    await held(AUDIO_CACHE, '/api/chapters/4.json?book=lom');  // its m4a long gone
    await removeBook('lom');
    expect(paths(AUDIO_CACHE)).toEqual([]);
  });

  test('removing a book this device holds nothing for is a no-op', async () => {
    await held(AUDIO_CACHE, '/api/chapters/1.m4a?book=sapiens');
    await removeBook('lom');
    expect(paths(AUDIO_CACHE)).toEqual(['/api/chapters/1.m4a?book=sapiens']);
  });
});
