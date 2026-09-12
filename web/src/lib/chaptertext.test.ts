import {describe, expect, test, vi} from 'vitest';
import {loadChapterText, shardOf, type ChapterWords, type TextSources} from './chaptertext';
import type {BookIndex, TextShard} from './types';

const index = (key: string): BookIndex => ({
  key, name: `${key}.epub`, title: key, total_min: 120, shards: 2, text_bytes: 1000,
  chapters: [
    {i: 0, title: 'One', n: 3, est_min: 4, shard: 0},
    {i: 1, title: 'Two', n: 3, est_min: 4, shard: 0},
    {i: 576, title: 'The Sea', n: 3, est_min: 4, shard: 1},
  ],
});

const shard = (s: number, ci: number): TextShard => ({
  shard: s, from: ci, to: ci,
  chapters: [{i: ci, chunks: ['a shard sentence.'], paras: [0]}],
});

const endpointText: ChapterWords = {title: 'The Sea', chunks: ['from the endpoint.'], paras: [0]};

/** Every route refuses unless a test says otherwise. */
const sources = (over: Partial<TextSources> = {}): TextSources => ({
  heldShard: () => undefined,
  oneChapter: () => Promise.resolve(null),
  fetchShard: () => Promise.resolve(null),
  ...over,
});

describe('shardOf - where a chapter\'s words live', () => {
  test('the index says which shard, and what the chapter is called', () => {
    expect(shardOf(index('lom'), 'lom', 576)).toEqual({shard: 1, title: 'The Sea'});
  });

  test("an index for another book maps shards to the wrong words: refuse it", () => {
    expect(shardOf(index('sapiens'), 'lom', 576)).toBeNull();
    expect(shardOf(undefined, 'lom', 576)).toBeNull();
  });

  test('a chapter the index does not carry, or carries without a shard', () => {
    expect(shardOf(index('lom'), 'lom', 999)).toBeNull();
    const noShard = {...index('lom'), chapters: [{i: 0, title: 'One', n: 3, est_min: 4}]};
    expect(shardOf(noShard, 'lom', 0)).toBeNull();
  });
});

describe('loadChapterText - the cheapest route that can answer', () => {
  const ref = {shard: 1, title: 'The Sea'};

  test('a shard already in memory is free: nothing is fetched at all', async () => {
    const oneChapter = vi.fn(() => Promise.resolve(endpointText));
    const fetchShard = vi.fn(() => Promise.resolve(null));
    const got = await loadChapterText(
      sources({heldShard: () => shard(1, 576), oneChapter, fetchShard}), 576, ref, true);
    expect(got).toEqual({title: 'The Sea', chunks: ['a shard sentence.'], paras: [0]});
    expect(oneChapter).not.toHaveBeenCalled();
    expect(fetchShard).not.toHaveBeenCalled();
  });

  test('otherwise the small endpoint, before the 1.5 MB shard', async () => {
    const fetchShard = vi.fn(() => Promise.resolve(shard(1, 576)));
    const got = await loadChapterText(
      sources({oneChapter: () => Promise.resolve(endpointText), fetchShard}), 576, ref, true);
    expect(got).toBe(endpointText);
    expect(fetchShard).not.toHaveBeenCalled();
  });

  /*
   * The bug, as a test. Offline the endpoint has nothing cached for a chapter
   * never visited, so it fails - and the shard sitting in Cache Storage is the
   * whole point of having taken the book's words. It only gets reached if that
   * failure *settles*, which is what lib/backoff.ts's retryWhileOnline is for.
   */
  test('the endpoint failing falls through to the shard on the device', async () => {
    const oneChapter = vi.fn(() => Promise.resolve(null));
    const got = await loadChapterText(
      sources({oneChapter, fetchShard: () => Promise.resolve(shard(1, 576))}), 576, ref, true);
    expect(got).toEqual({title: 'The Sea', chunks: ['a shard sentence.'], paras: [0]});
    expect(oneChapter).toHaveBeenCalledTimes(1);
  });

  test('a shard that does not carry this chapter is not an answer', async () => {
    const got = await loadChapterText(
      sources({fetchShard: () => Promise.resolve(shard(1, 42))}), 576, ref, false);
    expect(got).toBeNull();
  });

  test('cacheOnly never asks the endpoint - the server may hold another book', async () => {
    const oneChapter = vi.fn(() => Promise.resolve(endpointText));
    const got = await loadChapterText(sources({oneChapter}), 576, ref, false, true);
    expect(got).toBeNull();
    expect(oneChapter).not.toHaveBeenCalled();
  });

  test('with no shard to fall back to, the endpoint is the last resort too', async () => {
    const oneChapter = vi.fn(() => Promise.resolve(endpointText));
    const got = await loadChapterText(sources({oneChapter}), 576, null, false);
    expect(got).toBe(endpointText);
    expect(oneChapter).toHaveBeenCalledTimes(1);
  });

  test('everything refusing is null, not a hang', async () => {
    await expect(loadChapterText(sources(), 576, ref, true)).resolves.toBeNull();
  });
});
