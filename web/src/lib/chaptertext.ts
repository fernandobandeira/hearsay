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
