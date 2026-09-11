/**
 * What a chapter's state actually is, in words.
 *
 * The drawer used to show a bare string per chapter - "rendered", "12/76", a byte
 * count - which reads fine once you already know the two-stage pipeline behind it
 * and means nothing before that. The pipeline is: the server renders a chapter's
 * chunks, packs them into one file, and only then can this device keep a copy. So
 * every row carries which of those four things has happened, plus a sentence that
 * says it in full.
 *
 * Kept out of the component so the ladder can be tested rather than eyeballed.
 */
import type {ChapRow} from './types';

export type ChapterStateKey =
  | 'packing' | 'saving' | 'downloaded' | 'to-pack' | 'ready'
  | 'rendered' | 'queued' | 'partial' | 'none';

/**
 * A job this device is running on the chapter right now - one rung of
 * lib/download.ts's ladder. It outranks whatever the server's row says,
 * because a download in flight is the thing he is actually waiting on.
 */
export type Job = 'queued' | 'rendering' | 'packing' | 'saving';

export interface ChapterState {
  key: ChapterStateKey;
  /** the terse part, next to the icon */
  text: string;
  /** the whole sentence, on hover */
  tip: string;
  tone: 'ok' | 'work' | 'part' | 'done' | 'none';
  spin?: boolean;
}

export function chapterState(
  r: ChapRow, offline: boolean, fmtBytes: (n: number) => string, job?: Job,
): ChapterState {
  const packing: ChapterState = {
    key: 'packing', text: 'packing', tone: 'work', spin: true,
    tip: 'the server is packing this chapter into one audio file',
  };
  if (job === 'packing') return packing;
  if (job === 'saving') return {
    key: 'saving', text: 'saving', tone: 'work', spin: true,
    tip: 'copying the audio onto this device',
  };
  if (job === 'queued') return {
    key: 'queued', text: 'waiting', tone: 'work',
    tip: 'in the download queue - the server has to render it first',
  };
  if (job === 'rendering') return {
    key: 'queued',
    text: r.n && r.rendered ? `${r.rendered}/${r.n}` : 'rendering',
    tone: 'work', spin: !r.rendered,
    tip: 'the server is rendering this chapter, and the download follows it',
  };
  // On this device beats everything the server says about it.
  if (offline) return {
    key: 'downloaded', text: r.bytes ? fmtBytes(r.bytes) : 'saved', tone: 'ok',
    tip: 'downloaded — this chapter plays with no network at all',
  };
  if (r.packing) return packing;
  if (r.pack_queued) return {
    key: 'to-pack', text: 'to pack', tone: 'work',
    tip: 'rendered, waiting its turn to be packed',
  };
  if (r.m4a) return {
    key: 'ready', text: r.bytes ? fmtBytes(r.bytes) : 'ready', tone: 'done',
    tip: 'rendered and packed on the server — plays over the network, or download it',
  };
  if (r.n && r.rendered >= r.n) return {
    key: 'rendered', text: 'rendered', tone: 'done',
    tip: 'rendered on the server, not packed yet — downloading packs it first',
  };
  if (r.queued) return {
    key: 'queued', text: 'queued', tone: 'work',
    tip: 'queued for rendering on the server',
  };
  if (r.rendered) return {
    key: 'partial', text: `${r.rendered}/${r.n}`, tone: 'part',
    tip: `partly rendered: ${r.rendered} of ${r.n} chunks have audio`,
  };
  return {
    key: 'none', text: r.est_min != null ? `~${Math.round(r.est_min)}m` : '', tone: 'none',
    tip: 'not rendered — no audio for this chapter yet. Readable either way.',
  };
}
