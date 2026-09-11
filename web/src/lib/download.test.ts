import {describe, expect, test} from 'vitest';
import {
  anyEstimated, estimateBytes, isAction, jobFor, needsRender, phaseFor,
  type DownloadPhase,
} from './download';
import type {ChapRow} from './types';

const row = (p: Partial<ChapRow> = {}): ChapRow => ({
  i: 4, title: '5: The Sequence', n: 76, est_min: 12.2,
  rendered: 0, m4a: false, bytes: null, duration: null, ...p,
});
const none = new Set<number>();

describe('phaseFor - the rung a chapter is on', () => {
  test('nothing rendered: ask the server for it', () => {
    expect(phaseFor(row(), false)).toBe<DownloadPhase>('queue-render');
  });

  test('the server has it queued: wait', () => {
    expect(phaseFor(row({queued: true}), false)).toBe('await-render');
  });

  test('partly rendered but not queued goes back in the queue', () => {
    // the render worker derives its own bookkeeping from disk, so re-queueing a
    // half-rendered chapter is safe and is the only way to nudge a stalled one
    expect(phaseFor(row({rendered: 30}), false)).toBe('queue-render');
  });

  test('every chunk rendered: ask for the pack', () => {
    expect(phaseFor(row({rendered: 76}), false)).toBe('request-pack');
  });

  test('packing, or queued to pack: wait', () => {
    expect(phaseFor(row({rendered: 76, packing: true}), false)).toBe('await-pack');
    expect(phaseFor(row({rendered: 76, pack_queued: true}), false)).toBe('await-pack');
  });

  test('the m4a exists: store it', () => {
    expect(phaseFor(row({rendered: 76, m4a: true}), false)).toBe('store');
  });

  test('packed outranks a re-render queued behind it', () => {
    // otherwise a chapter the server is already re-rendering would never be
    // stored, even though the file it would store is sitting right there
    expect(phaseFor(row({m4a: true, queued: true, rendered: 10}), false)).toBe('store');
  });

  test('on this device: done, whatever the server says', () => {
    expect(phaseFor(row(), true)).toBe('stored');
    expect(phaseFor(row({queued: true, packing: true}), true)).toBe('stored');
    expect(phaseFor(undefined, true)).toBe('stored');
  });

  test('a row the server has not reported yet is a wait, not a re-queue', () => {
    // /api/chapters is polled; a missing row means "not asked yet", and
    // queueing a render for a chapter we know nothing about is the one thing
    // that could ask for work nobody wanted
    expect(phaseFor(undefined, false)).toBe('await-render');
  });

  test('a zero-chunk chapter never claims to be fully rendered', () => {
    expect(phaseFor(row({n: 0, rendered: 0}), false)).toBe('queue-render');
  });
});

describe('the badge and whose turn it is', () => {
  test('every phase has a job except the finished one', () => {
    expect(jobFor('queue-render')).toBe('queued');
    expect(jobFor('await-render')).toBe('rendering');
    expect(jobFor('request-pack')).toBe('packing');
    expect(jobFor('await-pack')).toBe('packing');
    expect(jobFor('store')).toBe('saving');
    expect(jobFor('stored')).toBeNull();
  });

  test('the client acts on three rungs and waits on the rest', () => {
    expect(['queue-render', 'request-pack', 'store'].every((p) => isAction(p as DownloadPhase)))
      .toBe(true);
    expect(['await-render', 'await-pack', 'stored'].some((p) => isAction(p as DownloadPhase)))
      .toBe(false);
  });
});

describe('needsRender - one queue call for the whole selection', () => {
  test('only the chapters with no audio at all', () => {
    const rows = [
      row({i: 0}),                                  // nothing: queue it
      row({i: 1, queued: true}),                    // already queued
      row({i: 2, rendered: 76}),                    // ready to pack
      row({i: 3, m4a: true}),                       // ready to store
      row({i: 4}),                                  // nothing: queue it
      row({i: 5}),                                  // held on the device
    ];
    expect(needsRender(rows, new Set([5]))).toEqual([0, 4]);
  });

  test('nothing to do is an empty list, not a call with no chapters', () => {
    expect(needsRender([row({m4a: true})], none)).toEqual([]);
    expect(needsRender([], none)).toEqual([]);
  });
});

describe('estimateBytes - what the confirm bar promises', () => {
  test('a packed chapter reports its real size', () => {
    expect(estimateBytes([row({bytes: 1_000_000})])).toBe(1_000_000);
    expect(anyEstimated([row({bytes: 1_000_000})])).toBe(false);
  });

  test('an unmade chapter is estimated from its minutes at 64 kbit/s', () => {
    // 12.2 min * 60 s * 8000 B/s
    expect(estimateBytes([row({est_min: 12.2})])).toBe(5_856_000);
    expect(anyEstimated([row()])).toBe(true);
  });

  test('a mixed selection adds both kinds and is still flagged as an estimate', () => {
    const rows = [row({bytes: 1_000_000}), row({est_min: 10})];
    expect(estimateBytes(rows)).toBe(1_000_000 + 4_800_000);
    expect(anyEstimated(rows)).toBe(true);
  });

  test('a chapter with neither a size nor an estimate contributes nothing', () => {
    expect(estimateBytes([row({est_min: null})])).toBe(0);
    expect(estimateBytes([])).toBe(0);
  });
});
