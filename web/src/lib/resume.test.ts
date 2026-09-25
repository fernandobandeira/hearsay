import {describe, expect, test} from 'vitest';
import {afterFastOpen, clampResume, parseUpdated, resolveResume, FUTURE_SLACK_MS} from './resume';

const NOW = Date.parse('2026-09-11T12:00:00');

/** exactly what `datetime.now().isoformat(timespec='seconds')` writes: local, no zone */
const iso = (ms: number) => {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`
    + `T${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
};

describe('parseUpdated', () => {
  test('reads the server naive-ISO stamp as local time', () => {
    expect(parseUpdated(iso(NOW - 60_000), NOW)).toBe(NOW - 60_000);
  });

  test('nothing is not a timestamp', () => {
    expect(parseUpdated(null, NOW)).toBeNull();
    expect(parseUpdated(undefined, NOW)).toBeNull();
    expect(parseUpdated('', NOW)).toBeNull();
    expect(parseUpdated('not a date', NOW)).toBeNull();
  });

  test('a stamp from the future is a clock difference, not news', () => {
    expect(parseUpdated(iso(NOW + FUTURE_SLACK_MS + 1000), NOW)).toBeNull();
    // inside the slack it is still trusted
    expect(parseUpdated(iso(NOW + 60_000), NOW)).toBe(NOW + 60_000);
  });
});

describe('resolveResume', () => {
  test('the server position is where a book opens', () => {
    expect(resolveResume({server: {chapter: 575, chunk: 12, updated: iso(NOW - 3600_000)}, now: NOW}))
      .toEqual({chapter: 575, chunk: 12, from: 'server'});
  });

  test('a queued position that is newer beats the server', () => {
    const r = resolveResume({
      server: {chapter: 575, chunk: 12, updated: iso(NOW - 3600_000)},
      queued: {chapter: 580, chunk: 3, ts: NOW - 60_000},
      now: NOW,
    });
    expect(r).toEqual({chapter: 580, chunk: 3, from: 'queued'});
  });

  test('a queued position that is older loses to the server', () => {
    const r = resolveResume({
      server: {chapter: 575, chunk: 12, updated: iso(NOW - 60_000)},
      queued: {chapter: 300, chunk: 3, ts: NOW - 3600_000},
      now: NOW,
    });
    expect(r).toEqual({chapter: 575, chunk: 12, from: 'server'});
  });

  test('an unstamped or skewed server record cannot outrank a queued one', () => {
    const queued = {chapter: 580, chunk: 3, ts: NOW - 3600_000};
    expect(resolveResume({server: {chapter: 1, chunk: 1}, queued, now: NOW}).from).toBe('queued');
    expect(resolveResume({
      server: {chapter: 1, chunk: 1, updated: iso(NOW + 6 * 60_000)}, queued, now: NOW,
    }).from).toBe('queued');
  });

  test('offline: the queue, then this device, then the beginning', () => {
    expect(resolveResume({queued: {chapter: 9, chunk: 2, ts: NOW}, device: {chapter: 1, chunk: 1}, now: NOW}))
      .toEqual({chapter: 9, chunk: 2, from: 'queued'});
    expect(resolveResume({device: {chapter: 4, chunk: 7}, now: NOW}))
      .toEqual({chapter: 4, chunk: 7, from: 'device'});
    expect(resolveResume({now: NOW})).toEqual({chapter: 0, chunk: 0, from: 'none'});
  });

  test('the server outranks this device: another device may have moved on', () => {
    const r = resolveResume({
      server: {chapter: 575, chunk: 12, updated: iso(NOW - 10_000)},
      device: {chapter: 2, chunk: 0}, now: NOW,
    });
    expect(r.from).toBe('server');
  });

  test('nonsense coordinates become the beginning rather than NaN', () => {
    const r = resolveResume({
      server: {chapter: NaN as unknown as number, chunk: -4, updated: iso(NOW)}, now: NOW,
    });
    expect(r).toEqual({chapter: 0, chunk: 0, from: 'server'});
  });
});

describe('clampResume', () => {
  test('a re-chunked book cannot push the position past the end', () => {
    expect(clampResume({chapter: 5000, chunk: 900, from: 'server'}, 1433, 76))
      .toEqual({chapter: 1432, chunk: 75, from: 'server'});
  });

  test('an unknown chunk count leaves the chunk alone', () => {
    expect(clampResume({chapter: 3, chunk: 12, from: 'queued'}, 10).chunk).toBe(12);
  });

  test('a book with no chapters resolves to zero', () => {
    expect(clampResume({chapter: 7, chunk: 1, from: 'device'}, 0).chapter).toBe(0);
  });
});

describe('afterFastOpen - the page painted, then the server answered', () => {
  const fast = {chapter: 40, chunk: 3};
  const server = (chapter: number, chunk: number) => ({chapter, chunk, from: 'server' as const});

  test('the phone read on: offer it, never jump to it', () => {
    expect(afterFastOpen({fast, here: fast, at: server(44, 0), serverMs: 2, deviceMs: 1}))
      .toBe('offer');
    // ahead is enough on its own, whatever the clocks say
    expect(afterFastOpen({fast, here: fast, at: server(40, 9), serverMs: 1, deviceMs: 2}))
      .toBe('offer');
  });

  test('another device moved behind, after this one wrote: still asked, not taken', () => {
    expect(afterFastOpen({fast, here: fast, at: server(12, 0), serverMs: 5, deviceMs: 1}))
      .toBe('offer');
  });

  test('behind and older is this device having read further, later', () => {
    expect(afterFastOpen({fast, here: fast, at: server(39, 0), serverMs: 1, deviceMs: 5}))
      .toBe('stay');
  });

  test('no stamp to compare is a question, not a guess', () => {
    expect(afterFastOpen({fast, here: fast, at: server(39, 0), serverMs: null, deviceMs: 5}))
      .toBe('offer');
    expect(afterFastOpen({fast, here: fast, at: server(39, 0), serverMs: 5}))
      .toBe('offer');
  });

  test('agreement, a reader who already moved, and a queued position all stay', () => {
    expect(afterFastOpen({fast, here: fast, at: server(40, 3)})).toBe('stay');
    expect(afterFastOpen({fast, here: {chapter: 41, chunk: 0}, at: server(44, 0)})).toBe('stay');
    expect(afterFastOpen({fast, here: fast, at: {chapter: 44, chunk: 0, from: 'queued'}}))
      .toBe('stay');
  });
});
