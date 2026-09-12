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
import {afterFailure, classify, nextAction, type OutboxCtx} from './outbox';
import type {NoteResult} from './types';

const toBase64 = (blob: Blob) => new Promise<string>((resolve) => {
  const r = new FileReader();
  r.onload = () => resolve(String(r.result).split(',')[1] ?? '');
  r.readAsDataURL(blob);
});

export async function flushOutbox(ctx: OutboxCtx): Promise<void> {
  for (const memo of await db.allMemos()) {
    if (nextAction(memo, ctx) !== 'send') continue;
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
