import {describe, expect, test} from 'vitest';
import {chunkAt, clampTime, isSane, startOf, type Manifest} from './manifest';

const M: Manifest = {
  book: 'b', chapter: 2, title: 'C', chunks: 5,
  starts: [0, 2.5, 7.25, 7.3, 19], duration: 22.75,
};

describe('chunk <-> time', () => {
  test('chunkAt is the inverse of starts, on and around every boundary', () => {
    M.starts.forEach((s, i) => {
      expect(chunkAt(M, s)).toBe(i);
      expect(chunkAt(M, s + 0.001)).toBe(i);
      if (i) expect(chunkAt(M, s - 0.001)).toBe(i - 1);
    });
  });

  test('adjacent starts a hair apart still resolve', () => {
    // 7.25 and 7.3 are 50 ms apart: a silent beat between two chunks.
    expect(chunkAt(M, 7.26)).toBe(2);
    expect(chunkAt(M, 7.31)).toBe(3);
  });

  test('clamps instead of returning nonsense', () => {
    expect(chunkAt(M, -10)).toBe(0);
    expect(chunkAt(M, 1e9)).toBe(4);
    expect(chunkAt(M, NaN)).toBe(0);
    expect(chunkAt({...M, starts: []}, 5)).toBe(0);
  });

  test('startOf clamps rather than yielding undefined', () => {
    expect(startOf(M, 3)).toBe(7.3);
    expect(startOf(M, 99)).toBe(19);
    expect(startOf(M, -5)).toBe(0);
    // An <audio> element given NaN silently seeks to zero; be explicit instead.
    expect(startOf(M, NaN)).toBe(0);
  });

  test('a round trip through time and back lands on the same chunk', () => {
    for (let i = 0; i < M.chunks; i++) expect(chunkAt(M, startOf(M, i))).toBe(i);
  });

  test('clampTime keeps a seek inside the chapter', () => {
    expect(clampTime(M, -1)).toBe(0);
    expect(clampTime(M, 10)).toBe(10);
    expect(clampTime(M, 1e6)).toBeLessThan(M.duration);
  });
});

describe('isSane', () => {
  test('accepts a real manifest', () => expect(isSane(M, 5)).toBe(true));

  test('rejects every way a manifest can lie', () => {
    expect(isSane(M, 6)).toBe(false);                                  // other chunking
    expect(isSane({...M, starts: [0, 5, 3]}, 3)).toBe(false);          // not ascending
    expect(isSane({...M, starts: [0, 2.5, 2.5, 7.3, 19]}, 5)).toBe(false); // repeated
    expect(isSane({...M, starts: [1, 2, 3, 4, 5]}, 5)).toBe(false);    // no zero start
    expect(isSane({...M, chunks: 4}, 4)).toBe(false);                  // disagrees
    expect(isSane({...M, duration: 1}, 5)).toBe(false);                // start past the end
    expect(isSane(null)).toBe(false);
    expect(isSane({error: 'not built'})).toBe(false);                  // a 404 body
  });
});
