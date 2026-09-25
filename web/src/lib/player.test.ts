/**
 * The player against two fake <audio> elements.
 *
 * What is pinned here is the part the platform punishes: which element plays,
 * and whether play() is called inside the event that allowed it. iOS lets a
 * backgrounded page keep playing the element it is already playing, and little
 * else - so a chapter boundary that swaps elements, or that awaits the network
 * before play(), is a book that stops at every chapter with the screen off.
 */
import {afterEach, beforeEach, describe, expect, test, vi} from 'vitest';
import type {Manifest} from './manifest';
import {Player, type OpenOptions, type PlayerEvents, type PlayMode} from './player';

/** Just enough of HTMLAudioElement, with every play() call recorded. */
class FakeAudio {
  static hls = false;
  attrs = new Map<string, string>();
  listeners = new Map<string, (() => void)[]>();
  paused = true;
  currentTime = 0;
  readyState = 0;
  playbackRate = 1;
  duration = NaN;
  error: {code: number} | null = null;
  preload = '';
  hidden = false;
  plays: string[] = [];
  /** What the next play() does; resolves by default. */
  nextPlay: (() => Promise<void>) | null = null;

  get src() { return this.attrs.get('src') ?? ''; }
  set src(v: string) { this.attrs.set('src', v); this.readyState = 0; }
  getAttribute(k: string) { return this.attrs.get(k) ?? null; }
  setAttribute(k: string, v: string) { this.attrs.set(k, v); }
  removeAttribute(k: string) { this.attrs.delete(k); }
  addEventListener(t: string, f: () => void) {
    this.listeners.set(t, [...(this.listeners.get(t) ?? []), f]);
  }
  fire(t: string) { for (const f of this.listeners.get(t) ?? []) f(); }
  canPlayType(t: string) { return FakeAudio.hls && t === 'application/vnd.apple.mpegurl' ? 'maybe' : ''; }
  load() { /* the fake has nothing to fetch */ }
  play() {
    this.plays.push(this.src);
    const run = this.nextPlay;
    this.nextPlay = null;
    if (run) return run();
    this.paused = false;
    return Promise.resolve();
  }
  pause() { this.paused = true; }
  remove() { /* detached */ }
}

let made: FakeAudio[];
const blobName = new WeakMap<Blob, string>();

beforeEach(() => {
  made = [];
  FakeAudio.hls = false;
  vi.stubGlobal('Audio', class extends FakeAudio { constructor() { super(); made.push(this); } });
  vi.stubGlobal('document', {body: {appendChild: () => {}}});
  // Chunk wavs: a blob per URL, named so a test can say which chunk is where.
  vi.stubGlobal('fetch', vi.fn(async (u: string) => {
    const b = new Blob([u]);
    blobName.set(b, u);
    return {ok: true, blob: async () => b};
  }));
  vi.spyOn(URL, 'createObjectURL').mockImplementation((b) => `blob:${blobName.get(b as Blob)}`);
  vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => {});
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

const manifest = (ci: number, chunks = 3): Manifest => ({
  book: 'lom', chapter: ci, title: `${ci}`, chunks,
  starts: Array.from({length: chunks}, (_, i) => i * 10), duration: chunks * 10,
});

const opts = (ci: number, p: Partial<OpenOptions> = {}): OpenOptions => ({
  key: 'lom', ci, chunkCount: 3, manifest: manifest(ci), packed: true, downloaded: false,
  startChunk: 0, ...p,
});

function player() {
  const log = {
    playing: [] as boolean[], waiting: [] as boolean[], modes: [] as PlayMode[],
    chunks: [] as number[], ends: 0, advanced: [] as number[],
  };
  const ev: PlayerEvents = {
    onChunk: (i) => log.chunks.push(i),
    onMode: (m) => log.modes.push(m),
    onPlaying: (p) => log.playing.push(p),
    onWaiting: (w) => log.waiting.push(w),
    onMessage: () => {},
    onChapterEnd: () => { log.ends++; },
    onAdvance: (ci) => log.advanced.push(ci),
  };
  const p = new Player(ev);
  const active = () => made[(p as unknown as {act: number}).act];
  return {p, log, active};
}

const settle = () => new Promise((r) => setTimeout(r, 0));

describe('open', () => {
  test('stops, and says it stopped', async () => {
    const {p, log} = player();
    await p.open(opts(1));
    await p.play();
    expect(log.playing.at(-1)).toBe(true);
    await p.open(opts(2));
    // The bar used to keep showing "pause" over audio open() had just stopped.
    expect(log.playing.at(-1)).toBe(false);
    expect(log.waiting.at(-1)).toBe(false);
    expect(p.playing).toBe(false);
  });

  test('keeps the element that holds the audio session', async () => {
    const {p, active} = player();
    (p as unknown as {act: number}).act = 1;
    await p.open(opts(1));
    expect(active()).toBe(made[1]);
    expect(made[1].src).toBe('/api/chapters/1.m4a?book=lom');
    expect(made[0].getAttribute('src')).toBeNull();
  });
});

describe('the chapter boundary', () => {
  test('carries on into the prepared chapter inside the ended event, on the same element', async () => {
    const {p, log, active} = player();
    await p.open(opts(1));
    await p.play();
    const el = active();
    p.prepareNext(opts(2));
    const before = el.plays.length;

    el.fire('ended');
    // No await between the event and play(): that is what iOS allows.
    expect(el.plays.length).toBe(before + 1);
    expect(el.plays.at(-1)).toBe('/api/chapters/2.m4a?book=lom');
    expect(active()).toBe(el);
    expect(log.advanced).toEqual([2]);
    expect(log.ends).toBe(0);
    expect(p.playing).toBe(true);
    expect(made.find((e) => e !== el)?.getAttribute('src')).toBeNull();
  });

  test('a chapter still rendering starts its first chunk on the same element', async () => {
    const {p, active} = player();
    await p.open(opts(1));
    await p.play();
    const el = active();
    p.prepareNext(opts(2, {manifest: null, packed: false}));
    await settle();                         // the prefetch lands
    el.fire('ended');
    expect(active()).toBe(el);
    expect(el.plays.at(-1)).toBe('blob:/api/chunk/2/0.wav');
    expect(p.mode).toBe('chunk');
  });

  test('nothing prepared: the caller is told, and nothing plays', async () => {
    const {p, log, active} = player();
    await p.open(opts(1));
    await p.play();
    const el = active();
    const before = el.plays.length;
    el.fire('ended');
    expect(log.ends).toBe(1);
    expect(el.plays.length).toBe(before);
  });

  test('a pause on the last word is still a pause', async () => {
    const {p, log, active} = player();
    await p.open(opts(1));
    await p.play();
    p.prepareNext(opts(2));
    p.pause();
    active().fire('ended');
    expect(log.advanced).toEqual([]);
    expect(log.ends).toBe(1);
  });

  test('something prepared for another chapter is not carried on into', async () => {
    const {p, log, active} = player();
    await p.open(opts(1));
    await p.play();
    p.prepareNext(opts(5));
    active().fire('ended');
    expect(log.advanced).toEqual([]);
    expect(log.ends).toBe(1);
  });
});

describe('a download landing under the chapter being played', () => {
  test('HLS moves onto the stored file: same element, same second, still playing', async () => {
    FakeAudio.hls = true;
    const {p, log, active} = player();
    await p.open(opts(1));
    await p.play();
    const el = active();
    expect(p.mode).toBe('hls');
    el.currentTime = 17.5;

    p.useDownloaded('lom', 1, manifest(1));
    expect(active()).toBe(el);
    expect(el.src).toBe('/api/chapters/1.m4a?book=lom');
    expect(el.plays.at(-1)).toBe('/api/chapters/1.m4a?book=lom');
    expect(log.modes.at(-1)).toBe('chapter');
    el.readyState = 1;
    el.fire('loadedmetadata');
    expect(el.currentTime).toBe(17.5);
    expect(made.filter((e) => !e.paused)).toEqual([el]);
  });

  test('paused, it moves and stays paused', async () => {
    FakeAudio.hls = true;
    const {p, active} = player();
    await p.open(opts(1));
    const el = active();
    const before = el.plays.length;
    p.useDownloaded('lom', 1, manifest(1));
    expect(el.src).toBe('/api/chapters/1.m4a?book=lom');
    expect(el.plays.length).toBe(before);
  });

  test('another chapter finishing is not this one', async () => {
    FakeAudio.hls = true;
    const {p, active} = player();
    await p.open(opts(1));
    p.useDownloaded('lom', 2, manifest(2));
    expect(active().src).toBe('/api/chapters/1.m3u8?book=lom');
  });
});

describe('a continuation that outlived its source', () => {
  test('a play() the next open interrupted does not start the next chapter', async () => {
    FakeAudio.hls = true;
    const {p, log, active} = player();
    await p.open(opts(1));
    let reject!: (e: unknown) => void;
    active().nextPlay = () => new Promise<void>((_, r) => { reject = r; });
    const pending = p.play();

    await p.open(opts(2));
    // What open()'s reset does to a pending play() on a real element.
    reject(new DOMException('interrupted', 'AbortError'));
    await pending;
    await settle();

    // It used to be read as the new chapter's failure: HLS -> file, and play.
    expect(p.mode).toBe('hls');
    expect(p.playing).toBe(false);
    expect(log.playing.at(-1)).toBe(false);
    expect(made.every((e) => e.paused)).toBe(true);
  });

  test('the element that is not active is silenced if it ever starts', async () => {
    const {p, active} = player();
    await p.open(opts(1));
    const other = made.find((e) => e !== active())!;
    other.paused = false;
    other.fire('playing');
    expect(other.paused).toBe(true);
  });
});

describe('chunk mode', () => {
  test('each boundary plays the next chunk, not whatever the standby last held', async () => {
    const {p, active} = player();
    await p.open(opts(1, {manifest: null, packed: false}));
    await p.play();
    await settle();
    const played = () => made.flatMap((e) => e.plays);
    expect(played()).toEqual(['blob:/api/chunk/1/0.wav']);

    active().fire('ended');
    await settle();
    active().fire('ended');
    await settle();
    expect(played().sort()).toEqual(
      ['blob:/api/chunk/1/0.wav', 'blob:/api/chunk/1/1.wav', 'blob:/api/chunk/1/2.wav']);
    expect(active().src).toBe('blob:/api/chunk/1/2.wav');
  });
});
