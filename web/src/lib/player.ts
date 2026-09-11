/**
 * The audio machinery, kept out of React.
 *
 * Three sources, in order of preference, because they fail in different ways:
 *
 *   cached m4a   the chapter was downloaded. No network is involved at all, so
 *                this wins even when the network is fine.
 *   HLS          the streaming path. iOS plays it natively in a plain <audio>,
 *                which hands buffering and per-segment retry to the OS: a blip
 *                re-fetches one six-second segment instead of killing the load,
 *                which is exactly what a single file over Range does.
 *   chunks       the chapter is still rendering. One ~1-2 s wav at a time, but
 *                prefetched into blobs and swapped between two elements, so the
 *                boundary costs no network.
 *
 * Whichever is playing, the reading position is a chunk index: `currentTime` maps
 * back through the chapter manifest. That is the whole reason the manifest is
 * built from the real WAV durations and the segments are stream-copied rather
 * than re-encoded - the three sources share one timeline.
 */
import {ChunkPrefetcher} from './prefetch';
import {chunkAt, clampTime, startOf, type Manifest} from './manifest';
import {classifyPlayError, playRetry, shouldRefetch} from './playerror';
import {chapterAudioUrl, chapterHlsUrl} from './api';

export type PlayMode = 'hls' | 'chapter' | 'chunk' | 'none';

export interface OpenOptions {
  key: string;
  ci: number;
  chunkCount: number;
  manifest: Manifest | null;
  packed: boolean;        // the server has an m4a for it
  downloaded: boolean;    // and this device has a copy
  startChunk: number;
}

export interface PlayerEvents {
  onChunk: (i: number) => void;
  onMode: (m: PlayMode) => void;
  onPlaying: (p: boolean) => void;
  onWaiting: (w: boolean) => void;
  onMessage: (m: string | null) => void;
  onChapterEnd: () => void;
}

/**
 * Native HLS, and only native HLS.
 *
 * Deliberately narrow: Chromium answers "maybe" to `application/x-mpegURL` - a
 * container it nominally knows - and then cannot play a note of it, which sends
 * the reader down a path that fails silently. Safari (and every iOS browser,
 * which is Safari) answers the canonical `vnd.apple.mpegurl` type; nothing else
 * that matters does. A wrong answer is survivable anyway: a failed HLS load falls
 * back to the packed file (see `heal`).
 */
const canPlayHls = (a: HTMLAudioElement) =>
  a.canPlayType('application/vnd.apple.mpegurl') !== '';

export class Player {
  private els: [HTMLAudioElement, HTMLAudioElement];
  private act = 0;
  private pf: ChunkPrefetcher;
  private manifest: Manifest | null = null;
  private opts: OpenOptions | null = null;
  private failures = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private wantPlaying = false;

  mode: PlayMode = 'none';
  chunk = 0;

  constructor(private ev: PlayerEvents) {
    this.els = [new Audio(), new Audio()];
    for (const e of this.els) {
      e.preload = 'auto';
      // In the document, not floating: some engines only grant a real media
      // session to an element the page owns, and it makes the player inspectable.
      e.setAttribute('data-testid', 'audio');
      e.hidden = true;
      document.body?.appendChild(e);
      e.addEventListener('timeupdate', () => this.onTime(e));
      e.addEventListener('ended', () => { if (e === this.au) this.onEnded(); });
      e.addEventListener('error', () => { if (e === this.au && this.wantPlaying) void this.play(true); });
      e.addEventListener('loadedmetadata', () => this.applyPendingSeek(e));
    }
    this.pf = new ChunkPrefetcher({
      fetch: (u, i) => fetch(u, i),
      createURL: (b) => URL.createObjectURL(b),
      revokeURL: (u) => URL.revokeObjectURL(u),
      ahead: 3,
    });
  }

  private get au() { return this.els[this.act]; }
  private get standby() { return this.els[this.act ^ 1]; }
  private pendingSeek: number | null = null;

  get playing() { return this.wantPlaying; }
  get duration() { return this.manifest?.duration ?? this.au.duration; }
  get currentTime() { return this.au.currentTime; }
  get bufferedBytes() { return this.pf.bytes; }

  /** Point the player at a chapter. Does not start it. */
  async open(o: OpenOptions): Promise<void> {
    this.stopRetry();
    this.wantPlaying = false;
    for (const e of this.els) { e.pause(); e.removeAttribute('src'); e.load(); }
    this.pf.drop();
    this.act = 0;
    this.opts = o;
    this.manifest = o.manifest;
    this.chunk = o.startChunk;
    this.failures = 0;

    if (o.manifest && o.downloaded) this.mode = 'chapter';
    else if (o.manifest && o.packed && canPlayHls(this.au)) this.mode = 'hls';
    else if (o.manifest && o.packed) this.mode = 'chapter';
    else this.mode = 'chunk';
    this.ev.onMode(this.mode);

    if (this.mode === 'chunk') {
      this.pf.prime(o.ci, o.startChunk, o.chunkCount);
    } else {
      this.au.src = this.sourceUrl();
      this.au.load();
      this.seekChunk(o.startChunk, false);
    }
    this.setMetadata();
  }

  private sourceUrl(): string {
    const o = this.opts!;
    return this.mode === 'hls' ? chapterHlsUrl(o.key, o.ci) : chapterAudioUrl(o.key, o.ci);
  }

  /**
   * Start (or resume), healing a failed load rather than surrendering to it.
   *
   * The failure this exists for: on iOS, over the tunnel, a source that fails to
   * load rejects play() with NotSupportedError. The old reader painted that as an
   * error and stopped, so a five-second blip cost a manual tap.
   */
  async play(isRetry = false): Promise<boolean> {
    if (!isRetry) { this.failures = 0; this.stopRetry(); }
    this.wantPlaying = true;
    this.ev.onPlaying(true);
    if (this.mode === 'chunk' && !this.au.getAttribute('src')) {
      await this.loadChunk(this.chunk);
      return this.wantPlaying;
    }
    try {
      await this.au.play();
      this.failures = 0;
      this.ev.onWaiting(false);
      this.ev.onMessage(null);
      // The lock screen reads this, not the element: without it the controls show
      // a play button over audio that is already playing.
      if ('mediaSession' in navigator) navigator.mediaSession.playbackState = 'playing';
      this.setPositionState();
      return true;
    } catch (err) {
      return this.heal(err);
    }
  }

  pause(): void {
    this.stopRetry();
    this.wantPlaying = false;
    this.au.pause();
    this.ev.onPlaying(false);
    this.ev.onWaiting(false);
    if ('mediaSession' in navigator) navigator.mediaSession.playbackState = 'paused';
  }

  toggle(): void { void (this.wantPlaying ? this.pause() : this.play()); }

  /** Jump to a chunk. In chunk mode that is a new source; otherwise a seek. */
  seekChunk(i: number, report = true): void {
    const o = this.opts;
    if (!o) return;
    const target = Math.max(0, Math.min(i, o.chunkCount - 1));
    this.chunk = target;
    if (report) this.ev.onChunk(target);
    if (this.mode === 'chunk') {
      this.pf.prime(o.ci, target, o.chunkCount);
      if (this.wantPlaying) void this.loadChunk(target);
      else { this.au.pause(); this.au.removeAttribute('src'); }
      return;
    }
    if (!this.manifest) return;
    const t = clampTime(this.manifest, startOf(this.manifest, target));
    try {
      if (this.au.readyState >= 1) this.au.currentTime = t;
      else this.pendingSeek = t;
    } catch { this.pendingSeek = t; }
    if (this.wantPlaying) void this.au.play().catch((e) => void this.heal(e));
  }

  /** Skip by seconds where that means something, by a chunk where it does not. */
  nudge(seconds: number): void {
    if (this.mode === 'chunk' || !this.manifest) {
      this.seekChunk(this.chunk + (seconds > 0 ? 1 : -1));
      return;
    }
    const t = clampTime(this.manifest, (this.au.currentTime || 0) + seconds);
    try { this.au.currentTime = t; } catch { /* not loaded yet */ }
  }

  destroy(): void {
    this.stopRetry();
    this.pf.drop();
    for (const e of this.els) { e.pause(); e.removeAttribute('src'); e.remove(); }
  }

  // ---------------------------------------------------------------- internals
  private applyPendingSeek(e: HTMLAudioElement) {
    if (e !== this.au || this.pendingSeek == null) return;
    try { this.au.currentTime = this.pendingSeek; } catch { /* ignore */ }
    this.pendingSeek = null;
  }

  private onTime(e: HTMLAudioElement) {
    if (e !== this.au || this.mode === 'chunk' || !this.manifest) return;
    const i = chunkAt(this.manifest, this.au.currentTime);
    if (i !== this.chunk) { this.chunk = i; this.ev.onChunk(i); }
    this.setPositionState();
  }

  private onEnded() {
    const o = this.opts;
    if (!o) return;
    if (this.mode !== 'chunk') { this.ev.onChapterEnd(); return; }
    if (this.chunk >= o.chunkCount - 1) { this.ev.onChapterEnd(); return; }
    const next = this.chunk + 1;
    this.chunk = next;
    this.ev.onChunk(next);
    // The standby element already holds this chunk: swap and play, no network.
    if (this.standby.getAttribute('src') && this.pf.ready(o.ci, next)) {
      this.act ^= 1;
      this.pf.prime(o.ci, next, o.chunkCount);
      void this.au.play().then(() => this.preload(next + 1)).catch((e) => void this.heal(e));
      this.ev.onWaiting(false);
    } else {
      void this.loadChunk(next);
    }
  }

  private async loadChunk(i: number) {
    const o = this.opts;
    if (!o) return;
    this.pf.prime(o.ci, i, o.chunkCount);
    const url = await this.pf.take(o.ci, i);
    if (!o || this.opts !== o || this.chunk !== i) return;      // moved on
    if (!url) {
      // Not rendered yet. This is the renderer being behind, not an error.
      this.ev.onWaiting(true);
      this.retryTimer = setTimeout(() => {
        if (this.wantPlaying && this.chunk === i) void this.loadChunk(i);
      }, 1000);
      return;
    }
    this.ev.onWaiting(false);
    this.au.src = url;
    if (this.wantPlaying) {
      try { await this.au.play(); this.failures = 0; this.ev.onMessage(null); }
      catch (err) { await this.heal(err); }
    }
    this.preload(i + 1);
  }

  /** Put the next chunk into the element that is not playing. */
  private preload(i: number) {
    const o = this.opts;
    if (!o || i >= o.chunkCount) return;
    void this.pf.take(o.ci, i).then((u) => {
      if (!u || this.opts !== o) return;
      if (this.standby.getAttribute('src') !== u) { this.standby.src = u; this.standby.load(); }
    });
  }

  private async heal(err: unknown): Promise<boolean> {
    const verdict = classifyPlayError(err, this.au.error);
    this.failures++;
    // A streaming source that will not load is not worth a second argument: the
    // packed file is right there, and this is also the safety net for a browser
    // that claimed HLS support it does not have.
    if (verdict !== 'gesture' && this.mode === 'hls' && this.opts) {
      this.mode = 'chapter';
      this.ev.onMode('chapter');
      this.au.src = this.sourceUrl();
      this.au.load();
      this.seekChunk(this.chunk, false);
      this.failures = 0;
      return this.play(true);
    }
    const plan = playRetry(verdict, this.failures);
    if (!plan.retry) {
      this.wantPlaying = false;
      this.ev.onPlaying(false);
      this.ev.onMessage(plan.message);
      return false;
    }
    // Stay "playing": the reader has not stopped, it is re-reaching for the
    // audio. The chunk, the chapter and the scroll position all stay put.
    this.ev.onWaiting(true);
    const refetch = shouldRefetch(verdict, this.au.error);
    const o = this.opts;
    const at = this.chunk;
    this.stopRetry();
    this.retryTimer = setTimeout(() => {
      if (!this.wantPlaying || this.opts !== o || this.chunk !== at) return;
      if (this.mode === 'chunk') {
        if (refetch) this.pf.drop(o!.ci);     // the blob we were handed is damaged
        void this.loadChunk(at);
      } else {
        const t = this.au.currentTime;
        if (refetch) { this.au.src = this.sourceUrl(); this.au.load(); }
        this.pendingSeek = t;
        void this.play(true);
      }
    }, plan.delay);
    return false;
  }

  private stopRetry() {
    if (this.retryTimer) { clearTimeout(this.retryTimer); this.retryTimer = null; }
  }

  // -------------------------------------------------------------- MediaSession
  private meta: {book: string; chapter: string} = {book: '', chapter: ''};

  setMedia(book: string, chapter: string) { this.meta = {book, chapter}; this.setMetadata(); }

  private setMetadata() {
    if (!('mediaSession' in navigator) || typeof MediaMetadata === 'undefined') return;
    try {
      navigator.mediaSession.metadata = new MediaMetadata({
        title: this.meta.chapter || 'narrator',
        artist: this.meta.book,
        album: this.meta.book,
        artwork: [
          {src: '/icon-192.png', sizes: '192x192', type: 'image/png'},
          {src: '/icon-512.png', sizes: '512x512', type: 'image/png'},
        ],
      });
    } catch { /* older engines */ }
  }

  private setPositionState() {
    const ms = navigator.mediaSession;
    if (!ms?.setPositionState || this.mode === 'chunk') return;
    const d = this.duration;
    if (!d || !isFinite(d)) return;
    try {
      ms.setPositionState({
        duration: d,
        playbackRate: this.au.playbackRate || 1,
        position: Math.max(0, Math.min(this.au.currentTime || 0, d)),
      });
    } catch { /* Safari is picky about position > duration */ }
  }

  /**
   * Arm the lock screen. Must be called from a user gesture on iOS, which is why
   * it is not done at construction.
   */
  armMediaSession(handlers: {prev: () => void; next: () => void}) {
    const ms = navigator.mediaSession;
    if (!ms) return;
    const set = (a: MediaSessionAction, f: MediaSessionActionHandler) => {
      try { ms.setActionHandler(a, f); } catch { /* unsupported action */ }
    };
    set('play', () => void this.play());
    set('pause', () => this.pause());
    set('stop', () => this.pause());
    set('previoustrack', handlers.prev);
    set('nexttrack', handlers.next);
    set('seekbackward', (d) => this.nudge(-(d.seekOffset ?? 15)));
    set('seekforward', (d) => this.nudge(d.seekOffset ?? 30));
    set('seekto', (d) => {
      if (this.mode === 'chunk' || d.seekTime == null || !this.manifest) return;
      try { this.au.currentTime = clampTime(this.manifest, d.seekTime); } catch { /* ignore */ }
    });
    ms.playbackState = this.wantPlaying ? 'playing' : 'paused';
  }
}
