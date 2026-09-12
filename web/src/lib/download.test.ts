import {describe, expect, test} from 'vitest';
import {
  anyEstimated, estimateBytes, jobFor, phaseFor, queueJob, type DownloadPhase,
} from './download';
import type {ChapRow} from './types';

const row = (p: Partial<ChapRow> = {}): ChapRow => ({
  i: 4, title: '5: The Sequence', n: 76, est_min: 12.2, est_bytes: 5_856_000,
  rendered: 0, m4a: false, bytes: null, duration: null,
  queued: false, packing: false, pack_queued: false, ...p,
});

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
});

describe('estimateBytes - what the confirm bar promises', () => {
  test('a packed chapter reports its real size, whatever the estimate said', () => {
    expect(estimateBytes([row({bytes: 1_000_000})])).toBe(1_000_000);
    expect(anyEstimated([row({bytes: 1_000_000})])).toBe(false);
  });

  test("an unmade chapter uses the server's own est_bytes", () => {
    // The server did this arithmetic with the bitrate it is actually packing
    // at; the client no longer guesses at 64 kbit/s and hopes.
    expect(estimateBytes([row({est_bytes: 9_000_000})])).toBe(9_000_000);
    expect(anyEstimated([row()])).toBe(true);
  });

  test('a row from before est_bytes falls back to minutes at a given rate', () => {
    const old = row({est_bytes: null, est_min: 12.2});
    // 12.2 min * 60 s * 8000 B/s, the 64 kbit/s default.
    expect(estimateBytes([old])).toBe(5_856_000);
    // And at the rate /api/status reports, which is the point of reporting it:
    // the same chapter at 128 kbit/s is twice the download.
    expect(estimateBytes([old], 960_000)).toBe(11_712_000);
    // A nonsense rate falls back rather than promising a chapter of nothing.
    expect(estimateBytes([old], 0)).toBe(5_856_000);
  });

  test('a mixed selection adds every kind and is still flagged as an estimate', () => {
    const rows = [row({bytes: 1_000_000}), row({est_bytes: 4_800_000})];
    expect(estimateBytes(rows)).toBe(1_000_000 + 4_800_000);
    expect(anyEstimated(rows)).toBe(true);
  });

  test('a chapter with no size, no estimate and no minutes contributes nothing', () => {
    expect(estimateBytes([row({est_min: null, est_bytes: null})])).toBe(0);
    expect(estimateBytes([])).toBe(0);
  });
});


// ------------------------------------------------- the badge, after a restart

describe('queueJob - what this device is doing about a chapter', () => {
  test('nothing queued is nothing to say: the row speaks for the server', () => {
    expect(queueJob(row({rendered: 76}), false, false)).toBeNull();
    expect(queueJob(undefined, false, false)).toBeNull();
  });

  test('a queued chapter reports the stage the server is actually at', () => {
    // "queued" on its own is only right before the server has started; once it
    // is rendering, that is the honest thing for the row to say.
    expect(queueJob(row(), true, false)).toBe('queued');
    expect(queueJob(row({queued: true}), true, false)).toBe('rendering');
    expect(queueJob(row({rendered: 76}), true, false)).toBe('packing');
    expect(queueJob(row({pack_queued: true}), true, false)).toBe('packing');
  });

  test('a copy in flight outranks everything: it is what he is waiting on', () => {
    expect(queueJob(row({m4a: true}), true, true)).toBe('saving');
    // even for a chapter no longer in the queue - the sweep is mid-copy
    expect(queueJob(row({m4a: true}), false, true)).toBe('saving');
  });

  test('the queue is the durable half, so this survives a restart', () => {
    // The old job map lived in component state and was empty after every
    // launch, so a chapter ordered last night came back looking unasked-for -
    // and tapping it downloaded it again. Both arguments here come from disk.
    const ordered = row({queued: true});
    expect(queueJob(ordered, true, false)).toBe('rendering');
  });
});
