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

beforeEach(() => {
  store = new Map();
  served = new Map();
  (globalThis as {caches?: unknown}).caches = {
    open: async (name: string) => {
      if (!store.has(name)) store.set(name, new FakeCache());
      return store.get(name) as unknown as Cache;
    },
  };
  (globalThis as {fetch?: unknown}).fetch = async (input: RequestInfo | URL) => {
    const made = served.get(urlOf(input));
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
