import {describe, expect, test} from 'vitest';
import {
  anyEstimated, buildVerdict, estimateBytes, isAction, jobFor, needsRender, phaseFor,
  renderAccepted, rowSignature, shouldReask, STALL_MS, type DownloadPhase,
} from './download';
import type {BuildResult, ChapRow} from './types';

const row = (p: Partial<ChapRow> = {}): ChapRow => ({
  i: 4, title: '5: The Sequence', n: 76, est_min: 12.2, est_bytes: 5_856_000,
  rendered: 0, m4a: false, bytes: null, duration: null,
  queued: false, packing: false, pack_queued: false, ...p,
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

describe('reading what the server said it did', () => {
  const build = (p: Partial<BuildResult> = {}): BuildResult => ({
    ok: true, built: [], building: [], rendering: [], refused: [], ...p,
  });

  test('a chapter the packer took is taken, and is not asked about again', () => {
    expect(buildVerdict(build({building: [4]}), 4)).toEqual({t: 'taken'});
    expect(buildVerdict(build({built: [4]}), 4)).toEqual({t: 'taken'});
  });

  test('a half-rendered chapter says so, with how far it got', () => {
    const r = build({
      rendering: [4],
      refused: [{chapter: 4, reason: 'not_rendered', rendered: 30, n: 76}],
    });
    expect(buildVerdict(r, 4)).toEqual({t: 'rendering', rendered: 30, n: 76});
  });

  test('a chapter that can never be packed is not waited on for 45 minutes', () => {
    const r = build({refused: [{chapter: 9, reason: 'out_of_range', rendered: 0, n: 0}]});
    expect(buildVerdict(r, 9)).toEqual({t: 'impossible', reason: 'out_of_range'});
    const empty = build({refused: [{chapter: 2, reason: 'no_chunks', rendered: 0, n: 0}]});
    expect(buildVerdict(empty, 2)).toEqual({t: 'impossible', reason: 'no_chunks'});
  });

  test('an answer that does not mention the chapter is unknown, not consent', () => {
    expect(buildVerdict(build(), 4)).toEqual({t: 'unknown'});
    expect(buildVerdict(null, 4)).toEqual({t: 'unknown'});
  });

  test("the render queue is the render call's receipt", () => {
    expect(renderAccepted({ok: true, queue: [1, 4], packing: []}, 4)).toBe(true);
    // Already complete: it skipped the queue and went to the packer.
    expect(renderAccepted({ok: true, queue: [], packing: [4]}, 4)).toBe(true);
    expect(renderAccepted({ok: true, queue: [1], packing: []}, 4)).toBe(false);
    expect(renderAccepted(null, 4)).toBe(false);
  });
});

describe('shouldReask - the repeat, kept for real stalls', () => {
  test('an ask nobody acknowledged is repeated', () => {
    expect(shouldReask({acked: false, sinceChangeMs: 0})).toBe(true);
  });

  test('an acknowledged ask is waited on, however slow the chapter is', () => {
    expect(shouldReask({acked: true, sinceChangeMs: 20_000})).toBe(false);
    expect(shouldReask({acked: true, sinceChangeMs: 89_000})).toBe(false);
  });

  test('a row that has not moved at all for ninety seconds is a stall', () => {
    expect(shouldReask({acked: true, sinceChangeMs: STALL_MS})).toBe(true);
    expect(shouldReask({acked: true, sinceChangeMs: 5_000}, 1_000)).toBe(true);
  });

  test('the signature changes exactly when the server got somewhere', () => {
    expect(rowSignature(row({rendered: 3}))).toBe(rowSignature(row({rendered: 3})));
    expect(rowSignature(row({rendered: 3}))).not.toBe(rowSignature(row({rendered: 4})));
    expect(rowSignature(row({queued: true}))).not.toBe(rowSignature(row()));
    expect(rowSignature(row({m4a: true}))).not.toBe(rowSignature(row()));
    expect(rowSignature(undefined)).toBe('none');
  });
});
