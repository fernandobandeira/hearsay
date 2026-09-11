import {expect, test} from 'vitest';
import {delayFor, DEFAULT, isRetryable} from './backoff';

test('delays grow exponentially and stop at the cap', () => {
  const one = () => 1;                       // no jitter: the top of each window
  const d = [0, 1, 2, 3, 4, 5, 6, 7].map((a) => delayFor(a, {}, one));
  expect(d.slice(0, 4)).toEqual([400, 800, 1600, 3200]);
  expect(d.every((v, i) => i === 0 || v >= d[i - 1])).toBe(true);
  expect(Math.max(...d)).toBe(DEFAULT.max);
});

test('jitter spreads retries instead of synchronising them', () => {
  expect(delayFor(3, {}, () => 0)).toBe(1600);
  expect(delayFor(3, {}, () => 1)).toBe(3200);
  expect(delayFor(3, {jitter: 0}, () => 0)).toBe(3200);
});

test('only failures worth repeating are retried', () => {
  expect(isRetryable(0)).toBe(true);            // no response at all
  for (const s of [500, 502, 503, 408, 429]) expect(isRetryable(s)).toBe(true);
  for (const s of [200, 400, 404]) expect(isRetryable(s)).toBe(false);
});
