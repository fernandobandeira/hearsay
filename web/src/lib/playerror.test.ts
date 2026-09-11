import {describe, expect, test} from 'vitest';
import {
  classifyPlayError, MAX_PLAY_RETRIES, playRetry, shouldRefetch,
} from './playerror';

const domErr = (name: string) => Object.assign(new Error(`${name} message`), {name});

describe('the production failure', () => {
  test('NotSupportedError is a blip, not a wall', () => {
    // Fernando, iPhone, over the tailnet proxy: a chunk boundary lands on a
    // network blip and play() rejects NotSupportedError. The old reader stopped
    // and asked for a tap; it must heal itself instead.
    expect(classifyPlayError(domErr('NotSupportedError'))).toBe('transient');
    const plan = playRetry('transient', 1);
    expect(plan.retry).toBe(true);
    expect(plan.message).toBeNull();      // nothing is shown for the first blip
    expect(plan.delay).toBeGreaterThan(0);
    expect(plan.delay).toBeLessThanOrEqual(500);
  });

  test('a real autoplay block is the one case that needs a human', () => {
    expect(classifyPlayError(domErr('NotAllowedError'))).toBe('gesture');
    const plan = playRetry('gesture', 1);
    expect(plan.retry).toBe(false);
    expect(plan.message).toMatch(/tap play/);
  });
});

test('an interrupted or dead load is transient too', () => {
  for (const n of ['AbortError', 'NetworkError', 'TypeError', 'InvalidStateError'])
    expect(classifyPlayError(domErr(n))).toBe('transient');
  expect(classifyPlayError(undefined)).toBe('transient');
  expect(classifyPlayError({})).toBe('transient');
});

test('the element error code wins over the promise, except for autoplay', () => {
  // 4 is SRC_NOT_SUPPORTED, which on iOS is how a truncated download presents.
  for (const code of [1, 2, 3, 4])
    expect(classifyPlayError(domErr('NotSupportedError'), {code})).toBe('transient');
  expect(classifyPlayError(domErr('NotSupportedError'), {code: 99})).toBe('fatal');
  expect(classifyPlayError(domErr('NotAllowedError'), {code: 4})).toBe('gesture');
});

test('a security failure is not retried', () => {
  expect(classifyPlayError(domErr('SecurityError'))).toBe('fatal');
  expect(playRetry('fatal', 1).retry).toBe(false);
});

test('retries back off and eventually give up, keeping the place', () => {
  const one = () => 1;
  const delays: number[] = [];
  for (let a = 1; a < MAX_PLAY_RETRIES; a++) {
    const p = playRetry('transient', a, one);
    expect(p.retry).toBe(true);
    delays.push(p.delay);
  }
  expect(delays.slice(0, 3)).toEqual([250, 500, 1000]);
  expect(delays.every((d, i) => i === 0 || d >= delays[i - 1])).toBe(true);
  expect(Math.max(...delays)).toBeLessThanOrEqual(6000);
  const done = playRetry('transient', MAX_PLAY_RETRIES, one);
  expect(done.retry).toBe(false);
  expect(done.message).toMatch(/press play/);
});

test('a damaged buffer is thrown away; an abort is not', () => {
  expect(shouldRefetch('transient', null)).toBe(true);
  expect(shouldRefetch('transient', {code: 3})).toBe(true);   // decode: the blob is bad
  expect(shouldRefetch('transient', {code: 4})).toBe(true);   // truncated download
  expect(shouldRefetch('transient', {code: 1})).toBe(false);  // we aborted it ourselves
  expect(shouldRefetch('gesture', null)).toBe(false);
});
