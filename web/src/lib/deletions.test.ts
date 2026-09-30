import {beforeEach, describe, expect, it, vi} from 'vitest';
import {isDeletedBook, syncDeletions, validDeletions} from './deletions';

const deps = (value: unknown) => ({fetch: vi.fn().mockResolvedValue(value), close: vi.fn(), remove: vi.fn().mockResolvedValue(undefined)});
beforeEach(async () => { await syncDeletions(deps([])); });

describe('confirmed server deletion sync', () => {
  it.each(['offline', 'timeout', 'HTTP 503', 'HTTP 404'])('preserves downloads on %s', async (why) => {
    const d = deps([]);
    d.fetch.mockRejectedValue(new Error(why));
    expect(await syncDeletions(d)).toBe(false);
    expect(d.close).not.toHaveBeenCalled();
    expect(d.remove).not.toHaveBeenCalled();
    expect(isDeletedBook('Keep')).toBe(false);
  });
  it.each([null, {}, {books: []}, ['Good', '../Bad'], [''], ['..'], ['a\\b'], ['a/b'], [42], ['x'.repeat(51)], ['bad\nkey']])('rejects the entire malformed response %j', async (value) => {
    const d = deps(value);
    expect(await syncDeletions(d)).toBe(false);
    expect(d.close).not.toHaveBeenCalled();
    expect(d.remove).not.toHaveBeenCalled();
  });
  it('an empty deletion list never infers deletion from an absent library', async () => {
    const d = deps([]);
    expect(await syncDeletions(d)).toBe(true);
    expect(d.remove).not.toHaveBeenCalled();
  });
  it('closes and removes exactly the confirmed keys, blocking stores first', async () => {
    const d = deps(['Finished']);
    d.remove.mockImplementation(async (key: string) => {
      expect(isDeletedBook(key)).toBe(true);
      expect(d.close).toHaveBeenCalledWith(key);
    });
    expect(await syncDeletions(d)).toBe(true);
    expect(d.remove).toHaveBeenCalledExactlyOnceWith('Finished');
    expect(isDeletedBook('Keep')).toBe(false);
    expect(validDeletions(['The Mom Test (2013)', '日本語'])).toBe(true);
  });
  it('retries cache cleanup after a failure without dropping the server record', async () => {
    const d = deps(['Finished']);
    d.remove.mockRejectedValueOnce(new Error('cache busy'));
    await syncDeletions(d);
    await syncDeletions(d);
    expect(d.remove).toHaveBeenCalledTimes(2);
  });
  it('only a later successful answer clears the store block for a reuploaded book', async () => {
    await syncDeletions(deps(['Finished']));
    const failed = deps([]); failed.fetch.mockRejectedValue(new Error('offline'));
    await syncDeletions(failed);
    expect(isDeletedBook('Finished')).toBe(true);
    await syncDeletions(deps([]));
    expect(isDeletedBook('Finished')).toBe(false);
  });
});
