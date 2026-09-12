/**
 * Local durable state: the voice-memo outbox and the reading-position queue.
 *
 * Both exist for the same reason - the phone is often the only copy. A memo lives
 * here from the moment recording stops until the server confirms the note it
 * filed; a position lives here whenever the server could not be told.
 */
import {openDB, type DBSchema, type IDBPDatabase} from 'idb';
import type {Memo} from './outbox';

export interface QueuedPosition {
  book: string;
  chapter: number;
  chunk: number;
  ts: number;
  chapter_title?: string;
  chunks_total?: number;
  chapters_total?: number;
}

interface NarratorDB extends DBSchema {
  outbox: {key: number; value: Memo};
  positions: {key: string; value: QueuedPosition};
}

let dbp: Promise<IDBPDatabase<NarratorDB> | null> | null = null;

function db() {
  dbp ??= openDB<NarratorDB>('narrator', 1, {
    upgrade(d) {
      if (!d.objectStoreNames.contains('outbox'))
        d.createObjectStore('outbox', {keyPath: 'id', autoIncrement: true});
      if (!d.objectStoreNames.contains('positions'))
        d.createObjectStore('positions', {keyPath: 'book'});
    },
  }).catch((e) => { console.warn('indexeddb unavailable', e); return null; });
  return dbp;
}

/**
 * A memo's stable identity, minted the moment it is stored.
 *
 * The server is idempotent on it: a memo posted again - which is what happens
 * every time the phone stops listening before the note is filed, and that is
 * most times on a slow box - is answered with the note the first POST wrote
 * instead of being transcribed a second time and filed twice. Kept to characters
 * the server will accept as a file name; `randomUUID` is only defined in a
 * secure context, so there is a fallback, and a memo that somehow gets no id at
 * all still works (the server then identifies it by its own bytes).
 */
export function newUid(): string {
  const c = globalThis.crypto;
  if (typeof c?.randomUUID === 'function') return c.randomUUID();
  if (typeof c?.getRandomValues === 'function') {
    const b = c.getRandomValues(new Uint8Array(16));
    return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export async function allMemos(): Promise<Memo[]> {
  return (await (await db())?.getAll('outbox')) ?? [];
}
/**
 * Stored with an id if it has none: whoever records a memo does not have to know
 * that the delivery contract needs one.
 *
 * Minted only for a memo that has never been stored (no IndexedDB key yet). A
 * memo queued before ids existed keeps sending none, on purpose - it has already
 * been posted under its content hash, which the server dedups by, and giving it
 * a fresh identity now would make the next retry look like a different memo and
 * file a second note.
 */
export async function putMemo(m: Memo): Promise<void> {
  const fresh = m.uid === undefined && m.id === undefined;
  await (await db())?.put('outbox', fresh ? {...m, uid: newUid()} : m);
}
export async function deleteMemo(id: number): Promise<void> {
  await (await db())?.delete('outbox', id);
}
export async function allPositions(): Promise<QueuedPosition[]> {
  return (await (await db())?.getAll('positions')) ?? [];
}
export async function putPosition(p: QueuedPosition): Promise<void> {
  await (await db())?.put('positions', p);
}
export async function deletePosition(book: string): Promise<void> {
  await (await db())?.delete('positions', book);
}
