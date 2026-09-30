/** Explicit server tombstones only. Neither an empty shelf nor a failed request
 * can remove a device download. This module keeps no offline deletion guesses. */
let deleted = new Set<string>();
export const isDeletedBook = (key: string): boolean => deleted.has(key);

export interface DeletionDeps {
  fetch: () => Promise<unknown>;
  close: (key: string) => void;
  remove: (key: string) => Promise<void>;
}

export function validDeletions(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((k) => typeof k === 'string'
    && k.length > 0 && Array.from(k).length <= 50 && k.trim() === k
    && k !== '.' && k !== '..' && !k.includes('/') && !k.includes(String.fromCharCode(92))
    && !Array.from(k).some((c) => c.charCodeAt(0) < 32));
}

/** Fetch first, validate the entire answer, and only then perform cleanup.
 * Failed cleanup is retried on the next successful sync. */
export async function syncDeletions(deps: DeletionDeps): Promise<boolean> {
  let keys: unknown;
  try { keys = await deps.fetch(); } catch { return false; }
  if (!validDeletions(keys)) return false;
  deleted = new Set(keys);
  for (const key of deleted) {
    deps.close(key); // Stop playback and new stores before removing existing ones.
    try { await deps.remove(key); } catch { /* retry next time; never drop memos */ }
  }
  return true;
}
