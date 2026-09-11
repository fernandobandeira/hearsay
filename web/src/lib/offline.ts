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
import {chapterAudioUrl, chapterManifestUrl, chapterTextUrl, bookIndexUrl, textShardUrl} from './api';

export const AUDIO_CACHE = 'narrator-audio';   // must match the Workbox rule
export const TEXT_CACHE = 'narrator-text';

const caches_ = () => (typeof caches === 'undefined' ? null : caches);

async function open(name: string): Promise<Cache | null> {
  try { return (await caches_()?.open(name)) ?? null; } catch { return null; }
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

/**
 * Take the whole book's words, in the background.
 *
 * The index plus every shard, one at a time, with a callback so the drawer can
 * show it happening. Shards already held are skipped, so this is safe to call on
 * every open - and `stop` lets it be abandoned the moment another book is opened
 * or the reader opts out, because 17 MB of a 1433-chapter novel is not something
 * to keep fetching for a book nobody is reading any more.
 *
 * Nothing waits on this. The chapter in front of the eyes comes from its own
 * endpoint; this is the copy that makes the *rest* of the book work with no
 * network at all.
 */
export async function downloadText(
  key: string,
  shards: number,
  onProgress?: (done: number, total: number) => void,
  stop?: () => boolean,
): Promise<Set<number>> {
  const c = await open(TEXT_CACHE);
  if (!c) return new Set();
  let have = await cachedShards(key);
  onProgress?.(have.size, shards);
  await put(c, bookIndexUrl(key)).catch(() => 0);
  for (let s = 0; s < shards; s++) {
    if (stop?.()) return have;
    if (have.has(s)) { onProgress?.(have.size, shards); continue; }
    await put(c, textShardUrl(key, s));
    have = await cachedShards(key);
    onProgress?.(have.size, shards);
  }
  return have;
}

/**
 * Give the words back. His consent concern, answered: the whole-book text is
 * taken without being asked, so removing it has to be one obvious action - and
 * it has to take the index with it, or the next open would look half-saved.
 */
export async function removeText(key: string): Promise<void> {
  const c = await open(TEXT_CACHE);
  if (!c) return;
  for (const req of await c.keys()) {
    const u = new URL(req.url);
    if (u.searchParams.get('book') !== key) continue;
    // the index, the shards, and the single chapters read since: all of it
    if (u.pathname === '/api/book.json'
        || /^\/api\/text\/\d+\.json$/.test(u.pathname)
        || /^\/api\/chapter\/\d+$/.test(u.pathname)) await c.delete(req);
  }
}

/**
 * Three URLs make a chapter listenable with no network: its audio, the
 * chunk->time manifest that keeps reading positions meaningful, and its text.
 * Returns the size of the audio.
 */
export async function downloadChapter(key: string, ci: number): Promise<number> {
  const audio = await open(AUDIO_CACHE);
  const text = await open(TEXT_CACHE);
  if (!audio) throw new Error('this browser will not store offline audio');
  const size = await put(audio, chapterAudioUrl(key, ci));
  await put(audio, chapterManifestUrl(key, ci));
  if (text) await put(text, chapterTextUrl(key, ci)).catch(() => 0);
  return size;
}

export async function removeChapter(key: string, ci: number): Promise<void> {
  const audio = await open(AUDIO_CACHE);
  const text = await open(TEXT_CACHE);
  await audio?.delete(chapterAudioUrl(key, ci));
  await audio?.delete(chapterManifestUrl(key, ci));
  await text?.delete(chapterTextUrl(key, ci));
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
    const res = await c.match(req);
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

async function put(c: Cache, url: string): Promise<number> {
  const res = await fetch(url, {cache: 'no-store'});
  if (!res.ok) throw new Error(`${url.split('?')[0]} → ${res.status}`);
  const size = Number(res.headers.get('content-length') ?? 0);
  await c.put(url, res.clone());
  return size;
}
