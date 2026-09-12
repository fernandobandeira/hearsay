/**
 * Draining the queues.
 *
 * The decisions live in outbox.ts; this is the plumbing that acts on them. The
 * one rule worth restating here: a recording leaves the device only when the
 * server answers with the note it filed. Everything else - a 500, a 4xx, a
 * timeout, a proxy's own 200 - leaves it exactly where it was.
 */
import * as db from './db';
import {bookKey} from './offline';
import {afterFailure, classify, nextAction, type Memo, type OutboxCtx} from './outbox';
import type {NoteResult} from './types';

const toBase64 = (blob: Blob) => new Promise<string>((resolve) => {
  const r = new FileReader();
  r.onload = () => resolve(String(r.result).split(',')[1] ?? '');
  r.readAsDataURL(blob);
});

/* One drain at a time.
 *
 * flush() is triggered from six places - recording a memo, the heartbeat
 * recovering, onlineManager, visibilitychange, pagehide, focus - and returning
 * to the PWA fires visibilitychange *and* focus. Unguarded, that is two drains
 * reading the same outbox a millisecond apart and posting the same recording
 * twice, which is exactly what production did. So concurrent callers get the
 * drain that is already running rather than one of their own.
 *
 * What must not be lost in the coalescing is a caller asking for *more* than the
 * running drain is doing: a manual retry (the user tapping "retry", which is the
 * only thing that gets a stalled memo sent again) or a drain that started while
 * offline. Those schedule exactly one follow-up pass instead of being dropped. */
let draining: Promise<void> | null = null;
let running: OutboxCtx | null = null;
let queued: OutboxCtx | null = null;

const widens = (now: OutboxCtx, next: OutboxCtx) =>
  (!!next.manual && !now.manual) || (next.online && !now.online);

const merge = (a: OutboxCtx | null, b: OutboxCtx): OutboxCtx =>
  a ? {online: a.online || b.online, manual: a.manual || b.manual} : b;

export function flushOutbox(ctx: OutboxCtx): Promise<void> {
  if (draining) {
    if (running && widens(running, ctx)) queued = merge(queued, ctx);
    return draining;
  }
  draining = (async () => {
    try {
      let next: OutboxCtx | null = ctx;
      while (next) {
        running = next;
        queued = null;
        await sendQueued(next);
        next = queued;
      }
    } finally {
      draining = null;
      running = null;
      queued = null;
    }
  })();
  return draining;
}

/**
 * Ask whether a memo became a note, without pushing the recording up again.
 *
 * For the one that ran out of tries here while the server was quietly finishing
 * it - which is now the normal end of a memo that was interrupted, because the
 * server resumes its own unfiled work across a restart. An id and no audio is
 * the question; the note is the answer, and only then is the copy deleted. A 404
 * means "not filed", which changes nothing: it stays.
 */
async function collect(memo: Memo): Promise<boolean> {
  if (!memo.uid) return false;
  try {
    const res = await fetch('/api/note', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({audio: '', id: memo.uid}),
    });
    const body = await res.json().catch(() => null);
    if (classify(res.status, body) !== 'delivered') return false;
    await db.deleteMemo(memo.id as number);
    return true;
  } catch {
    return false;                        // still nothing answering; it keeps
  }
}

async function sendQueued(ctx: OutboxCtx): Promise<void> {
  for (const memo of await db.allMemos()) {
    const action = nextAction(memo, ctx);
    // A memo that has run out of tries is not uploaded again until the user asks
    // - but it may still collect a confirmation, which costs a few hundred bytes
    // and is the only thing standing between it and being deleted.
    if (action === 'stalled') { await collect(memo); continue; }
    if (action !== 'send') continue;
    let status = 0;
    let body: unknown = null;
    try {
      const res = await fetch('/api/note', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
          audio: await toBase64(memo.blob), mime: memo.mime,
          chapter: memo.chapter, chunk: memo.chunk,
          // The memo stores the book's file name; the server files by cache key.
          // Naming it lets the note quote the right book's text even when the
          // server has since loaded another one.
          ...(memo.book ? {book: bookKey({name: memo.book})} : {}),
          // The memo's own id, so a retry of one the server already filed - the
          // usual case, because the note takes minutes and the phone rarely
          // waits - is answered with that note rather than transcribed again.
          // A memo queued before ids existed simply sends none; the server then
          // identifies it by the bytes.
          ...(memo.uid ? {id: memo.uid} : {}),
        }),
      });
      status = res.status;
      body = await res.json().catch(() => null);
    } catch { status = 0; }

    if (classify(status, body) === 'delivered') {
      await db.deleteMemo(memo.id as number);
      continue;
    }
    const why = (body as {error?: string} | null)?.error ?? (status ? `HTTP ${status}` : 'offline');
    await db.putMemo(afterFailure(memo, why));
  }
}

/**
 * Positions go to /api/position, which names its book rather than assuming the
 * server still holds it. Last write wins, which is the right rule for one reader
 * on several devices.
 */
export async function flushPositions(): Promise<void> {
  for (const p of await db.allPositions()) {
    try {
      const res = await fetch('/api/position', {
        method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(p),
      });
      if (res.ok) await db.deletePosition(p.book);
    } catch { /* still offline; it keeps */ }
  }
}

export type {NoteResult};
