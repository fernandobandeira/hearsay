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
  /**
   * A transient that is running *underneath* a downloaded row - a re-render, a
   * repack, a second save. Never the row's own state: it is a whisper beside
   * the download mark, which stays exactly what it was.
   */
  note?: string;
}

/**
 * The other tier, on the same row.
 *
 * `chapterState` is entirely about audio - render, pack, download - because that
 * is the expensive half. The cheap half is the words, and until now the drawer
 * only stated it per *book* ("text 7/12"), which says nothing about which
 * chapters the missing five parts were. Offline those chapters are a dead tap.
 *
 * Stated as an absence rather than a presence, deliberately: the words being
 * here is the norm and the whole point of taking them without asking, so a mark
 * on every row would be noise. A mark means "these particular words are not
 * saved", and what it means for the reader depends on whether there is a network
 * to fetch them over - which is the only reason `connected` is a parameter.
 *
 * Two icons, both already in the drawer's vocabulary: the text tier's own `Type`
 * when the words are merely not saved, and the connection badge's `CloudOff`
 * when that actually stands between the reader and the chapter.
 */
export interface TextMark {
  icon: 'text' | 'no-network';
  tone: ChapterState['tone'];
  tip: string;
}

export function textMark(saved: boolean | null, connected: boolean): TextMark | null {
  // null is "the index cannot say", and a row must not claim on a guess.
  if (saved !== false) return null;
  return connected
    ? {icon: 'text', tone: 'none',
       tip: 'these words are not in the saved text — opening this chapter fetches them'}
    : {icon: 'no-network', tone: 'part',
       tip: 'these words are not in the saved text — with no network this chapter may not open'};
}

/**
 * **Downloaded outranks everything.** One question a row has to answer before any
 * other: is this chapter *on this device*? Everything else on the ladder - a job
 * this device is running, a queue or a spinner on the server, a size label - is
 * about work in flight somewhere else, and for a while any of them could take the
 * download mark's place, which left the list unable to say what was actually
 * stored here. So `offline` is read first and nothing below can reach past it: a
 * stored chapter is always the download icon in the `ok` tone. A transient
 * running at the same time (he re-queued a render, the server is repacking)
 * survives as `note` and inside the sentence - a secondary hint, never the state.
 */
export function chapterState(
  r: ChapRow, offline: boolean, fmtBytes: (n: number) => string, job?: Job,
): ChapterState {
  const other = elsewhereState(r, fmtBytes, job);
  if (!offline) return other;
  // `work` is the tone of the things that are still happening; a `ready`/
  // `rendered`/size state is not news next to a copy that is already here.
  const busy = other.tone === 'work' ? other.text : null;
  return {
    key: 'downloaded', text: r.bytes ? fmtBytes(r.bytes) : 'saved', tone: 'ok',
    ...(busy ? {note: busy} : {}),
    tip: busy
      ? `downloaded — this chapter plays with no network at all (${busy}, in the background)`
      : 'downloaded — this chapter plays with no network at all',
  };
}

/**
 * The rest of the ladder: what the row would say if this device had no copy -
 * the job it is running, then whatever the server's row reports. Only ever
 * reached through `chapterState`, which puts the download mark above all of it.
 */
function elsewhereState(
  r: ChapRow, fmtBytes: (n: number) => string, job?: Job,
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
