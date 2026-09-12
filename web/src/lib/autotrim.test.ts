import {describe, expect, test} from 'vitest';
import {chaptersToTrim, furthestReached, KEEP_BEHIND} from './autotrim';

const held = (...cs: number[]) => new Set(cs);

describe('the window', () => {
  test('keeps the furthest chapter and the two behind it, gives back the rest', () => {
    expect(chaptersToTrim(held(504, 505, 506, 507, 508, 509), 509))
      .toEqual([504, 505, 506]);
  });

  test('the default window is two chapters behind', () => {
    expect(KEEP_BEHIND).toBe(2);
    expect(chaptersToTrim(held(7, 8, 9, 10), 10))
      .toEqual(chaptersToTrim(held(7, 8, 9, 10), 10, 2));
  });

  test('the edge itself survives: F-2 is kept, F-3 is not', () => {
    expect(chaptersToTrim(held(6, 7, 8, 9), 9)).toEqual([6]);
  });

  test('a wider window keeps more', () => {
    expect(chaptersToTrim(held(0, 1, 2, 3, 4, 5), 5, 4)).toEqual([0]);
  });

  test('nothing ahead of the anchor is ever touched', () => {
    // downloaded ahead is the whole point of downloading ahead
    expect(chaptersToTrim(held(10, 11, 12, 13, 20), 11)).toEqual([]);
  });

  test('early in a book there is nothing behind to give back', () => {
    for (const f of [0, 1, 2]) expect(chaptersToTrim(held(0, 1, 2), f)).toEqual([]);
  });

  test('the result is in reading order, whatever order the cache reported', () => {
    expect(chaptersToTrim(held(9, 1, 4, 0, 7), 20)).toEqual([0, 1, 4, 7, 9]);
  });

  test('only what is actually downloaded is named', () => {
    expect(chaptersToTrim(held(3), 100)).toEqual([3]);
    expect(chaptersToTrim(held(), 100)).toEqual([]);
  });
});

describe('what must survive', () => {
  test('the chapter in hand is kept even when it is far behind', () => {
    // he went back to re-read chapter 12 with the anchor at 509
    expect(chaptersToTrim(held(10, 11, 12, 13), 509, KEEP_BEHIND, [12]))
      .toEqual([10, 11, 13]);
  });

  test('several protected chapters are all kept', () => {
    expect(chaptersToTrim(held(1, 2, 3, 4), 50, KEEP_BEHIND, [2, 4])).toEqual([1, 3]);
  });

  test('nonsense in the cache listing is ignored rather than deleted', () => {
    expect(chaptersToTrim([1.5, -3, Number.NaN, 2], 50)).toEqual([2]);
  });
});

describe('the anchor', () => {
  test('moves forward with the reading', () => {
    expect(furthestReached(508, 509)).toBe(509);
  });

  test('never moves back when he re-reads', () => {
    expect(furthestReached(509, 12)).toBe(509);
  });

  test('starts wherever the reader first lands', () => {
    expect(furthestReached(null, 509)).toBe(509);
    expect(furthestReached(undefined, 0)).toBe(0);
  });

  test('survives a corrupt or missing stored value', () => {
    expect(furthestReached(Number.NaN, 4)).toBe(4);
    expect(furthestReached(-7, 4)).toBe(4);
    expect(furthestReached('nonsense' as unknown as number, 4)).toBe(4);
  });

  test('re-reading cannot widen the trim: the same chapters stay held', () => {
    const anchor = furthestReached(furthestReached(509, 12), 13);
    expect(anchor).toBe(509);
    expect(chaptersToTrim(held(507, 508, 509), anchor, KEEP_BEHIND, [13])).toEqual([]);
  });

  test('walking a book forward trims exactly one chapter per chapter read', () => {
    // the shape of a real session: download ahead, read on, and what is left
    // behind each arrival is the single chapter that just fell out of the window
    let anchor = 0;
    const downloaded = new Set([0, 1, 2, 3, 4, 5]);
    const gone: number[] = [];
    for (const ci of [0, 1, 2, 3, 4, 5]) {
      anchor = furthestReached(anchor, ci);
      for (const c of chaptersToTrim(downloaded, anchor, KEEP_BEHIND, [ci])) {
        downloaded.delete(c);
        gone.push(c);
      }
    }
    expect(gone).toEqual([0, 1, 2]);
    expect([...downloaded]).toEqual([3, 4, 5]);   // F-2, F-1, F, and nothing else
  });
});
