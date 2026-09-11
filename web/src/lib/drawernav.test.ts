import {describe, expect, test} from 'vitest';
import {centeredScrollTop, initialView, scrollTargetIndex} from './drawernav';

describe('initialView - what the drawer opens on', () => {
  test('a book is open, so its chapters are', () => {
    expect(initialView(true)).toBe('chapters');
  });

  test('nothing open falls back to the library', () => {
    expect(initialView(false)).toBe('books');
  });
});

describe('scrollTargetIndex - which row to centre', () => {
  const all = [0, 1, 2, 3, 4];

  test('the chapter being read, by its position in the list', () => {
    expect(scrollTargetIndex(all, 3)).toBe(3);
  });

  test('a filtered list is positional, not indexed by chapter', () => {
    // "filter chapters…" left only these four; chapter 700 is the second row.
    expect(scrollTargetIndex([12, 700, 1101, 1432], 700)).toBe(1);
    expect(scrollTargetIndex([12, 700, 1101, 1432], 1432)).toBe(3);
  });

  test('a filter that hides the current chapter asks for no scroll', () => {
    expect(scrollTargetIndex([12, 1101], 700)).toBeNull();
  });

  test('no list, no target', () => {
    expect(scrollTargetIndex([], 0)).toBeNull();
  });

  test('no chapter, no target', () => {
    expect(scrollTargetIndex(all, null)).toBeNull();
    expect(scrollTargetIndex(all, undefined)).toBeNull();
    expect(scrollTargetIndex(all, NaN)).toBeNull();
  });

  test('chapter zero is a target like any other', () => {
    expect(scrollTargetIndex(all, 0)).toBe(0);
  });
});

describe('centeredScrollTop - centred, and never past an end', () => {
  const rows = (i: number, h = 28, count = 1433) => ({
    rowTop: i * h, rowHeight: h, viewportHeight: 420, scrollHeight: count * h,
  });

  test('a row in the middle of the big book lands on the middle of the viewport', () => {
    const top = centeredScrollTop(rows(700));
    // the row's centre sits half a viewport below the scroll offset
    expect(700 * 28 + 14 - top).toBe(420 / 2);
  });

  test('the first rows cannot be centred, and do not scroll off the top', () => {
    expect(centeredScrollTop(rows(0))).toBe(0);
    expect(centeredScrollTop(rows(3))).toBe(0);
  });

  test('the last rows clamp to the bottom of the scroller', () => {
    const m = rows(1432);
    expect(centeredScrollTop(m)).toBe(1433 * 28 - 420);
  });

  test('a list shorter than the viewport never scrolls', () => {
    expect(centeredScrollTop(rows(2, 28, 6))).toBe(0);
  });

  test('fractional measurements give a whole scrollTop', () => {
    const top = centeredScrollTop({
      rowTop: 1111.5, rowHeight: 27.3, viewportHeight: 401.7, scrollHeight: 9999,
    });
    expect(top).toBe(Math.round(1111.5 + 27.3 / 2 - 401.7 / 2));
    expect(Number.isInteger(top)).toBe(true);
  });
});
