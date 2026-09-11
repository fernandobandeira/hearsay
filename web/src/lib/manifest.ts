/**
 * The chunk <-> time mapping for a packed chapter.
 *
 * Concatenating a chapter's chunks into one audio file is what makes lock-screen
 * playback work, and it is also what would destroy narrator's reading positions
 * if nothing else were done: a position is a chunk index, and an audio file has
 * only seconds. The manifest is the bridge, and this module is the arithmetic -
 * pure and separate precisely because getting it wrong sends every voice note and
 * every resume point to the wrong sentence.
 */
export interface Manifest {
  book: string;
  chapter: number;
  title: string;
  chunks: number;       // how many chunks the chapter had when it was packed
  starts: number[];     // start second of each chunk, strictly ascending
  duration: number;
  gap?: number;
  para_gap?: number;
  bytes?: number;
  sample_rate?: number;
}

/**
 * Which chunk is playing at `seconds`. Clamped at both ends: a currentTime a
 * hair past the end (the element rounds) is still the last chunk, not a crash.
 */
export function chunkAt(m: Manifest, seconds: number): number {
  const s = m.starts;
  if (!s?.length) return 0;
  if (!(seconds > 0)) return 0;                 // also catches NaN
  let lo = 0, hi = s.length - 1, best = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (s[mid] <= seconds) { best = mid; lo = mid + 1; } else hi = mid - 1;
  }
  return best;
}

/** Where a chunk starts. Out of range clamps rather than yielding NaN, which an
 *  <audio> element would silently turn into a seek to zero. */
export function startOf(m: Manifest, chunk: number): number {
  const s = m.starts;
  if (!s?.length) return 0;
  const i = Math.max(0, Math.min(Math.floor(chunk) || 0, s.length - 1));
  return s[i];
}

/**
 * Is this manifest usable for a chapter of `chunkCount` chunks?
 *
 * Two ways it can lie: it was built from a different chunking of the book
 * (re-parsing can move chunk boundaries), or its starts are not ascending (a hole
 * in the source audio would do that). Either way, playing it would put the
 * reader's position on the wrong words - so say no and fall back.
 */
export function isSane(m: unknown, chunkCount?: number): m is Manifest {
  if (!m || typeof m !== 'object') return false;
  const o = m as Manifest;
  if (!Array.isArray(o.starts) || !o.starts.length) return false;
  if (typeof o.duration !== 'number' || !(o.duration > 0)) return false;
  if (o.starts.length !== o.chunks) return false;
  if (chunkCount != null && o.chunks !== chunkCount) return false;
  if (o.starts[0] !== 0) return false;
  for (let i = 1; i < o.starts.length; i++)
    if (!(o.starts[i] > o.starts[i - 1])) return false;
  return o.starts[o.starts.length - 1] < o.duration;
}

/** Clamp a seek target into the chapter. */
export function clampTime(m: Manifest, t: number): number {
  if (!(t > 0)) return 0;
  return Math.min(t, Math.max(0, m.duration - 0.05));
}
