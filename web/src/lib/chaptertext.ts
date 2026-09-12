/**
 * One chapter's words, by the cheapest route that can answer.
 *
 * Order matters, and it is the fix for the blank screen: a shard already in hand
 * is free, the chapter's own endpoint is one small request, and the shard fetch -
 * up to 1.5 MB - is the offline fallback, not the front door. The 17 MB whole-book
 * bundle is never on this path at all; it downloads behind the reader.
 *
 * `cacheOnly` forbids everything the loaded-book endpoint could get wrong: it is
 * used before the server has confirmed which book it holds.
 *
 * The three routes arrive as a `TextSources` seam rather than as a query client,
 * because the order they are tried in is the whole logic and it is worth a test:
 * the bug this file was extracted for was the *second* route hanging forever
 * offline, so the third one - the shard already on the device - was never
 * reached. See lib/backoff.ts's `retryWhileOnline`; the adapter in state.tsx is
 * what applies it.
 */
import type {BookIndex, TextShard} from './types';

/** What the reading view needs, whichever route produced it. */
export interface ChapterWords {
  title: string;
  chunks: string[];
  paras?: number[] | null;
}

/** Which shard holds a chapter's words, and what that chapter is called. */
export interface ShardRef {
  shard: number;
  title: string;
}

export interface TextSources {
  /** A shard already in memory. Free, and synchronous. */
  heldShard: (shard: number) => TextShard | undefined;
  /** `/api/chapter/{ci}` - one small request. Null on any failure. */
  oneChapter: () => Promise<ChapterWords | null>;
  /** The shard itself. Offline this comes straight out of Cache Storage. */
  fetchShard: (shard: number) => Promise<TextShard | null>;
}

/**
 * Where this chapter's words live, according to the book index - or null if the
 * index cannot say. An index for *another* book maps shards to the wrong words
 * entirely, so the key is checked rather than assumed.
 */
export function shardOf(
  index: BookIndex | undefined, key: string, ci: number,
): ShardRef | null {
  if (!index || index.key !== key) return null;
  const meta = index.chapters.find((c) => c.i === ci);
  if (!meta || meta.shard == null) return null;
  return {shard: meta.shard, title: meta.title ?? ''};
}

/**
 * Which chapters this device can produce the words for with no network at all.
 *
 * The whole-book text arrives in shards, and a shard that never made it onto the
 * device takes every chapter in it with it - on the 1433-chapter novel that is a
 * hundred-odd chapters at a time. The reader had no way to say so: every row in
 * the chapter drawer looked equally openable, and offline the ones whose shard
 * was missing were a dead tap with nothing to explain it. The index already
 * carries `shard` per chapter and Cache Storage already knows which shards are
 * here, so the answer costs one pass over the table of contents.
 *
 * `null` means "the index cannot say" - it is absent, or it belongs to another
 * book and maps shards to the wrong words. Not knowing is not the same as not
 * having, and a row must never claim either one on a guess.
 *
 * Note what this deliberately does *not* know: a chapter read before the network
 * went away is in the chapter cache and opens fine whether or not its shard is
 * here. So a chapter missing from this set is "may not open", never "cannot".
 */
export function chaptersWithText(
  index: BookIndex | undefined, key: string, held: ReadonlySet<number>,
): Set<number> | null {
  if (!index || index.key !== key) return null;
  const out = new Set<number>();
  for (const c of index.chapters) if (c.shard != null && held.has(c.shard)) out.add(c.i);
  return out;
}

const fromShard = (
  s: TextShard | null | undefined, ci: number, ref: ShardRef,
): ChapterWords | null => {
  const c = s?.chapters.find((x) => x.i === ci);
  return c ? {title: ref.title, chunks: c.chunks, paras: c.paras} : null;
};

export async function loadChapterText(
  src: TextSources, ci: number, ref: ShardRef | null,
  serverHasBook: boolean, cacheOnly = false,
): Promise<ChapterWords | null> {
  if (ref) {
    const held = fromShard(src.heldShard(ref.shard), ci, ref);
    if (held) return held;
  }
  if (serverHasBook) {
    const one = await src.oneChapter();
    if (one) return one;
  }
  if (ref) {
    // Offline this comes straight out of Cache Storage: local, and book-scoped.
    const c = fromShard(await src.fetchShard(ref.shard), ci, ref);
    if (c) return c;
  }
  if (cacheOnly) return null;
  // Last resort: the endpoint, even though the server may hold another book.
  return src.oneChapter();
}
