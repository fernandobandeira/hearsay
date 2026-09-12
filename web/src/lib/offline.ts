/**
 * What is actually on this device.
 *
 * Workbox owns the caching *rules*; this owns the deliberate acts - downloading a
 * chapter, removing one, and asking Cache Storage what survived. Always asked,
 * never remembered: a quota eviction has to show up as missing rather than as a
 * stale tick in the chapter list.
 *
 * Two tiers, and they are different in kind:
 *   text   the whole book, a few hundred kB to ~18 MB, taken on first open
 *          without being asked. It is what makes the reader usable with no
 *          network at all, on chapters that have never been rendered.
 *   audio  per chapter, explicit, tens of MB each. Always the second step.
 */
import {
  ApiError, chapterAudioUrl, chapterManifestUrl, chapterTextUrl, bookIndexUrl, textShardUrl,
} from './api';
import {delayFor, isRetryable, MAX_RETRIES, statusOf} from './backoff';

export const AUDIO_CACHE = 'narrator-audio';   // must match the Workbox rule
export const TEXT_CACHE = 'narrator-text';     // book.json + the shards
/**
 * `/api/chapter/{ci}`, and why it is not in TEXT_CACHE.
 *
 * Workbox's expiration records are keyed by the cache name, so two rules sharing
 * a name share one `maxEntries` budget. These two did: on the 1433-chapter novel
 * each chapter read added an entry, and at 400 the oldest went - which is the
 * shards, downloaded first, and the only reason the *unread* chapters work with
 * no network. Two names, two budgets.
 *
 * Devices upgrading from that layout still have chapter entries sitting in
 * TEXT_CACHE, so every sweep below covers both by URL rather than by cache.
 */
export const CHAPTER_CACHE = 'narrator-chapter';

/** The two caches a chapter's words may be in - the new one, and the old layout. */
const TEXT_CACHES = [TEXT_CACHE, CHAPTER_CACHE] as const;
/** Every cache a book can have something in: what a whole-book sweep walks. */
const ALL_CACHES = [AUDIO_CACHE, TEXT_CACHE, CHAPTER_CACHE] as const;

const caches_ = () => (typeof caches === 'undefined' ? null : caches);

async function open(name: string): Promise<Cache | null> {
  try { return (await caches_()?.open(name)) ?? null; } catch { return null; }
}

/** Ignore Vary everywhere we address an entry by URL - see the Workbox rules. */
const MATCH: CacheQueryOptions = {ignoreVary: true};

/**
 * The name a book's files are filed under, here and on the server.
 *
 * The server computes it as the EPUB path's file stem truncated to 50 characters
 * and hands it back from `/api/load`, which is fine for the book that is open -
 * but the Books list has to name a book's *device* copy before anything has been
 * loaded, and offline it may never be loaded at all. So the same rule is spelled
 * out here: last path segment, minus one trailing extension, truncated to 50. A
 * key the server has already given us always wins over the guess.
 */
export function bookKey(b: {path?: string; name?: string; key?: string}): string {
  if (b.key) return b.key;
  const base = (b.path || b.name || '').replace(/\\/g, '/').split('/').pop() ?? '';
  // `(?!^)` keeps the stem rule's one oddity: a dotfile is all stem, no suffix.
  return base.replace(/(?!^)\.[^.]+$/, '').slice(0, 50);
}

/** Chapter indices whose audio is held on this device, for one book. */
export async function cachedChapters(key: string | null): Promise<Set<number>> {
  const out = new Set<number>();
  const c = await open(AUDIO_CACHE);
  if (!c || !key) return out;
  for (const req of await c.keys()) {
    const u = new URL(req.url);
    if (u.searchParams.get('book') !== key) continue;
    const m = /^\/api\/chapters\/(\d+)\.m4a$/.exec(u.pathname);
    if (m) out.add(Number(m[1]));
  }
  return out;
}

/**
 * Every book this device holds anything at all for, by cache key.
 *
 * One pass over every cache rather than a scan per book: the Books list asks this
 * for the whole library, and it is the only thing that decides whether a book gets
 * a remove action at all. Anything not carrying a `?book=` - the shell, the icons
 * - belongs to no book and is not named.
 */
export async function heldBooks(): Promise<Set<string>> {
  const out = new Set<string>();
  for (const name of ALL_CACHES) {
    const c = await open(name);
    if (!c) continue;
    for (const req of await c.keys()) {
      const k = new URL(req.url).searchParams.get('book');
      if (k) out.add(k);
    }
  }
  return out;
}

/** Text shards held on this device, for one book. */
export async function cachedShards(key: string | null): Promise<Set<number>> {
  const out = new Set<number>();
  const c = await open(TEXT_CACHE);
  if (!c || !key) return out;
  for (const req of await c.keys()) {
    const u = new URL(req.url);
    if (u.searchParams.get('book') !== key) continue;
    const m = /^\/api\/text\/(\d+)\.json$/.exec(u.pathname);
    if (m) out.add(Number(m[1]));
  }
  return out;
}

/** What a whole-book text download actually achieved. */
export interface TextSave {
  /** the shards on this device now - asked of Cache Storage, never counted */
  have: Set<number>;
  /** the ones that are still not, after every retry the policy allows */
  missing: number[];
  /** how many there are altogether */
  shards: number;
  /**
   * Abandoned by `stop()` rather than finished. `missing` is then a snapshot of
   * where it got to, not a verdict on the book - another book is being opened
   * and what this one is short of is nobody's news.
   */
  stopped: boolean;
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** The shards of a book that this device does not hold, in order. */
const absent = (have: ReadonlySet<number>, shards: number): number[] => {
  const out: number[] = [];
  for (let s = 0; s < shards; s++) if (!have.has(s)) out.push(s);
  return out;
};

/**
 * Take the whole book's words, in the background.
 *
 * The index plus every shard, with a callback so the drawer can show it
 * happening. Shards already held are skipped, so this is safe to call on every
 * open - and `stop` lets it be abandoned the moment another book is opened or
 * the reader opts out, because 17 MB of a 1433-chapter novel is not something to
 * keep fetching for a book nobody is reading any more.
 *
 * Nothing waits on this. The chapter in front of the eyes comes from its own
 * endpoint; this is the copy that makes the *rest* of the book work with no
 * network at all.
 *
 * **One shard's failure is not the book's.** This used to be a plain loop with
 * an unguarded `await put(...)` in it, so the first tunnel blip threw straight
 * out of the whole download and every later shard was simply never asked for.
 * The caller swallowed it, so the drawer sat at "7 of 12" with no error and no
 * retry, every later open resumed and stopped at the same shard, and on the
 * 1433-chapter novel that left most of the book unreadable with no network -
 * while the chapters either side of the reader, whose shard was saved long ago,
 * kept working perfectly. That asymmetry is what the bug looked like from the
 * outside: the navbar arrows worked and the chapter drawer did not.
 *
 * So each shard is attempted on its own, failures are collected rather than
 * thrown, and the collection is retried in rounds on the reader's one retry
 * curve (lib/backoff.ts - same base, same factor, same five attempts as every
 * request). A failure that will fail the same way next time (a 404, a 400) is
 * not retried at all; only the round waits, so a bad shard never holds up a
 * good one. The result says what is actually held, which is what makes the
 * drawer able to be honest about a book taken without being asked.
 */
export async function downloadText(
  key: string,
  shards: number,
  opts: {
    onProgress?: (done: number, total: number) => void;
    stop?: () => boolean;
    /** the pause between rounds; injected by the tests, the shared curve otherwise */
    wait?: (attempt: number) => Promise<void>;
  } = {},
): Promise<TextSave> {
  const {onProgress, stop} = opts;
  const wait = opts.wait ?? ((attempt: number) => sleep(delayFor(attempt)));
  const c = await open(TEXT_CACHE);
  const have = await cachedShards(key);
  const gaveUp = (missing: number[]): TextSave =>
    ({have, missing: [...missing].sort((a, b) => a - b), shards, stopped: true});
  if (!c) return {have, missing: absent(have, shards), shards, stopped: false};
  onProgress?.(have.size, shards);

  // The index is in the rounds too: without it nothing can say which shard holds
  // a chapter, so a book with every shard and no index is still not readable.
  // "Settled" rather than "saved", because a 404 is an answer as much as a 200 is
  // and neither is worth five more asks.
  let indexSettled = false;
  let todo = absent(have, shards);
  for (let round = 0; ; round++) {
    if (!indexSettled) indexSettled = await put(c, bookIndexUrl(key))
      .then(() => true, (e: unknown) => !isRetryable(statusOf(e)));
    const failed: number[] = [];
    for (let i = 0; i < todo.length; i++) {
      if (stop?.()) return gaveUp([...failed, ...todo.slice(i)]);
      const s = todo[i];
      try {
        await put(c, textShardUrl(key, s));
        have.add(s);
      } catch (e) {
        // Worth asking again, or answered for good? A 404 shard means the bundle
        // was rebuilt with a different shape, and five more asks will say so.
        if (isRetryable(statusOf(e))) failed.push(s);
      }
      onProgress?.(have.size, shards);
    }
    todo = failed;
    if ((indexSettled && !todo.length) || round >= MAX_RETRIES) break;
    if (stop?.()) return gaveUp(todo);
    await wait(round);
  }

  // The verdict comes from Cache Storage rather than from the bookkeeping above:
  // a quota eviction during the download is exactly the case where counting what
  // was stored and asking what is stored give different answers.
  const held = await cachedShards(key);
  return {have: held, missing: absent(held, shards), shards, stopped: false};
}

/**
 * Give the words back. His consent concern, answered: the whole-book text is
 * taken without being asked, so removing it has to be one obvious action - and
 * it has to take the index with it, or the next open would look half-saved.
 */
export async function removeText(key: string): Promise<void> {
  // Both caches: chapters live in CHAPTER_CACHE now, and in TEXT_CACHE on any
  // device that read them before the split.
  for (const name of TEXT_CACHES) {
    const c = await open(name);
    if (!c) continue;
    for (const req of await c.keys()) {
      const u = new URL(req.url);
      if (u.searchParams.get('book') !== key) continue;
      // the index, the shards, and the single chapters read since: all of it
      if (u.pathname === '/api/book.json'
          || /^\/api\/text\/\d+\.json$/.test(u.pathname)
          || /^\/api\/chapter\/\d+$/.test(u.pathname)) await c.delete(req);
    }
  }
}

/**
 * Three URLs make a chapter listenable with no network: its audio, the
 * chunk->time manifest that keeps reading positions meaningful, and its text.
 * Returns the size of the audio.
 */
export async function downloadChapter(key: string, ci: number): Promise<number> {
  const audio = await open(AUDIO_CACHE);
  // The chapter's words go where the Workbox rule for them reads from.
  const text = await open(CHAPTER_CACHE);
  if (!audio) throw new Error('this browser will not store offline audio');
  const size = await put(audio, chapterAudioUrl(key, ci));
  await put(audio, chapterManifestUrl(key, ci));
  if (text) await put(text, chapterTextUrl(key, ci)).catch(() => 0);
  return size;
}

export async function removeChapter(key: string, ci: number): Promise<void> {
  const audio = await open(AUDIO_CACHE);
  await audio?.delete(chapterAudioUrl(key, ci), MATCH);
  await audio?.delete(chapterManifestUrl(key, ci), MATCH);
  // Both, so a copy stored under the old shared layout goes too.
  for (const name of TEXT_CACHES) {
    const c = await open(name);
    await c?.delete(chapterTextUrl(key, ci), MATCH);
  }
}

/**
 * Give a whole book back: both tiers, in one act.
 *
 * Removing a book has to mean *everything this device holds for it* - the words
 * that were taken without being asked and every chapter downloaded on purpose -
 * or "removed" would still be sitting on a phone as a few hundred megabytes of
 * audio. The server keeps its own files; this is a device eviction only.
 *
 * The sweep at the end is not redundant: quota eviction and abandoned downloads
 * leave halves behind (a manifest whose m4a is gone, so `cachedChapters` never
 * names that chapter), and anything still carrying this `?book=` belongs to the
 * book being removed whatever shape it is in.
 */
export async function removeBook(key: string): Promise<void> {
  await removeText(key);
  for (const ci of await cachedChapters(key)) await removeChapter(key, ci);
  for (const name of ALL_CACHES) {
    const c = await open(name);
    if (!c) continue;
    for (const req of await c.keys())
      if (new URL(req.url).searchParams.get('book') === key) await c.delete(req, MATCH);
  }
}

/** Bytes of chapter audio held for one book. */
export async function audioBytes(key: string | null): Promise<number> {
  const c = await open(AUDIO_CACHE);
  if (!c || !key) return 0;
  let n = 0;
  for (const req of await c.keys()) {
    const u = new URL(req.url);
    if (u.searchParams.get('book') !== key) continue;
    if (!u.pathname.endsWith('.m4a')) continue;
    const res = await c.match(req, MATCH);
    const len = res?.headers.get('content-length');
    n += len ? Number(len) : (await res?.blob())?.size ?? 0;
  }
  return n;
}

export async function storageEstimate(): Promise<{usage: number; quota: number} | null> {
  try {
    const e = await navigator.storage?.estimate?.();
    if (!e || e.usage == null) return null;
    return {usage: e.usage, quota: e.quota ?? 0};
  } catch { return null; }
}

/**
 * Ask for persistent storage. Not granted on iOS today, and never assumed: every
 * read falls back to the network, and the chapter list is rebuilt from what Cache
 * Storage actually still holds.
 */
export async function requestPersistence(): Promise<boolean> {
  try { return (await navigator.storage?.persist?.()) ?? false; } catch { return false; }
}

/**
 * Store one URL, and measure what was actually stored.
 *
 * The wire is not always what the browser hands back. This reader is
 * interchangeable between the rust server and the python one, and the python one
 * serves `/api/book.json` and `/api/text/*.json` gzipped: `fetch` decodes the
 * body but leaves `Content-Encoding: gzip` and the *compressed* `Content-Length`
 * on the response object. Storing that response verbatim puts a decoded body in
 * Cache Storage under headers claiming it is gzip - which a consumer is entitled
 * to act on - and the size accounting would report the compressed number.
 *
 * So when an encoding is declared, the body is buffered and re-stored with those
 * two headers stripped, and the blob's own size is the answer. Only text takes
 * that path: it is a few hundred kB at most, and the m4a - tens of MB, never
 * gzipped - keeps the streaming clone and the header size rather than being read
 * into memory just to be measured.
 */
async function put(c: Cache, url: string): Promise<number> {
  const res = await fetch(url, {cache: 'no-store'});
  // ApiError rather than Error, so the retry policy can read the status off it:
  // a 503 is a blip worth repeating and a 404 is an answer. See lib/backoff.ts.
  if (!res.ok) throw new ApiError(res.status, `${url.split('?')[0]} → ${res.status}`);
  if (res.headers.get('content-encoding')) {
    const body = await res.blob();
    const headers = new Headers(res.headers);
    headers.delete('content-encoding');
    headers.delete('content-length');
    await c.put(url, new Response(body, {status: res.status, statusText: res.statusText, headers}));
    return body.size;
  }
  const size = Number(res.headers.get('content-length') ?? 0);
  await c.put(url, res.clone());
  return size;
}
