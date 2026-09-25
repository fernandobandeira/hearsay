/**
 * Where a book opens.
 *
 * Three parties can claim to know the reading position, and they disagree for
 * good reasons:
 *
 *   server   `/api/load` returns the vault's record for this book - written by
 *            whichever device was reading last, including another one.
 *   queued   a position this device could not deliver. It exists *because* the
 *            POST failed, so the server's record cannot contain it.
 *   device   the last position this device reported successfully, kept in
 *            localStorage. It is the only thing left when the server is gone.
 *
 * The rule is "newest wins", with one honest caveat: the server stamps its record
 * with a naive local ISO timestamp, and the container's clock may not be in the
 * browser's timezone. A server stamp that claims to be in the future is therefore
 * not evidence of anything, so a queued position - which is known-undelivered -
 * keeps precedence over it.
 */

export interface ServerPosition {
  chapter: number;
  chunk: number;
  /** naive ISO local time, as `save_position()` writes it */
  updated?: string | null;
}

/** A position waiting in the outbox: by construction the server has not seen it. */
export interface QueuedPosition {chapter: number; chunk: number; ts: number}

/** The last position this device reported successfully. */
export interface DevicePosition {chapter: number; chunk: number}

export type ResumeSource = 'server' | 'queued' | 'device' | 'none';

export interface Resume {chapter: number; chunk: number; from: ResumeSource}

/** A server stamp this far into the future is a clock difference, not news. */
export const FUTURE_SLACK_MS = 5 * 60_000;

const whole = (n: unknown): number => {
  const v = Math.floor(Number(n));
  return Number.isFinite(v) && v > 0 ? v : 0;
};

/**
 * `2026-09-11T14:22:07` with no zone, read as local time - which is what it is
 * when the server and the browser share one. Returns null for anything
 * unparseable, and for a stamp far enough ahead of `now` to be a clock skew.
 */
export function parseUpdated(s: string | null | undefined, now = Date.now()): number | null {
  if (!s) return null;
  const t = Date.parse(s);
  if (!Number.isFinite(t)) return null;
  if (t > now + FUTURE_SLACK_MS) return null;
  return t;
}

/**
 * Resolve the three claims into one place to open.
 *
 *   1. A queued position wins, unless the server's record is provably newer.
 *   2. Otherwise the server's record, which is the cross-device truth.
 *   3. Otherwise this device's last known position (the offline case).
 *   4. Otherwise the beginning.
 */
export function resolveResume(
  {server, queued, device, now = Date.now()}: {
    server?: ServerPosition | null;
    queued?: QueuedPosition | null;
    device?: DevicePosition | null;
    now?: number;
  },
): Resume {
  const at = (p: {chapter: number; chunk: number}, from: ResumeSource): Resume =>
    ({chapter: whole(p.chapter), chunk: whole(p.chunk), from});

  if (queued) {
    const serverMs = server ? parseUpdated(server.updated, now) : null;
    const serverIsNewer = server != null && serverMs != null && serverMs > queued.ts;
    if (!serverIsNewer) return at(queued, 'queued');
  }
  if (server) return at(server, 'server');
  if (device) return at(device, 'device');
  return {chapter: 0, chunk: 0, from: 'none'};
}

interface Spot {chapter: number; chunk: number}
const same = (a: Spot, b: Spot) => a.chapter === b.chapter && a.chunk === b.chunk;

/**
 * The fast open painted `fast` from this device alone; `/api/load` has since
 * resolved `at`. Stay where the page is, or ask?
 *
 * Never "jump", which is what it used to do: a book opened on the laptop sat on
 * the page it painted from localStorage for a second and then leapt to where the
 * phone had got to - or, worse, the fast open had already told the server this
 * device's stale position and written it over the phone's newer one, so nothing
 * moved at all and the phone's progress was simply gone. He wants to be asked.
 *
 *   the reader already moved      stay: that was a choice, and it is theirs
 *   the server agrees             stay
 *   not the server's record       stay: a queued position is this device's own
 *   ahead of the page, or newer   offer - someone read on somewhere else
 *   than this device's last write
 *   behind, and older             stay: this device read further, later
 *
 * `serverMs` is the record's `updated_ms`, `deviceMs` when this device last
 * stored its own position. Either missing means "cannot tell", which offers:
 * asking is the recoverable mistake.
 */
export function afterFastOpen({fast, here, at, serverMs, deviceMs}: {
  fast: Spot; here: Spot; at: Resume; serverMs?: number | null; deviceMs?: number | null;
}): 'stay' | 'offer' {
  if (!same(here, fast) || same(at, fast) || at.from !== 'server') return 'stay';
  const ahead = at.chapter !== fast.chapter ? at.chapter > fast.chapter : at.chunk > fast.chunk;
  const newer = serverMs == null || deviceMs == null || serverMs > deviceMs;
  return ahead || newer ? 'offer' : 'stay';
}

/** Keep a resolved position inside a book that may have been re-chunked. */
export function clampResume(r: Resume, chapters: number, chunksInChapter?: number): Resume {
  const chapter = chapters > 0 ? Math.min(r.chapter, chapters - 1) : 0;
  const chunk = chunksInChapter && chunksInChapter > 0
    ? Math.min(r.chunk, chunksInChapter - 1) : r.chunk;
  return {chapter, chunk, from: r.from};
}
