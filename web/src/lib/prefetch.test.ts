import {describe, expect, test} from 'vitest';
import {ChunkPrefetcher} from './prefetch';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** A server behind a tunnel: every chunk costs `latency`, and chunks at or past
 *  `renderedUpTo` are not rendered yet and 404. */
function fakeServer({latency = 0, renderedUpTo = 1e9} = {}) {
  let frontier = renderedUpTo;
  const calls: string[] = [];
  const fetchFn = (async (url: string) => {
    calls.push(url);
    await sleep(latency);
    const i = Number(/\/(\d+)\.wav$/.exec(url)?.[1]);
    if (i >= frontier) return new Response('{"error":"not ready"}', {status: 404});
    return new Response(new Uint8Array(1024), {status: 200});
  }) as unknown as typeof fetch;
  return {fetchFn, calls, render: (n: number) => { frontier = n; }};
}

function make(fetchFn: typeof fetch, o: Partial<{ahead: number; retryMs: number}> = {}) {
  let n = 0;
  const live = new Set<string>();
  const pf = new ChunkPrefetcher({
    fetch: fetchFn,
    createURL: () => { const u = `blob:${++n}`; live.add(u); return u; },
    revokeURL: (u) => { live.delete(u); },
    ...o,
  });
  return {pf, live};
}

describe('the boundary gap', () => {
  test('a primed chunk is handed over with no network at the boundary', async () => {
    const srv = fakeServer({latency: 120});
    const {pf} = make(srv.fetchFn);
    pf.prime(0, 0, 20);
    await sleep(400);                     // the buffer fills while chunk 0 plays

    const t0 = performance.now();
    const url = await pf.take(0, 1);
    const cost = performance.now() - t0;

    expect(url).toBeTruthy();
    expect(cost).toBeLessThan(20);        // ~0 ms, not ~120 ms
    expect(pf.ready(0, 2) && pf.ready(0, 3)).toBe(true);
  });

  test('without a prime, the same boundary costs a full round trip', async () => {
    const srv = fakeServer({latency: 120});
    const {pf} = make(srv.fetchFn);
    const t0 = performance.now();
    await pf.take(0, 1);
    expect(performance.now() - t0).toBeGreaterThanOrEqual(100);
  });
});

test('priming is idempotent - one fetch per chunk however often it is called', async () => {
  const srv = fakeServer({latency: 5});
  const {pf} = make(srv.fetchFn, {ahead: 2});
  for (let k = 0; k < 10; k++) pf.prime(0, 0, 50);
  await sleep(60);
  expect([...srv.calls].sort()).toEqual([
    '/api/chunk/0/0.wav', '/api/chunk/0/1.wav', '/api/chunk/0/2.wav',
  ]);
});

test('a chunk the renderer has not reached is a wait, not an error', async () => {
  const srv = fakeServer({renderedUpTo: 2});
  const {pf} = make(srv.fetchFn, {retryMs: 30});
  pf.prime(0, 0, 10);
  await sleep(20);
  expect(await pf.take(0, 2)).toBeNull();

  srv.render(10);                         // the renderer catches up
  await sleep(40);                        // ...past the cooldown
  pf.prime(0, 0, 10);
  await sleep(20);
  expect(await pf.take(0, 2)).toBeTruthy();
});

test('a missing chunk is re-probed on a cooldown, not hammered', async () => {
  const srv = fakeServer({renderedUpTo: 0});
  const {pf} = make(srv.fetchFn, {retryMs: 10_000});
  for (let k = 0; k < 5; k++) { pf.prime(0, 0, 3); await sleep(5); }
  expect(srv.calls.filter((u) => u.endsWith('/0.wav')).length).toBeLessThanOrEqual(2);
});

test('the buffer is bounded and releases what the playhead has passed', async () => {
  const srv = fakeServer();
  const {pf, live} = make(srv.fetchFn, {ahead: 2});
  for (let i = 0; i < 8; i++) { pf.prime(0, i, 20); await sleep(5); }
  expect(live.size).toBeLessThanOrEqual(5);
  expect(pf.ready(0, 0)).toBe(false);
});

test('changing chapter frees every blob', async () => {
  const srv = fakeServer();
  const {pf, live} = make(srv.fetchFn);
  pf.prime(3, 0, 10);
  await sleep(20);
  expect(live.size).toBeGreaterThan(0);
  pf.drop();
  expect(live.size).toBe(0);
  expect(pf.bytes).toBe(0);
});

test('a transport failure is a miss, not a crash', async () => {
  const boom = (async () => { throw new Error('ECONNRESET'); }) as unknown as typeof fetch;
  const {pf} = make(boom);
  expect(await pf.take(0, 0)).toBeNull();
});
