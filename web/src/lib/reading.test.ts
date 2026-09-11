import {describe, expect, test} from 'vitest';
import {centerTop, firstVisible, isVisible, scrollPick, type Span} from './reading';

/** Ten chunks, 100px tall each, in a 400px viewport. */
const spans: Span[] = Array.from({length: 10}, (_, i) => ({top: i * 100, height: 100}));
const view = (scrollTop: number) => ({scrollTop, viewHeight: 400});
const box = {viewHeight: 400, maxScroll: 600};

describe('centerTop', () => {
  test('puts the chunk on the middle of the viewport, not the top', () => {
    // chunk 5 spans 500..600, centre 550; viewport centre is 200 in
    expect(centerTop(spans[5], box)).toBe(350);
  });

  test('clamps at both ends instead of scrolling past them', () => {
    expect(centerTop(spans[0], box)).toBe(0);
    expect(centerTop(spans[9], box)).toBe(600);
  });

  test('a page shorter than the viewport never scrolls', () => {
    expect(centerTop(spans[1], {viewHeight: 400, maxScroll: 0})).toBe(0);
  });
});

describe('isVisible', () => {
  test('overlap counts, edges do not once padded', () => {
    expect(isVisible(spans[3], view(300))).toBe(true);     // 300..400 at the top
    expect(isVisible(spans[7], view(300))).toBe(false);    // 700..800, below
    expect(isVisible(spans[3], view(300), 40)).toBe(true);
    expect(isVisible(spans[2], view(300), 40)).toBe(false); // ends exactly at the edge
  });
});

describe('firstVisible', () => {
  test('finds the first chunk the reader can see', () => {
    expect(firstVisible(spans, view(0))).toBe(0);
    expect(firstVisible(spans, view(250))).toBe(2);
    expect(firstVisible(spans, view(250), 60)).toBe(3);
  });

  test('survives unmeasured chunks and a scroll past the end', () => {
    const holes: (Span | null)[] = [...spans];
    holes[2] = null;
    expect(firstVisible(holes, view(250))).toBe(3);
    expect(firstVisible(spans, view(99999))).toBe(9);
  });
});

describe('scrollPick - bug 4: a click must not snap back', () => {
  test('scrolling while the active chunk is still on screen changes nothing', () => {
    // the reader clicked chunk 3, then nudged the page a little
    expect(scrollPick({active: 3, spans, view: view(280), playing: false})).toBeNull();
  });

  test('scrolling away moves the position to what is now in view', () => {
    expect(scrollPick({active: 3, spans, view: view(700), playing: false})).toBe(7);
  });

  test('playback owns the position: scrolling never sets it', () => {
    expect(scrollPick({active: 3, spans, view: view(700), playing: true})).toBeNull();
  });

  test('no move when the answer is where we already are', () => {
    expect(scrollPick({active: 7, spans, view: view(700), playing: false})).toBeNull();
  });

  test('an unmeasured active chunk still resolves to something visible', () => {
    const holes: (Span | null)[] = [...spans];
    holes[3] = null;
    expect(scrollPick({active: 3, spans: holes, view: view(700), playing: false})).toBe(7);
  });
});
