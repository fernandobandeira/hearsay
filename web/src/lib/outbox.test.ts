import {describe, expect, test} from 'vitest';
import {afterFailure, classify, MAX_TRIES, nextAction, summary, type Memo} from './outbox';

const memo = (o: Partial<Memo> = {}): Memo => ({
  blob: {size: 2048} as Blob, mime: 'audio/webm', book: 'A Book.epub',
  chapter: 3, chunk: 7, ts: 0, tries: 0, ...o,
});

describe('when a memo is sent', () => {
  test('when the server is reachable and holding its book', () => {
    expect(nextAction(memo(), {online: true, serverBook: 'A Book.epub'})).toBe('send');
    expect(nextAction(memo({book: null}), {online: true, serverBook: 'Other.epub'})).toBe('send');
  });

  test('offline it waits - it is never dropped', () => {
    expect(nextAction(memo(), {online: false, serverBook: 'A Book.epub'})).toBe('hold');
  });

  test('it waits for its own book rather than quoting the wrong one', () => {
    // The server builds the note's quote callout from the chapter *it* has open.
    expect(nextAction(memo(), {online: true, serverBook: 'Different.epub'})).toBe('hold');
  });

  test('after enough failures it stalls, and a manual retry un-stalls it', () => {
    const tired = memo({tries: MAX_TRIES});
    expect(nextAction(tired, {online: true, serverBook: 'A Book.epub'})).toBe('stalled');
    expect(nextAction(tired, {online: true, serverBook: 'A Book.epub', manual: true})).toBe('send');
  });
});

describe('what counts as delivered', () => {
  test('only a 2xx carrying the filed note', () => {
    expect(classify(200, {ok: true, file: '202609111706 a thought.md'})).toBe('delivered');
    expect(classify(201, {file: 'x.md'})).toBe('delivered');
  });

  test('a 2xx with no note is not delivery - most likely a proxy answering', () => {
    expect(classify(200, {ok: true})).toBe('retry');
    expect(classify(200, null)).toBe('retry');
  });

  test('transport and server failures retry; a 4xx does not', () => {
    expect(classify(0, null)).toBe('retry');
    expect(classify(500, {error: 'transcription failed'})).toBe('retry');
    expect(classify(429, null)).toBe('retry');
    expect(classify(400, {error: 'heard nothing'})).toBe('reject');
    expect(classify(404, null)).toBe('reject');
  });

  test('a rejected memo is still kept, with the reason recorded', () => {
    const r = afterFailure(memo({tries: 2}), 'heard nothing');
    expect(r.tries).toBe(3);
    expect(r.err).toBe('heard nothing');
    expect(r.blob.size).toBe(2048);   // the recording itself is never touched
    expect(r.chapter).toBe(3);        // nor the position it was recorded at
  });
});

test('the badge says what is waiting, and nothing when nothing is', () => {
  expect(summary([], 0, true)).toBe('');
  expect(summary([memo()], 0, true)).toBe('1 note queued');
  expect(summary([memo(), memo()], 1, true)).toBe('2 notes queued · position queued');
  expect(summary([], 0, false)).toBe('offline');
  expect(summary([memo({tries: MAX_TRIES})], 0, true)).toBe('1 note queued · 1 stalled');
});
