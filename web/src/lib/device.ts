/**
 * Which device this is.
 *
 * Nothing in the reader knew. There is one server-side session and one position
 * record per book, and a `position` event says where the reading moved to but
 * never *who* moved it — so "is this my own echo coming back?" was answered by
 * guessing: if the event lands within a couple of chunks of where this device
 * already is, it is probably ours (`SLACK` in lib/live.ts). That guess is wrong
 * in both directions. It swallows a genuine two-chunk move made on the laptop,
 * and it waves through this device's own report the moment the playhead has
 * drifted three chunks by the time the event arrives — which, on a phone with a
 * tunnel between it and the box, is most of them.
 *
 * So every request that can cause a position write carries this id, the server
 * stores it with the record, and it comes back on the event. An event carrying
 * our own id is our own echo, whatever it says and however far it has moved:
 * an exact test instead of a distance heuristic.
 *
 * The id is per browser profile, not per person and not per book. It is a
 * `crypto.randomUUID()` minted once and kept in `localStorage`, which means a
 * cleared site-data store mints a new one — and that is harmless, because the id
 * is only ever compared for equality against itself. Nothing is keyed on it, no
 * history is lost when it changes, and a device that forgets its id is simply
 * back to the chunk-distance backstop for one page load.
 *
 * **Every storage access here is wrapped**, and not as a formality: on iOS in a
 * private window, and with site data blocked, reading `localStorage` throws on
 * property *access* rather than returning null. An uncaught throw at this point
 * would be thrown out of the first call that identifies the device, which is on
 * the path of every position report — so the reader would lose position syncing
 * entirely on exactly the browsers it is most often opened in. When storage is
 * unavailable the id lives in this module for the life of the page: stable
 * enough to suppress this session's own echoes, forgotten on reload.
 */

/** The header carrying the id. The server stores it on the position record. */
export const DEVICE_HEADER = 'X-Narrator-Device';
/** The header carrying the label. Display only — never logic, on either side. */
export const DEVICE_NAME_HEADER = 'X-Narrator-Device-Name';

/** Where the id is kept, when it can be kept at all. */
export const DEVICE_KEY = 'narrator.device';

/** The slice of `Storage` this module uses, so a test can hand one over. */
export interface Store {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/**
 * `localStorage`, or null if touching it is not allowed here.
 *
 * `typeof` first because the property genuinely does not exist under vitest's
 * node environment, and a bare reference would be a `ReferenceError` rather than
 * something the `catch` below could turn into "no storage, then".
 */
function localStore(): Store | null {
  try {
    if (typeof localStorage === 'undefined' || !localStorage) return null;
    return localStorage;
  } catch {
    return null;                        // blocked site data: throws on access
  }
}

/**
 * A fresh id, from the platform's UUID generator when it has one.
 *
 * The fallback is not a security claim and does not need to be: two devices
 * colliding here would mean one of them ignoring the other's position events,
 * which is the failure this module already tolerates when storage is gone. It
 * exists because `crypto.randomUUID` is unavailable over plain http on some
 * browsers, and the reader is served over a tailnet without TLS.
 */
function mintId(): string {
  try {
    const uuid = globalThis.crypto?.randomUUID?.();
    if (uuid) return uuid;
  } catch { /* no crypto, or a policy that refuses it */ }
  return `d-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/**
 * The id in `store`, or a new one written back to it.
 *
 * Pure in its inputs, which is the whole reason it is separate from `deviceId`:
 * the cases worth testing are a store that throws, a store that is not there and
 * a store that already holds an id, and none of those are reachable through a
 * function that reads the real `localStorage` once per page load.
 *
 * A write that fails is not an error: the caller gets a usable id either way,
 * and the only cost of losing the write is a new id on the next page load.
 */
export function resolveDeviceId(store: Store | null, mint: () => string = mintId): string {
  try {
    const held = store?.getItem(DEVICE_KEY);
    if (typeof held === 'string' && held.length > 0) return held;
  } catch { /* a getItem that throws is a store that is not there */ }
  const id = mint();
  try {
    store?.setItem(DEVICE_KEY, id);
  } catch { /* quota, private mode: the id still works for this page */ }
  return id;
}

/** The module-level fallback, and the memo. Minted once per page load. */
let cached: string | null = null;

/**
 * This device's id. Stable for the life of the page, and across page loads
 * wherever `localStorage` survives.
 */
export function deviceId(): string {
  if (cached === null) cached = resolveDeviceId(localStore());
  return cached;
}

/**
 * A short guess at what kind of device this is, from a user-agent string.
 *
 * Only ever shown to a person — "moved on another device · iPhone" reads better
 * than a UUID, and the point of the line is to tell Fernando which of his own
 * devices moved. **Nothing branches on it.** That is deliberate: user-agent
 * sniffing is wrong often enough that any logic built on it would be a bug
 * waiting for a browser update, whereas a label that says "Mac" for an iPad in
 * desktop mode (which is what iPadOS Safari reports) is merely a bit wrong on
 * one line of one screen.
 *
 * `Android` is tested before `Linux` because every Android user agent says
 * Linux, and `iPad` before `Mac` for the same reason in the other direction.
 */
export function labelFor(ua: string, platform?: string): string {
  const s = `${platform ?? ''} ${ua}`;
  if (/iPhone|iPod/i.test(s)) return 'iPhone';
  if (/iPad/i.test(s)) return 'iPad';
  if (/Android/i.test(s)) return 'Android';
  if (/Mac OS X|Macintosh|macOS/i.test(s)) return 'Mac';
  if (/Windows/i.test(s)) return 'Windows';
  if (/Linux|X11|CrOS/i.test(s)) return 'Linux';
  return 'device';
}

/** `labelFor` against this browser, falling back to "device" when asking fails. */
export function deviceLabel(): string {
  try {
    const nav = globalThis.navigator as
      (Navigator & {userAgentData?: {platform?: string}}) | undefined;
    if (!nav) return 'device';
    return labelFor(nav.userAgent ?? '', nav.userAgentData?.platform);
  } catch {
    return 'device';
  }
}
