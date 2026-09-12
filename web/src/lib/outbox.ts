/**
 * The voice-memo outbox: decisions only.
 *
 * A memo is a thing that happened once. The old reader base64'd it straight into
 * a POST and, if that POST failed, the thought was gone. Now the recording is
 * written to IndexedDB first and deleted only when the server answers with the
 * note it filed - so the rule that matters is "what counts as delivered", and it
 * lives here, away from the DOM and the network, where it can be tested.
 */
export const MAX_TRIES = 5;

export interface Memo {
  id?: number;
  blob: Blob;
  mime: string;
  book: string | null;      // the book it was recorded against
  chapter: number;
  chunk: number;
  ts: number;
  tries?: number;
  err?: string;
}

export interface OutboxCtx {
  online: boolean;
  manual?: boolean;            // the user asked for a retry by hand
}

/**
 * What to do with one queued memo right now.
 *
 * There used to be a second 'hold': a memo recorded against a book the server
 * had since swapped out waited, because the server built the quote callout from
 * *its* loaded chapter. Now the memo names its book and the server quotes that
 * book's on-disk text, so the only thing worth waiting for is the network.
 */
export function nextAction(rec: Memo, ctx: OutboxCtx): 'send' | 'hold' | 'stalled' {
  if (!ctx.online) return 'hold';
  if (!ctx.manual && (rec.tries ?? 0) >= MAX_TRIES) return 'stalled';
  return 'send';
}

/**
 * Did the server take it? Only a 2xx carrying the note it wrote counts. A 2xx
 * with no note in it is most likely a proxy answering for narrator, not narrator.
 */
export function classify(status: number, body: unknown): 'delivered' | 'retry' | 'reject' {
  const b = body as {file?: string; error?: string} | null;
  if (status >= 200 && status < 300) {
    if (b && typeof b.file === 'string' && b.file) return 'delivered';
    return 'retry';
  }
  if (status === 0 || status === 408 || status === 429 || status >= 500) return 'retry';
  return 'reject';                      // a 4xx: asking again changes nothing
}

/** The record as stored after an attempt that did not deliver. Never drops the
 *  blob - that is the whole point of the queue. */
export function afterFailure(rec: Memo, why: string): Memo {
  return {...rec, tries: (rec.tries ?? 0) + 1, err: why};
}

/** One line for the badge. Here so the indicator and the diagnostics agree. */
export function summary(memos: Memo[], positions: number, online: boolean): string {
  const bits: string[] = [];
  if (!online) bits.push('offline');
  if (memos.length) bits.push(`${memos.length} note${memos.length > 1 ? 's' : ''} queued`);
  const stalled = memos.filter(m => (m.tries ?? 0) >= MAX_TRIES).length;
  if (stalled) bits.push(`${stalled} stalled`);
  if (positions) bits.push('position queued');
  return bits.join(' · ');
}
