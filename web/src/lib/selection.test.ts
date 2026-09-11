import {describe, expect, test} from 'vitest';
import {chosen, idle, rangeAfter, reduce, type SelectionState} from './selection';

const all = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
const dl = (s: SelectionState = idle) => reduce(s, {t: 'start', verb: 'download'});

describe('the selection mode - what a tap on a row means', () => {
  test('there is no selection until a verb starts one', () => {
    expect(idle.verb).toBeNull();
    expect(idle.picked.size).toBe(0);
    // a tap outside a mode is the caller's business (it opens the chapter) and
    // must not quietly start selecting
    expect(reduce(idle, {t: 'toggle', ci: 3, eligible: all})).toBe(idle);
    expect(reduce(idle, {t: 'pick', cis: [1, 2], eligible: all})).toBe(idle);
  });

  test('starting a verb clears whatever the last one had', () => {
    const picked = reduce(dl(), {t: 'toggle', ci: 4, eligible: all});
    const rm = reduce(picked, {t: 'start', verb: 'remove'});
    expect(rm.verb).toBe('remove');
    expect(rm.picked.size).toBe(0);
  });

  test('starting the verb already running is a no-op, not a reset', () => {
    const picked = reduce(dl(), {t: 'toggle', ci: 4, eligible: all});
    expect(reduce(picked, {t: 'start', verb: 'download'})).toBe(picked);
  });

  test('a row toggles on and off', () => {
    let s = dl();
    s = reduce(s, {t: 'toggle', ci: 2, eligible: all});
    s = reduce(s, {t: 'toggle', ci: 5, eligible: all});
    expect(chosen(s)).toEqual([2, 5]);
    s = reduce(s, {t: 'toggle', ci: 2, eligible: all});
    expect(chosen(s)).toEqual([5]);
  });

  test('an ineligible row cannot be picked - download skips what is here', () => {
    const eligible = [1, 2, 3];             // 0 is already on the device
    const s = reduce(dl(), {t: 'toggle', ci: 0, eligible});
    expect(s.picked.size).toBe(0);
    expect(chosen(reduce(s, {t: 'toggle', ci: 2, eligible}))).toEqual([2]);
  });

  test('a bulk pick replaces rather than accumulates', () => {
    let s = reduce(dl(), {t: 'toggle', ci: 9, eligible: all});
    s = reduce(s, {t: 'pick', cis: [1, 2, 3], eligible: all});
    expect(chosen(s)).toEqual([1, 2, 3]);
  });

  test('a bulk pick is filtered by eligibility too', () => {
    const s = reduce(dl(), {t: 'pick', cis: [1, 2, 3, 4], eligible: [2, 4]});
    expect(chosen(s)).toEqual([2, 4]);
  });

  test('clear keeps the mode, cancel leaves it', () => {
    const s = reduce(dl(), {t: 'pick', cis: [1, 2], eligible: all});
    const cleared = reduce(s, {t: 'clear'});
    expect([cleared.verb, cleared.picked.size]).toEqual(['download', 0]);
    expect(reduce(s, {t: 'cancel'})).toEqual(idle);
  });

  test('chosen is always in chapter order, whatever order they were tapped', () => {
    let s = dl();
    for (const ci of [7, 1, 4]) s = reduce(s, {t: 'toggle', ci, eligible: all});
    expect(chosen(s)).toEqual([1, 4, 7]);
  });
});

describe('rangeAfter - the bulk options', () => {
  test('next N starts after the chapter being read, not at it', () => {
    expect(rangeAfter(all, 3, 2)).toEqual([4, 5]);
    expect(rangeAfter(all, 3, 5)).toEqual([4, 5, 6, 7, 8]);
  });

  test('null means everything after it', () => {
    expect(rangeAfter(all, 7, null)).toEqual([8, 9]);
  });

  test('it counts eligible chapters, so "next 3 to download" skips the ones held', () => {
    const eligible = [1, 5, 6, 9];          // the rest are already downloaded
    expect(rangeAfter(eligible, 2, 3)).toEqual([5, 6, 9]);
    expect(rangeAfter(eligible, 0, 2)).toEqual([1, 5]);
  });

  test('running off the end of the book gives what there is, not an error', () => {
    expect(rangeAfter(all, 8, 20)).toEqual([9]);
    expect(rangeAfter(all, 9, 5)).toEqual([]);
    expect(rangeAfter([], 0, 5)).toEqual([]);
  });

  test('no chapter open yet means the whole book is "after"', () => {
    expect(rangeAfter(all, null, 3)).toEqual([0, 1, 2]);
    expect(rangeAfter(all, undefined, null)).toEqual(all);
  });

  test('a nonsense count is empty, never the whole book', () => {
    expect(rangeAfter(all, 0, 0)).toEqual([]);
    expect(rangeAfter(all, 0, -5)).toEqual([]);
  });

  test('the result is a fresh array - the caller may not mutate the source', () => {
    const src = [1, 2, 3];
    const out = rangeAfter(src, 0, null);
    out.push(99);
    expect(src).toEqual([1, 2, 3]);
  });
});
