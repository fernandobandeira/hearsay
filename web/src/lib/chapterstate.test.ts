import {describe, expect, test} from 'vitest';
import {chapterState, textMark} from './chapterstate';
import type {ChapRow} from './types';

const row = (p: Partial<ChapRow> = {}): ChapRow => ({
  i: 4, title: '5: The Sequence', n: 76, est_min: 12.2, est_bytes: 5_856_000,
  rendered: 0, m4a: false, bytes: null, duration: null,
  queued: false, packing: false, pack_queued: false, ...p,
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

/**
 * The other tier on the row. It appears only as an absence, and what the absence
 * means depends on whether there is a network standing behind it.
 */
describe('textMark - whether this chapter\'s words are on the device', () => {
  test('the words being here is the norm, so it says nothing at all', () => {
    expect(textMark(true, true)).toBeNull();
    expect(textMark(true, false)).toBeNull();
  });

  test('not knowing is not the same as not having', () => {
    expect(textMark(null, true)).toBeNull();
    expect(textMark(null, false)).toBeNull();
  });

  test('online, missing words are a fetch away: quiet, in the text tier\'s own icon', () => {
    const m = textMark(false, true);
    expect(m).toMatchObject({icon: 'text', tone: 'none'});
    expect(m?.tip).toMatch(/not in the saved text/);
  });

  test('offline they are what stands between the reader and the chapter', () => {
    const m = textMark(false, false);
    expect(m).toMatchObject({icon: 'no-network', tone: 'part'});
    // "may not open", never "cannot": a chapter read before the network went
    // away is in the chapter cache and opens whatever its shard did.
    expect(m?.tip).toMatch(/may not open/);
  });
});
