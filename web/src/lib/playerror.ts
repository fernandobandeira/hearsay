/**
 * What a failed `audio.play()` actually means.
 *
 * The bug this exists for: on an iPhone, over the tailnet HTTPS proxy, a chunk
 * boundary that hits a network blip (or a wifi-to-LTE handover) surfaces as
 * `play()` rejecting with **NotSupportedError** - the media element failed to
 * load the source, and the promise carries that rather than a network error. The
 * old reader painted "audio blocked: NotSupportedError - press play again" and
 * stopped dead, so a five-second blip cost a manual tap.
 *
 * Two failures wear the same shape and need opposite answers:
 *   NotAllowedError   the browser refuses to start audio without a gesture.
 *                     Retrying is pointless and infuriating - ask for the tap.
 *   everything else   the source did not load. The playhead is still valid and
 *                     the audio is still on the server: re-fetch and resume.
 */
import {delayFor, MEDIA} from './backoff';

export type PlayVerdict = 'gesture' | 'transient' | 'fatal';

/** DOMException names that mean "the load failed", not "you may not play". */
const TRANSIENT_NAMES = new Set([
  'NotSupportedError',   // iOS: the element could not load the source at all
  'AbortError',          // load() or a new src interrupted the play()
  'NetworkError',
  'OperationError',
  'UnknownError',
  'TypeError',           // some engines reject with this for a dead blob URL
  'InvalidStateError',
]);

/**
 * MediaError codes (the element's own `.error`), more reliable than the promise
 * rejection when both are present:
 *   1 ABORTED  2 NETWORK  3 DECODE  4 SRC_NOT_SUPPORTED
 * All four mean the bytes did not arrive or did not parse, and every one is worth
 * another fetch - including 4, which is how iOS presents a truncated download.
 */
const TRANSIENT_CODES = new Set([1, 2, 3, 4]);

export function classifyPlayError(err: unknown, mediaError?: {code?: number} | null): PlayVerdict {
  const name = err && typeof err === 'object' && 'name' in err
    ? String((err as {name: unknown}).name) : '';
  if (name === 'NotAllowedError') return 'gesture';   // the one a retry cannot fix
  if (name === 'SecurityError') return 'fatal';
  if (mediaError && typeof mediaError.code === 'number')
    return TRANSIENT_CODES.has(mediaError.code) ? 'transient' : 'fatal';
  if (TRANSIENT_NAMES.has(name)) return 'transient';
  if (!name) return 'transient';                      // no name at all: assume a dead load
  return 'fatal';
}

/** Attempts before a transient failure stops being treated as transient. */
export const MAX_PLAY_RETRIES = 6;

export interface PlayPlan { retry: boolean; delay: number; message: string | null; }

export function playRetry(verdict: PlayVerdict, attempt: number, random?: () => number): PlayPlan {
  if (verdict === 'gesture')
    return {retry: false, delay: 0, message: 'tap play to start audio'};
  if (verdict === 'fatal')
    return {retry: false, delay: 0, message: 'this audio will not play here'};
  if (attempt >= MAX_PLAY_RETRIES)
    return {retry: false, delay: 0, message: 'audio keeps failing — press play to retry'};
  return {retry: true, delay: delayFor(attempt - 1, MEDIA, random), message: null};
}

/**
 * Does recovering mean throwing the bytes away? A decode failure or an
 * unsupported source usually means a truncated buffer: re-assigning the same URL
 * would fail identically forever. An abort damaged nothing.
 */
export function shouldRefetch(verdict: PlayVerdict, mediaError?: {code?: number} | null): boolean {
  if (verdict !== 'transient') return false;
  if (!mediaError || typeof mediaError.code !== 'number') return true;
  return mediaError.code !== 1;
}
