/**
 * The hand-written half of api.ts: the URL builders (which are also this
 * reader's Cache Storage keys) and the retry policy the awaited fallback chains
 * depend on. The calls themselves are generated and need no test here.
 */
import {onlineManager} from '@tanstack/react-query';
import {afterEach, expect, test} from 'vitest';
import {
  ApiError, awaitedRetry, bookIndexUrl, chapterAudioUrl, chapterTextUrl, textShardUrl,
} from './api';
import {MAX_RETRIES} from './backoff';

afterEach(() => onlineManager.setOnline(true));

test('every cacheable URL names its book, so one book cannot answer for another', () => {
  expect(bookIndexUrl('01 - Lord of Mysteries'))
    .toBe('/api/book.json?book=01%20-%20Lord%20of%20Mysteries');
  expect(textShardUrl('lom', 3)).toBe('/api/text/3.json?book=lom');
  expect(chapterTextUrl('lom', 576)).toBe('/api/chapter/576?book=lom');
  expect(chapterAudioUrl('lom', 576)).toBe('/api/chapters/576.m4a?book=lom');
  expect(bookIndexUrl(null)).toBe('/api/book.json?book=');
});

test('awaitedRetry follows the browser: offline it settles, online it repeats', () => {
  const gone = new ApiError(0, 'no answer');
  onlineManager.setOnline(true);
  expect(awaitedRetry(0, gone)).toBe(true);
  expect(awaitedRetry(MAX_RETRIES, gone)).toBe(false);
  expect(awaitedRetry(0, new ApiError(404, 'no such chapter'))).toBe(false);

  // Offline, a retry would be *paused* rather than run - and a paused query's
  // promise never settles, which is the chapter that never paints in airplane
  // mode. Giving up here is what lets the caller reach the cached shard.
  onlineManager.setOnline(false);
  expect(awaitedRetry(0, gone)).toBe(false);
});
