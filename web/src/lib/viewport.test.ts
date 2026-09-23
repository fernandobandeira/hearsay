import {describe, expect, test} from 'vitest';
import {isRotation, shouldHeal, trackBaseline, viewportShortfall} from './viewport';

describe('trackBaseline - the largest the viewport has been', () => {
  test('the first reading is the baseline', () => {
    expect(trackBaseline(null, 852)).toBe(852);
  });

  test('it never comes down, because the bug only ever shrinks', () => {
    expect(trackBaseline(852, 793)).toBe(852);
    expect(trackBaseline(852, 852)).toBe(852);
  });

  test('it does come up - a taller viewport is the honest one', () => {
    expect(trackBaseline(793, 852)).toBe(852);
  });

  test('a nonsense reading is ignored rather than believed', () => {
    expect(trackBaseline(852, 0)).toBe(852);
    expect(trackBaseline(852, Number.NaN)).toBe(852);
    expect(trackBaseline(null, 0)).toBe(0);
  });
});

describe('shouldHeal - has the keyboard left the viewport short', () => {
  test('the reported bug: a status bar gone missing and not coming back', () => {
    expect(shouldHeal({innerHeight: 873, baseline: 932})).toBe(true);
  });

  test('the usual size is not a fault', () => {
    expect(shouldHeal({innerHeight: 852, baseline: 852})).toBe(false);
  });

  test('sub-pixel and toolbar noise is under the tolerance', () => {
    expect(shouldHeal({innerHeight: 849, baseline: 852})).toBe(false);
    expect(shouldHeal({innerHeight: 843, baseline: 852})).toBe(true);
  });

  test('a taller viewport is a rotation, not something to heal', () => {
    expect(shouldHeal({innerHeight: 932, baseline: 852})).toBe(false);
  });

  test('with no baseline yet there is nothing to compare against', () => {
    expect(shouldHeal({innerHeight: 100, baseline: null})).toBe(false);
  });

  test('a nonsense reading never triggers a reflow', () => {
    expect(shouldHeal({innerHeight: 0, baseline: 852})).toBe(false);
    expect(shouldHeal({innerHeight: Number.NaN, baseline: 852})).toBe(false);
  });
});

describe('isRotation - when the baseline stops meaning anything', () => {
  test('portrait to landscape', () => {
    expect(isRotation({w: 393, h: 852}, {w: 852, h: 393})).toBe(true);
  });

  test('the keyboard is not a rotation: the width does not move', () => {
    expect(isRotation({w: 393, h: 852}, {w: 393, h: 793})).toBe(false);
  });

  test('nothing moving is not a rotation', () => {
    expect(isRotation({w: 393, h: 852}, {w: 393, h: 852})).toBe(false);
  });

  test('a desktop window widening without turning over is not one either', () => {
    expect(isRotation({w: 1200, h: 800}, {w: 1400, h: 800})).toBe(true);
  });
});

describe('viewportShortfall - what iOS took off the bottom', () => {
  // The reported case: an installed PWA on an iPhone 15 Pro, laid out from the
  // top of the screen and handed a viewport one status bar shorter than it.
  const phone = {innerWidth: 393, innerHeight: 793, screenWidth: 393, screenHeight: 852};

  test('the black band: the page is short by exactly a status bar', () => {
    expect(viewportShortfall({...phone, insetTop: 59})).toBe(59);
  });

  test('a shortfall a few px past the inset is still the same fault', () => {
    expect(viewportShortfall({...phone, innerHeight: 789, insetTop: 59})).toBe(63);
  });

  test('no top inset means the web view was inset instead: nothing is missing', () => {
    // A browser tab, or a status bar style that starts the page below the bar.
    expect(viewportShortfall({...phone, insetTop: 0})).toBe(0);
  });

  test('a viewport that already reaches the bottom gets nothing', () => {
    expect(viewportShortfall({...phone, innerHeight: 852, insetTop: 59})).toBe(0);
    expect(viewportShortfall({...phone, innerHeight: 900, insetTop: 59})).toBe(0);
  });

  test('the keyboard shrink is the other bug, and not this one to fix', () => {
    // 852 -> 734 is two status bars: shouldHeal owns that one.
    expect(viewportShortfall({...phone, innerHeight: 734, insetTop: 59})).toBe(0);
  });

  test('a window narrower than the screen is short because it is a window', () => {
    // An iPad split view, or any desktop browser.
    expect(viewportShortfall({
      innerWidth: 500, innerHeight: 700, screenWidth: 1024, screenHeight: 1366, insetTop: 24,
    })).toBe(0);
  });

  test('landscape: which way the screen is is read off the viewport', () => {
    // screen.width/height do not agree across iOS versions about turning over,
    // so 393x852 has to mean 852x393 when the viewport is the wide way round.
    expect(viewportShortfall({
      innerWidth: 852, innerHeight: 334, screenWidth: 393, screenHeight: 852, insetTop: 59,
    })).toBe(59);
    // ...and the honest landscape reading, where iOS hides the status bar and
    // there is nothing missing to give back.
    expect(viewportShortfall({
      innerWidth: 852, innerHeight: 393, screenWidth: 393, screenHeight: 852, insetTop: 0,
    })).toBe(0);
  });

  test('nonsense is never believed', () => {
    expect(viewportShortfall({...phone, innerHeight: 0, insetTop: 59})).toBe(0);
    expect(viewportShortfall({...phone, innerHeight: Number.NaN, insetTop: 59})).toBe(0);
    expect(viewportShortfall({...phone, screenHeight: 0, screenWidth: 0, insetTop: 59})).toBe(0);
  });
});
