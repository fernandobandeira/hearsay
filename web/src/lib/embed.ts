/**
 * The reader inside Obsidian.
 *
 * The vault's narrator plugin no longer has a reader of its own: it frames this
 * one, so every reader change reaches Obsidian with no plugin release. What it
 * still needs from the page is the two things only the page knows how to do -
 * open a book at a position (an `.epub` clicked in the vault, an
 * `obsidian://narrator-open` link out of a voice note) and say where it is (the
 * "insert reading position link" command). This is that conversation, kept to
 * parsing and validation so it can be tested without a frame around it.
 *
 * Two doors, one request:
 *
 *   * `?open=<epub name>&chapter=N&chunk=M` on the page URL, for the first open -
 *     the frame's own src, so nothing waits for a handshake;
 *   * `postMessage` after that, from the parent window only and only from an
 *     origin that is this page or Obsidian. A page that frames the reader from
 *     anywhere else gets silence: "where is he reading" is not its business.
 */

/** Open a book, by its epub file name - the same name in the vault and in `/api/books`. */
export interface OpenRequest {book: string; chapter: number | null; chunk: number}

export type EmbedMessage =
  | ({type: 'hearsay:open'} & OpenRequest)
  | {type: 'hearsay:where'; id: string};

export interface Here {
  type: 'hearsay:here';
  id: string;
  book: string | null;
  title: string | null;
  chapter: number;
  chapterTitle: string;
  chunk: number;
}

/** Obsidian's renderer origins: desktop, iOS, Android. */
const OBSIDIAN = ['app://obsidian.md', 'capacitor://localhost', 'http://localhost'];

export const trustedOrigin = (origin: string, self: string): boolean =>
  origin === self || OBSIDIAN.includes(origin);

const index = (v: unknown): number | null => {
  const n = typeof v === 'string' && v.trim() !== '' ? Number(v) : v;
  return typeof n === 'number' && Number.isInteger(n) && n >= 0 ? n : null;
};

/** `?open=` on a page URL, or null when there is none worth acting on. */
export function parseOpen(search: string): OpenRequest | null {
  const q = new URLSearchParams(search);
  const book = q.get('open');
  if (!book) return null;
  return {book, chapter: index(q.get('chapter')), chunk: index(q.get('chunk')) ?? 0};
}

/** The page URL with the open request taken back off it, so a reload does not repeat it. */
export function withoutOpen(href: string): string {
  const u = new URL(href);
  for (const k of ['open', 'chapter', 'chunk']) u.searchParams.delete(k);
  return u.pathname + u.search + u.hash;
}

export function parseMessage(data: unknown): EmbedMessage | null {
  if (!data || typeof data !== 'object') return null;
  const m = data as Record<string, unknown>;
  if (m.type === 'hearsay:where')
    return typeof m.id === 'string' ? {type: 'hearsay:where', id: m.id} : null;
  if (m.type === 'hearsay:open') {
    if (typeof m.book !== 'string' || !m.book) return null;
    return {type: 'hearsay:open', book: m.book,
            chapter: index(m.chapter), chunk: index(m.chunk) ?? 0};
  }
  return null;
}
