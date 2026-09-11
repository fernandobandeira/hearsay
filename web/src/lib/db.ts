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

export async function allMemos(): Promise<Memo[]> {
  return (await (await db())?.getAll('outbox')) ?? [];
}
export async function putMemo(m: Memo): Promise<void> {
  await (await db())?.put('outbox', m);
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
