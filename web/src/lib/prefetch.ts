/**
 * Chunk-mode prefetch: the last-resort player's buffer.
 *
 * The old reader pointed one <audio> element at `/api/chunk/ci/i.wav`, waited for
 * it to end, then pointed the same element at the next one. Over a tailnet tunnel
 * that is a round trip plus a decode at every boundary - roughly every ten
 * seconds - which is exactly the stutter Fernando hears. HLS and the packed m4a
 * remove the boundaries for any chapter that has been rendered; this covers the
 * case where he is listening *at* the render frontier, which is most of the time.
 *
 * A chunk that is not rendered yet is a 404, not an error - it is the renderer
 * being behind. Those are remembered with a timestamp and re-probed on a cooldown
 * rather than hammered.
 */
export interface PrefetchDeps {
  fetch: typeof globalThis.fetch;
  createURL: (b: Blob) => string;
  revokeURL: (u: string) => void;
  now?: () => number;
  ahead?: number;      // chunks kept in flight past the playhead
  retryMs?: number;    // cooldown before re-probing an unrendered chunk
}

interface Entry {
  state: 'loading' | 'ready' | 'missing';
  promise?: Promise<string | null>;
  url?: string;
  bytes?: number;
  at?: number;
}

export class ChunkPrefetcher {
  private map = new Map<string, Entry>();
  private ahead: number;
  private retryMs: number;
  private now: () => number;
  hits = 0;
  misses = 0;

  constructor(private d: PrefetchDeps) {
    this.ahead = d.ahead ?? 3;
    this.retryMs = d.retryMs ?? 1200;
    this.now = d.now ?? (() => Date.now());
  }

  key(ci: number, i: number) { return `${ci}:${i}`; }
  url(ci: number, i: number) { return `/api/chunk/${ci}/${i}.wav`; }

  /** Fill the buffer for a playhead at `i`. Cheap to call often. */
  prime(ci: number, i: number, total: number): void {
    for (let n = i; n < Math.min(total, i + this.ahead + 1); n++) this.want(ci, n);
    this.trim(ci, i);
  }

  /**
   * The playable URL for a chunk, fetching now if the buffer missed. Resolves
   * null when the chunk is not rendered yet - the caller waits and asks again.
   */
  async take(ci: number, i: number): Promise<string | null> {
    const e = this.map.get(this.key(ci, i));
    if (e?.state === 'ready' && e.url) { this.hits++; return e.url; }
    if (e?.state === 'loading' && e.promise) { this.hits++; return e.promise; }
    this.misses++;
    return (await this.want(ci, i, true)) ?? null;
  }

  ready(ci: number, i: number): boolean {
    return this.map.get(this.key(ci, i))?.state === 'ready';
  }

  /** The blob URL for a chunk already in hand, without waiting for anything. */
  readyUrl(ci: number, i: number): string | null {
    const e = this.map.get(this.key(ci, i));
    return e?.state === 'ready' && e.url ? e.url : null;
  }

  get bytes(): number {
    let n = 0;
    for (const e of this.map.values()) n += e.bytes ?? 0;
    return n;
  }

  /** Drop everything, or one chapter's worth. */
  drop(ci?: number): void {
    for (const [k, e] of [...this.map]) {
      if (ci != null && k.slice(0, k.indexOf(':')) !== String(ci)) continue;
      if (e.url) this.d.revokeURL(e.url);
      this.map.delete(k);
    }
  }

  private want(ci: number, i: number, force?: boolean): Promise<string | null> | null {
    const k = this.key(ci, i);
    const e = this.map.get(k);
    if (e) {
      if (e.state === 'ready') return Promise.resolve(e.url ?? null);
      if (e.state === 'loading') return e.promise ?? null;
      if (!force && this.now() - (e.at ?? 0) < this.retryMs) return null;
    }
    const promise = this.fetchOne(ci, i);
    this.map.set(k, {state: 'loading', promise});
    return promise;
  }

  private async fetchOne(ci: number, i: number): Promise<string | null> {
    const k = this.key(ci, i);
    try {
      const res = await this.d.fetch(this.url(ci, i));
      if (!res.ok) { this.map.set(k, {state: 'missing', at: this.now()}); return null; }
      const blob = await res.blob();
      const url = this.d.createURL(blob);
      if (!this.map.has(k)) { this.d.revokeURL(url); return null; }  // chapter changed
      this.map.set(k, {state: 'ready', url, bytes: blob.size});
      return url;
    } catch {
      this.map.set(k, {state: 'missing', at: this.now()});
      return null;
    }
  }

  private trim(ci: number, i: number): void {
    for (const [k, e] of [...this.map]) {
      const [c, n] = k.split(':').map(Number);
      if (c !== ci || (n >= i - 1 && n <= i + this.ahead)) continue;
      if (e.state === 'loading') continue;          // let it land, then it trims
      if (e.url) this.d.revokeURL(e.url);
      this.map.delete(k);
    }
  }
}
