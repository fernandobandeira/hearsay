import {QueryClient, onlineManager} from '@tanstack/react-query';
import {afterEach, describe, expect, test} from 'vitest';
import {
  delayFor, DEFAULT, isRetryable, MAX_RETRIES, retryQuery, retryWhileOnline, statusOf,
} from './backoff';

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

/** Anything with a numeric `status` - ApiError, without importing it. */
class Failure extends Error {
  constructor(readonly status: number) { super(`HTTP ${status}`); }
}

test('the status comes off whatever threw, and nothing is status 0', () => {
  expect(statusOf(new Failure(503))).toBe(503);
  expect(statusOf(new TypeError('Failed to fetch'))).toBe(0);   // transport
  expect(statusOf(undefined)).toBe(0);
  expect(statusOf({status: 'nope'})).toBe(0);
});

test('the default policy: five repeats, and none for a 404', () => {
  expect(retryQuery(0, new Failure(503))).toBe(true);
  expect(retryQuery(MAX_RETRIES - 1, new Failure(503))).toBe(true);
  expect(retryQuery(MAX_RETRIES, new Failure(503))).toBe(false);
  expect(retryQuery(0, new Failure(404))).toBe(false);
});

test('offline, the awaited policy gives up at once; online it is the default', () => {
  const off = retryWhileOnline(() => false);
  const on = retryWhileOnline(() => true);
  expect(off(0, new Failure(0))).toBe(false);
  expect(on(0, new Failure(0))).toBe(true);
  expect(on(0, new Failure(404))).toBe(false);
  expect(on(MAX_RETRIES, new Failure(0))).toBe(false);
});

/*
 * The regression, driven against the real query-core rather than described.
 *
 * `networkMode: 'offlineFirst'` runs the first attempt whatever the browser
 * thinks - but a *retry* is paused while it says offline, and a paused query's
 * promise never settles. Every awaited `fetchQuery(...).catch(() => null)` in
 * the reader is a fallback chain, and a promise that never settles is a chain
 * that never reaches the copy already on the device: in airplane mode only the
 * chapter that was already open would display.
 */
describe('offlineFirst + the retry policy, against @tanstack/query-core', () => {
  afterEach(() => onlineManager.setOnline(true));

  const client = (retry: (n: number, e: unknown) => boolean) => new QueryClient({
    defaultOptions: {queries: {networkMode: 'offlineFirst', retry, retryDelay: 0}},
  });

  /** A query that always fails the way an unreachable server fails. */
  const failing = (qc: QueryClient, calls: {n: number}) => qc.fetchQuery({
    queryKey: ['gone'],
    queryFn: () => { calls.n++; return Promise.reject(new Failure(0)); },
  });

  const settledWithin = (p: Promise<unknown>, ms: number) => Promise.race([
    p.then(() => 'resolved', () => 'rejected'),
    new Promise((r) => setTimeout(() => r('pending'), ms)),
  ]);

  test('the default policy pauses offline, and the promise never settles', async () => {
    onlineManager.setOnline(false);
    const qc = client(retryQuery);
    const calls = {n: 0};
    const outcome = await settledWithin(failing(qc, calls), 60);
    expect(outcome).toBe('pending');
    expect(calls.n).toBe(1);                                   // one attempt, then paused
    expect(qc.getQueryState(['gone'])?.fetchStatus).toBe('paused');
    expect(qc.getQueryState(['gone'])?.status).toBe('pending');
    qc.clear();
  });

  test('the awaited policy settles on the first failure instead', async () => {
    onlineManager.setOnline(false);
    const qc = client(retryWhileOnline(() => onlineManager.isOnline()));
    const calls = {n: 0};
    const outcome = await settledWithin(failing(qc, calls), 60);
    expect(outcome).toBe('rejected');
    expect(calls.n).toBe(1);
    expect(qc.getQueryState(['gone'])?.fetchStatus).toBe('idle');
    qc.clear();
  });

  test('online it is still the full curve - and still nothing for a 404', async () => {
    const qc = client(retryWhileOnline(() => onlineManager.isOnline()));
    const calls = {n: 0};
    await expect(failing(qc, calls)).rejects.toBeInstanceOf(Failure);
    expect(calls.n).toBe(MAX_RETRIES + 1);                     // the try, then five

    let n = 0;
    await expect(qc.fetchQuery({
      queryKey: ['missing'],
      queryFn: () => { n++; return Promise.reject(new Failure(404)); },
    })).rejects.toBeInstanceOf(Failure);
    expect(n).toBe(1);
    qc.clear();
  });
});
