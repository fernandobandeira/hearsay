import {describe, expect, test} from 'vitest';
import {chapterState} from './chapterstate';
import type {ChapRow} from './types';

const row = (p: Partial<ChapRow> = {}): ChapRow => ({
  i: 4, title: '5: The Sequence', n: 76, est_min: 12.2,
  rendered: 0, m4a: false, bytes: null, duration: null, ...p,
});
const fmt = (n: number) => `${n} B`;

describe('chapterState - the four things that can have happened', () => {
  test('nothing yet', () => {
    const s = chapterState(row(), false, fmt);
    expect(s.key).toBe('none');
    expect(s.text).toBe('~12m');
    expect(s.tip).toMatch(/not rendered/);
  });

  test('partly rendered says how far', () => {
    const s = chapterState(row({rendered: 12}), false, fmt);
    expect([s.key, s.text, s.tone]).toEqual(['partial', '12/76', 'part']);
    expect(s.tip).toBe('partly rendered: 12 of 76 chunks have audio');
  });

  test('fully rendered but not packed', () => {
    expect(chapterState(row({rendered: 76}), false, fmt).key).toBe('rendered');
  });

  test('packed on the server shows its size', () => {
    const s = chapterState(row({rendered: 76, m4a: true, bytes: 1234}), false, fmt);
    expect([s.key, s.text]).toEqual(['ready', '1234 B']);
  });

  test('on this device outranks everything the server says', () => {
    const s = chapterState(row({rendered: 76, m4a: true, bytes: 99, queued: true}), true, fmt);
    expect([s.key, s.tone]).toEqual(['downloaded', 'ok']);
    expect(s.tip).toMatch(/no network/);
  });

  test('queues and jobs are visible while they run', () => {
    expect(chapterState(row({queued: true}), false, fmt).key).toBe('queued');
    expect(chapterState(row({rendered: 76, pack_queued: true}), false, fmt).key).toBe('to-pack');
    expect(chapterState(row({packing: true}), false, fmt).spin).toBe(true);
    expect(chapterState(row({m4a: true}), false, fmt, 'saving').key).toBe('saving');
    expect(chapterState(row(), false, fmt, 'packing').key).toBe('packing');
  });

  test('a chapter with no estimate shows nothing rather than NaN', () => {
    expect(chapterState(row({est_min: null}), false, fmt).text).toBe('');
  });

  test('every state carries a sentence', () => {
    const all = [
      chapterState(row(), false, fmt),
      chapterState(row({rendered: 3}), false, fmt),
      chapterState(row({rendered: 76}), false, fmt),
      chapterState(row({m4a: true}), false, fmt),
      chapterState(row({queued: true}), false, fmt),
      chapterState(row({pack_queued: true}), false, fmt),
      chapterState(row({packing: true}), false, fmt),
      chapterState(row(), true, fmt),
      chapterState(row(), false, fmt, 'saving'),
    ];
    for (const s of all) expect(s.tip.length).toBeGreaterThan(10);
  });
});
