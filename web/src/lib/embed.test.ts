import {describe, expect, test} from 'vitest';
import {parseMessage, parseOpen, trustedOrigin, withoutOpen} from './embed';

describe('parseOpen', () => {
  test('a book, a chapter and a chunk', () => {
    expect(parseOpen('?open=Lord%20of%20Mysteries.epub&chapter=847&chunk=12'))
      .toEqual({book: 'Lord of Mysteries.epub', chapter: 847, chunk: 12});
  });
  test('a book alone resumes wherever it was left', () => {
    expect(parseOpen('?open=a.epub')).toEqual({book: 'a.epub', chapter: null, chunk: 0});
  });
  test('nonsense positions are dropped, not guessed at', () => {
    expect(parseOpen('?open=a.epub&chapter=-1&chunk=x'))
      .toEqual({book: 'a.epub', chapter: null, chunk: 0});
    expect(parseOpen('?open=a.epub&chapter=1.5')).toMatchObject({chapter: null});
  });
  test('no request is no request', () => {
    expect(parseOpen('')).toBeNull();
    expect(parseOpen('?open=')).toBeNull();
    expect(parseOpen('?device=x')).toBeNull();
  });
});

test('withoutOpen keeps everything else', () => {
  expect(withoutOpen('https://h/?open=a.epub&chapter=1&chunk=2&x=1#y')).toBe('/?x=1#y');
  expect(withoutOpen('https://h/?open=a.epub')).toBe('/');
});

describe('parseMessage', () => {
  test('open', () => {
    expect(parseMessage({type: 'hearsay:open', book: 'a.epub', chapter: 3, chunk: 4}))
      .toEqual({type: 'hearsay:open', book: 'a.epub', chapter: 3, chunk: 4});
    expect(parseMessage({type: 'hearsay:open', book: 'a.epub'}))
      .toEqual({type: 'hearsay:open', book: 'a.epub', chapter: null, chunk: 0});
  });
  test('where', () => {
    expect(parseMessage({type: 'hearsay:where', id: 'q1'})).toEqual({type: 'hearsay:where', id: 'q1'});
  });
  test('anything else is ignored', () => {
    for (const d of [null, 'hearsay:where', 42, {}, {type: 'hearsay:where'},
                     {type: 'hearsay:open'}, {type: 'hearsay:open', book: ''},
                     {type: 'other', book: 'a.epub'}])
      expect(parseMessage(d)).toBeNull();
  });
});

describe('trustedOrigin', () => {
  const self = 'https://reader.example';
  test('this page and Obsidian', () => {
    expect(trustedOrigin(self, self)).toBe(true);
    expect(trustedOrigin('app://obsidian.md', self)).toBe(true);
    expect(trustedOrigin('capacitor://localhost', self)).toBe(true);
  });
  test('nobody else', () => {
    expect(trustedOrigin('https://evil.example', self)).toBe(false);
    expect(trustedOrigin('null', self)).toBe(false);
  });
});
