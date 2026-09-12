/**
 * The loading-state invariant, for opens that overlap.
 *
 * Written against the seam rather than against React on purpose: the bug is not
 * in a component, it is in the rule about who is allowed to say the reading view
 * has stopped waiting. Stated once here, it is the same rule for a chapter
 * picked from the drawer, a book being opened, a live `position` event following
 * another device, and the optimistic first paint.
 */
import {describe, expect, test} from 'vitest';
import {openSequence} from './opening';

/** A `chapterLoading` flag, and every value it was ever set to. */
const flag = () => {
  const set: boolean[] = [];
  return {set, on: () => set[set.length - 1] ?? false,
          setLoading: (v: boolean) => { set.push(v); }};
};

describe('openSequence', () => {
  test('one open: it holds the wait, and it releases it', () => {
    const f = flag();
    const seq = openSequence(f.setLoading);
    const a = seq.begin();
    expect(f.on()).toBe(true);
    expect(a.current()).toBe(true);
    a.settle();
    expect(f.on()).toBe(false);
  });

  test('the newest open owns the view; the older one paints nothing', () => {
    const f = flag();
    const seq = openSequence(f.setLoading);
    const first = seq.begin();
    const second = seq.begin();
    expect(first.current()).toBe(false);
    expect(second.current()).toBe(true);
  });

  /*
   * The stuck skeleton, as a test. The old guard compared refs and returned
   * without clearing the flag, so when the superseded open was the *last* to
   * settle the reading view kept its skeleton with no words and no message.
   */
  test('a straggler settling last leaves the flag where the winner left it', () => {
    const f = flag();
    const seq = openSequence(f.setLoading);
    const slow = seq.begin();
    const fast = seq.begin();
    fast.settle();                       // the drawer's pick paints
    expect(f.on()).toBe(false);
    slow.settle();                       // ...and the one it replaced finally returns
    expect(f.on()).toBe(false);
  });

  test('a straggler settling first does not release the wait the winner holds', () => {
    const f = flag();
    const seq = openSequence(f.setLoading);
    const slow = seq.begin();
    const fast = seq.begin();
    slow.settle();
    expect(f.on()).toBe(true);           // the fast one is still working
    fast.settle();
    expect(f.on()).toBe(false);
  });

  /**
   * The invariant itself, over every order the opens can finish in: whichever
   * way they interleave, the flag ends up false. That is the whole guarantee -
   * a reader is never left on a skeleton because two taps overlapped.
   */
  test('however many opens overlap, and in whatever order they finish', () => {
    const orders = [
      [0, 1, 2], [2, 1, 0], [1, 0, 2], [0, 2, 1], [2, 0, 1], [1, 2, 0],
    ];
    for (const order of orders) {
      const f = flag();
      const seq = openSequence(f.setLoading);
      const opens = [seq.begin(), seq.begin(), seq.begin()];
      for (const i of order) opens[i].settle();
      expect(f.on()).toBe(false);
    }
  });

  test('a new open after everything settled starts waiting again', () => {
    const f = flag();
    const seq = openSequence(f.setLoading);
    seq.begin().settle();
    const next = seq.begin();
    expect(f.on()).toBe(true);
    expect(next.current()).toBe(true);
    next.settle();
    expect(f.on()).toBe(false);
  });

  /*
   * The one deliberate exception, spelled out so it cannot be broken by
   * accident: state.tsx's optimistic `cacheOnly` paint returns on a miss
   * *without* settling, because the real open is already behind it and owns the
   * wait. The invariant survives because that real open is a later `begin()`.
   */
  test('an attempt that never settles is harmless once a later one does', () => {
    const f = flag();
    const seq = openSequence(f.setLoading);
    seq.begin();                          // the optimistic paint: misses, returns
    const real = seq.begin();
    real.settle();
    expect(f.on()).toBe(false);
  });
});
