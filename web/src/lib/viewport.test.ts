import {describe, expect, test} from 'vitest';
import {isRotation, shouldHeal, trackBaseline} from './viewport';

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
