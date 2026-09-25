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
  /** The chapter ended and nothing was ready to carry on into: the caller opens the next. */
  onChapterEnd: () => void;
  /**
   * The chapter ended and the player has already carried on into the one that
   * was prepared (`prepareNext`) - audio first, on the same element. Everything
   * else about the new chapter (its words, the server, the lock screen title)
   * is the caller's to catch up on, *after* the fact.
   */
  onAdvance?: (ci: number) => void;
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

/** Which source a chapter plays from - the preference order in the header. */
function pickMode(o: OpenOptions, el: HTMLAudioElement): PlayMode {
  if (o.manifest && o.downloaded) return 'chapter';
  if (o.manifest && o.packed && canPlayHls(el)) return 'hls';
  if (o.manifest && o.packed) return 'chapter';
  return 'chunk';
}

const mediaSession = (): MediaSession | undefined =>
  (typeof navigator !== 'undefined' && 'mediaSession' in navigator
    ? navigator.mediaSession : undefined);

export class Player {
  private els: [HTMLAudioElement, HTMLAudioElement];
  private act = 0;
  private pf: ChunkPrefetcher;
  private manifest: Manifest | null = null;
  private opts: OpenOptions | null = null;
  private failures = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private wantPlaying = false;
  /**
   * Which source the player is on, bumped every time that changes.
   *
   * Everything here that waits - a `play()` promise, a prefetch, a retry timer -
   * finishes after the world may have moved on, and each of them used to act on
   * whatever the player held *by then*. A `play()` interrupted by the reset in
   * `open` rejects with AbortError, and `heal` took that as the *new* chapter's
   * failure: it fell back from HLS and started the new chapter playing,
   * uninvited. A chunk-mode wait whose timer handle had been overwritten (so
   * `stopRetry` could no longer cancel it) would load a chunk wav over the
   * chapter file of the next open. So a continuation captures this, and does
   * nothing if it has changed.
   */
  private gen = 0;
  /** The chapter to carry on into when this one ends - see `prepareNext`. */
  private next: OpenOptions | null = null;
  /**
   * The chunk the standby element holds, in chunk mode. The swap at a chunk
   * boundary used to check only that the standby had *a* source and that the
   * next chunk was in the prefetcher, so a standby still holding a chunk it had
   * already played was swapped in and played again.
   */
  private standbyChunk: number | null = null;

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
      /* The element that is not active has no business making a sound, and if it
         ever does it is the worst failure available: two voices at once, and a
         pause button that only reaches one of them. Every path here is meant to
         make that impossible; this is the backstop in case one does not. */
      e.addEventListener('playing', () => { if (e !== this.au) e.pause(); });
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

  /**
   * Point the player at a chapter. Does not start it - and says so.
   *
   * It stops whatever was playing, so it reports stopped: without that the bar
   * kept showing "pause" over silence and the lock screen kept its play state
   * until something else happened to correct them.
   *
   * The active element stays the active element. It used to be forced back to
   * the first one, and on iOS the two are not interchangeable: the one that was
   * playing is the one holding the audio session, and a backgrounded page that
   * moves to the other may not be allowed to start it.
   */
  async open(o: OpenOptions): Promise<void> {
    this.stopRetry();
    this.gen++;
    const was = this.wantPlaying;
    this.wantPlaying = false;
    for (const e of this.els) { e.pause(); e.removeAttribute('src'); e.load(); }
    this.standbyChunk = null;
    this.pf.drop();
    this.next = null;
    this.opts = o;
    this.manifest = o.manifest;
    this.chunk = o.startChunk;
    this.failures = 0;
    this.pendingSeek = null;
    if (was) this.ev.onPlaying(false);
    this.ev.onWaiting(false);
    this.setPlaybackState();

    this.mode = pickMode(o, this.au);
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

  /**
   * What to carry on into when this chapter ends, worked out while it plays.
   *
   * The reason is iOS. A PWA with its screen off may keep playing audio it is
   * already playing, and very little else: when a chapter ended, the reader
   * used to fetch the next chapter's words, tell the server, fetch its
   * manifest, ask Cache Storage what was downloaded, open it - resetting both
   * elements - and only then call play(), seconds and several awaits after the
   * `ended` event. Backgrounded, that play() was refused and the book stopped
   * at every chapter boundary. With the next source known in advance, the
   * boundary is a src change and a play() inside the `ended` handler itself,
   * on the element that holds the audio session.
   *
   * Null forgets it (the last chapter, another book).
   */
  prepareNext(o: OpenOptions | null): void {
    this.next = o;
    // A chapter still rendering starts from chunk wavs: have the first few in
    // hand, so that boundary costs no network either. The prefetcher trims per
    // chapter, so this never touches what is playing.
    if (o && pickMode(o, this.au) === 'chunk') this.pf.prime(o.ci, o.startChunk, o.chunkCount);
  }

  /** Has chapter `ci` of `key` been prepared to carry on into? */
  preparedFor(key: string, ci: number): boolean {
    return !!this.next && this.next.key === key && this.next.ci === ci;
  }

  /**
   * Go straight on into the prepared chapter, without leaving the element.
   *
   * Synchronous up to and including the play() call, which is the point: from
   * an `ended` handler or a lock-screen "next" this is still inside the event
   * the platform allowed audio for. Returns false if nothing was prepared for
   * the chapter after this one; the caller then opens it the slow way.
   */
  advance(): boolean {
    const n = this.next;
    const o = this.opts;
    if (!n || !o || n.key !== o.key || n.ci !== o.ci + 1) return false;
    this.next = null;
    this.stopRetry();
    const g = ++this.gen;
    const el = this.au;
    this.spareOff();
    this.pf.drop(o.ci);
    this.opts = n;
    this.manifest = n.manifest;
    this.chunk = n.startChunk;
    this.failures = 0;
    this.pendingSeek = null;
    this.mode = pickMode(n, el);
    this.ev.onMode(this.mode);

    if (this.mode === 'chunk') {
      // The blob if the prefetch has it, the chunk itself if not: a src now
      // beats a better src after an await.
      el.src = this.pf.readyUrl(n.ci, n.startChunk) ?? this.pf.url(n.ci, n.startChunk);
      this.pf.prime(n.ci, n.startChunk, n.chunkCount);
    } else {
      el.src = this.sourceUrl();
      if (this.manifest && n.startChunk > 0)
        this.pendingSeek = clampTime(this.manifest, startOf(this.manifest, n.startChunk));
    }
    this.wantPlaying = true;
    el.play().then(() => {
      if (g !== this.gen) return;
      this.ev.onWaiting(false);
      this.setPlaybackState();
      this.setPositionState();
      if (this.mode === 'chunk') this.preload(this.chunk + 1);
    }, (e) => { if (g === this.gen) void this.heal(e); });
    // No onChunk: that is a playhead report, and it would race the caller's
    // `/api/open` for the new chapter - a chunk filed under the old one.
    this.ev.onAdvance?.(n.ci);
    return true;
  }

  /**
   * The chapter in hand has just finished downloading: move onto the stored
   * copy without anybody noticing.
   *
   * Same element, same moment, same play state - only the source changes, so
   * there is nothing for the ear to notice but the network going away. Before
   * this, nothing happened at all: the chapter kept streaming (or kept playing
   * chunk by chunk at the render frontier) until it was re-opened by hand.
   *
   * A streamed m4a (a browser with no HLS) is already the stored URL - the
   * service worker answers it from Cache Storage from here on - so that case
   * only notes the fact. Chunk mode needs the manifest to know where in the
   * file the current chunk is, and is left alone without one.
   */
  useDownloaded(key: string, ci: number, manifest: Manifest | null): void {
    const o = this.opts;
    if (!o || o.key !== key || o.ci !== ci || o.downloaded) return;
    const m = manifest ?? this.manifest;
    if (!m) return;
    this.opts = {...o, downloaded: true, packed: true, manifest: m};
    if (this.mode === 'chapter') return;
    const el = this.au;
    const into = Math.max(0, el.currentTime || 0);
    const t = this.mode === 'chunk' ? startOf(m, this.chunk) + into : into;
    this.stopRetry();
    const g = ++this.gen;
    this.spareOff();
    if (this.mode === 'chunk') this.pf.drop(ci);
    this.manifest = m;
    this.mode = 'chapter';
    this.ev.onMode('chapter');
    el.src = this.sourceUrl();
    this.pendingSeek = clampTime(m, t);
    el.load();
    if (this.wantPlaying) {
      el.play().then(() => { if (g === this.gen) this.ev.onWaiting(false); },
                     (e) => { if (g === this.gen) void this.heal(e); });
    }
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
    const g = this.gen;
    try {
      await this.au.play();
      if (g !== this.gen) return this.wantPlaying;
      this.failures = 0;
      this.ev.onWaiting(false);
      this.ev.onMessage(null);
      // The lock screen reads this, not the element: without it the controls show
      // a play button over audio that is already playing.
      this.setPlaybackState();
      this.setPositionState();
      return true;
    } catch (err) {
      // A play() the source changed under is not this source's failure.
      return g === this.gen ? this.heal(err) : false;
    }
  }

  pause(): void {
    this.stopRetry();
    this.wantPlaying = false;
    this.au.pause();
    this.ev.onPlaying(false);
    this.ev.onWaiting(false);
    this.setPlaybackState();
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
      else { this.gen++; this.au.pause(); this.au.removeAttribute('src'); }
      return;
    }
    if (!this.manifest) return;
    const t = clampTime(this.manifest, startOf(this.manifest, target));
    try {
      if (this.au.readyState >= 1) this.au.currentTime = t;
      else this.pendingSeek = t;
    } catch { this.pendingSeek = t; }
    const g = this.gen;
    if (this.wantPlaying) void this.au.play().catch((e) => { if (g === this.gen) void this.heal(e); });
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
    this.gen++;
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
    if (this.mode !== 'chunk' || this.chunk >= o.chunkCount - 1) {
      // Carry on into the prepared chapter right here, inside the event - see
      // `prepareNext`. Only if something was playing: a pause that landed on
      // the last word is still a pause.
      if (!(this.wantPlaying && this.advance())) this.ev.onChapterEnd();
      return;
    }
    const next = this.chunk + 1;
    this.chunk = next;
    this.ev.onChunk(next);
    // The standby element already holds this chunk: swap and play, no network.
    if (this.standbyChunk === next && this.standby.getAttribute('src')) {
      this.act ^= 1;
      this.standbyChunk = null;
      const g = ++this.gen;
      this.pf.prime(o.ci, next, o.chunkCount);
      this.au.play().then(() => { if (g === this.gen) this.preload(next + 1); },
                          (e) => { if (g === this.gen) void this.heal(e); });
      this.ev.onWaiting(false);
    } else {
      void this.loadChunk(next);
    }
  }

  private async loadChunk(i: number) {
    const o = this.opts;
    if (!o) return;
    const g0 = this.gen;
    this.pf.prime(o.ci, i, o.chunkCount);
    const url = await this.pf.take(o.ci, i);
    // Moved on: another chunk, another chapter, or another source altogether.
    if (this.opts !== o || this.chunk !== i || this.mode !== 'chunk' || this.gen !== g0) return;
    if (!url) {
      // Not rendered yet. This is the renderer being behind, not an error.
      this.ev.onWaiting(true);
      this.stopRetry();
      this.retryTimer = setTimeout(() => {
        if (this.wantPlaying && this.chunk === i && this.gen === g0) void this.loadChunk(i);
      }, 1000);
      return;
    }
    this.ev.onWaiting(false);
    const g = ++this.gen;
    this.au.src = url;
    if (this.wantPlaying) {
      try {
        await this.au.play();
        if (g !== this.gen) return;
        this.failures = 0; this.ev.onMessage(null);
      } catch (err) { if (g === this.gen) await this.heal(err); }
    }
    if (g === this.gen) this.preload(i + 1);
  }

  /** Put the next chunk into the element that is not playing. */
  private preload(i: number) {
    const o = this.opts;
    if (!o || i >= o.chunkCount) return;
    const g = this.gen;
    void this.pf.take(o.ci, i).then((u) => {
      if (!u || this.opts !== o || this.gen !== g || this.mode !== 'chunk') return;
      if (this.standbyChunk === i && this.standby.getAttribute('src') === u) return;
      this.standby.src = u;
      this.standby.load();
      this.standbyChunk = i;
    });
  }

  /** Empty the element that is not playing. */
  private spareOff() {
    const spare = this.standby;
    spare.pause();
    spare.removeAttribute('src');
    this.standbyChunk = null;
  }

  private async heal(err: unknown): Promise<boolean> {
    const verdict = classifyPlayError(err, this.au.error);
    this.failures++;
    // A streaming source that will not load is not worth a second argument: the
    // packed file is right there, and this is also the safety net for a browser
    // that claimed HLS support it does not have.
    if (verdict !== 'gesture' && this.mode === 'hls' && this.opts) {
      this.gen++;
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
      this.ev.onWaiting(false);
      this.ev.onMessage(plan.message);
      this.setPlaybackState();
      return false;
    }
    // Stay "playing": the reader has not stopped, it is re-reaching for the
    // audio. The chunk, the chapter and the scroll position all stay put.
    this.ev.onWaiting(true);
    const refetch = shouldRefetch(verdict, this.au.error);
    const o = this.opts;
    const at = this.chunk;
    const g = this.gen;
    this.stopRetry();
    this.retryTimer = setTimeout(() => {
      if (!this.wantPlaying || this.opts !== o || this.chunk !== at || this.gen !== g) return;
      if (this.mode === 'chunk') {
        if (refetch) this.pf.drop(o!.ci);     // the blob we were handed is damaged
        void this.loadChunk(at);
      } else {
        const t = this.au.currentTime;
        if (refetch) { this.gen++; this.au.src = this.sourceUrl(); this.au.load(); }
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
    if (!mediaSession() || typeof MediaMetadata === 'undefined') return;
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

  /** The lock screen's play/pause glyph, from what the player means to be doing. */
  private setPlaybackState() {
    const ms = mediaSession();
    if (ms) ms.playbackState = this.wantPlaying ? 'playing' : 'paused';
  }

  private setPositionState() {
    const ms = mediaSession();
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
    const ms = mediaSession();
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
    this.setPlaybackState();
  }
}
