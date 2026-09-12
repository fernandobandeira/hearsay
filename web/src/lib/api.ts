/**
 * Every conversation with the server.
 *
 * The calls themselves are **generated**: `src/client` is built by
 * `scripts/gen-client.sh` from the server's OpenAPI document, which utoipa
 * generates from the handlers. So the request and response shapes in this file
 * are not a description of the API that has to be kept true — they are the API,
 * and a field that moves in Rust is a type error here in the same change.
 *
 * What stays hand-written is the *policy*, which no generator knows:
 *
 *   networkMode 'offlineFirst' everywhere. The browser saying "offline" does not
 *   mean the request will fail - the service worker answers /api/book.json,
 *   /api/text/* and downloaded chapter audio from Cache Storage. Letting Query
 *   pause those requests would break offline reading, which is the opposite of
 *   what it is for.
 *
 *   The book key rides along as ?book= / "book" on everything. On the cacheable
 *   GETs it is what keeps a downloaded chapter 3 of one book from answering for
 *   chapter 3 of another; on the session endpoints and the chapter verbs it is
 *   what makes the server answer 409 instead of acting on the wrong book. Both
 *   matter, and they are the same parameter.
 *
 *   The URL builders below stay builders. `<audio src>`, an HLS playlist and a
 *   Cache Storage key are strings, not calls - and the download action stores
 *   chapter audio under exactly these URLs, so they are also the reader's cache
 *   keys. The generated client encodes query values with `encodeURIComponent`
 *   too, so a builder URL and an SDK URL for the same resource are byte for byte
 *   the same request.
 */
import {
  QueryClient, onlineManager, useQuery, useMutation, useQueryClient,
} from '@tanstack/react-query';
import * as sdk from '@/client';
import {delayFor, isRetryable, retryQuery, retryWhileOnline} from './backoff';
import type {
  BookIndex, BuildResult, CancelResult, ChaptersResult, ChapterText, LoadResult,
  RenderResult, TextShard,
} from './types';

export class ApiError extends Error {
  constructor(readonly status: number, message: string) { super(message); }
}

export const qs = (key: string | null | undefined) =>
  `?book=${encodeURIComponent(key ?? '')}`;
export const chapterAudioUrl = (key: string | null, ci: number) =>
  `/api/chapters/${ci}.m4a${qs(key)}`;
export const chapterManifestUrl = (key: string | null, ci: number) =>
  `/api/chapters/${ci}.json${qs(key)}`;
export const chapterHlsUrl = (key: string | null, ci: number) =>
  `/api/chapters/${ci}.m3u8${qs(key)}`;
export const chapterTextUrl = (key: string | null, ci: number) =>
  `/api/chapter/${ci}${qs(key)}`;
export const bookIndexUrl = (key: string | null) => `/api/book.json${qs(key)}`;
export const textShardUrl = (key: string | null, s: number) => `/api/text/${s}.json${qs(key)}`;

/** The `{error}` body every failure in this API carries. */
const reason = (e: unknown): string | null => {
  if (typeof e === 'string') return e;
  const o = e as {error?: unknown} | null;
  return typeof o?.error === 'string' ? o.error : null;
};

/**
 * The generated client's `{data, error, response}` envelope, unwrapped into the
 * reader's one failure type.
 *
 * Keeping ApiError is the point: the retry policy reads its status (a 4xx is not
 * worth repeating, a 409 means the server is on another book and asking again
 * will say so again), and the UI shows its message, which is the server's own
 * `{error: "..."}` string rather than "HTTP 409".
 */
export async function call<T>(
  op: Promise<{data?: T; error?: unknown; response?: Response}>,
): Promise<T> {
  const r = await op;
  // No response at all is a transport failure, which the retry policy treats as
  // status 0: worth repeating, unlike a 4xx.
  const status = r.response?.status ?? 0;
  if (r.error !== undefined || !r.response?.ok) {
    throw new ApiError(status, reason(r.error) ?? (status ? `HTTP ${status}` : 'no answer'));
  }
  return r.data as T;
}

/** A typed GET of one of the URL-keyed cacheable resources above. */
export async function get<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    throw new ApiError(res.status, reason(body) ?? `HTTP ${res.status}`);
  }
  return res.json() as Promise<T>;
}

/** Fire and forget: playback bookkeeping the UI must never wait on or flinch at. */
export function tell<T>(op: Promise<{data?: T; error?: unknown; response?: Response}>): void {
  void call(op).catch(() => {});
}

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      networkMode: 'offlineFirst',
      refetchOnReconnect: true,
      refetchOnWindowFocus: true,
      retry: retryQuery,
      retryDelay: (attempt) => delayFor(attempt),
      staleTime: 5_000,
    },
    mutations: {networkMode: 'offlineFirst', retry: 1},
  },
});

/**
 * The retry policy for an **awaited** `fetchQuery` that has a fallback behind it.
 *
 * `offlineFirst` + the default policy means a retry is *paused* rather than run
 * while the browser says it is offline, and a paused query's promise never
 * settles. A hook can afford that - it keeps showing what it has and resumes on
 * reconnect. A `await qc.fetchQuery(...).catch(() => null)` cannot: the catch
 * never runs, so the cached copy sitting one line further down is never reached.
 * See lib/backoff.ts. Hook queries keep the default; this is for the chain in
 * lib/chaptertext.ts and the book index behind it.
 */
export const awaitedRetry = retryWhileOnline(() => onlineManager.isOnline());

export const keys = {
  books: ['books'] as const,
  status: ['status'] as const,
  chapters: ['chapters'] as const,
  bookIndex: (k: string | null) => ['book-index', k] as const,
  shard: (k: string | null, s: number) => ['shard', k, s] as const,
  chapterText: (k: string | null, ci: number) => ['chapter-text', k, ci] as const,
  manifest: (k: string | null, ci: number) => ['manifest', k, ci] as const,
};

export function useBooks() {
  return useQuery({
    queryKey: keys.books,
    queryFn: () => call(sdk.books()),
    staleTime: 60_000,
  });
}

/**
 * The heartbeat.
 *
 * It used to be polled every second, because a poll was the only way to notice
 * anything - including that the server had come back. `/api/events` does that
 * job now, so this runs at a fifth of the rate: it is a fallback for a browser
 * with no live stream and the source of the few numbers no event carries (the
 * packed bitrate, the disk figures).
 */
export function useStatus(enabled = true) {
  return useQuery({
    queryKey: keys.status,
    queryFn: () => call(sdk.status()),
    refetchInterval: 5_000,
    refetchIntervalInBackground: false,
    staleTime: 0,
    retry: 1,
    enabled,
  });
}

/**
 * Per-chapter render/pack state, for one named book.
 *
 * `book` is not decoration: this answer is what a tap on "download the rest"
 * acts on, and the server refuses (409) rather than describing a book it swapped
 * to since the last poll.
 */
export function useChapters(enabled: boolean, book: string | null) {
  return useQuery({
    queryKey: keys.chapters,
    queryFn: () => call(sdk.chaptersList({query: {book: book ?? undefined}})),
    refetchInterval: enabled ? 2_000 : false,
    enabled,
    // One retry, and none at all for a refusal: a 409 says the server is holding
    // another book, and asking again two milliseconds later will say so again.
    retry: (count, e) => count < 1 && isRetryable(e instanceof ApiError ? e.status : 0),
  });
}

/** The book's table of contents, and where each chapter's words live. */
export function useBookIndex(key: string | null) {
  return useQuery({
    queryKey: keys.bookIndex(key),
    queryFn: () => get<BookIndex>(bookIndexUrl(key)),
    enabled: !!key,
    staleTime: Infinity,
    gcTime: Infinity,
  });
}

export function useTextShard(key: string | null, shard: number | null) {
  return useQuery({
    queryKey: keys.shard(key, shard ?? -1),
    queryFn: () => get<TextShard>(textShardUrl(key, shard as number)),
    enabled: !!key && shard != null && shard >= 0,
    staleTime: Infinity,
    gcTime: 10 * 60_000,
  });
}

export const loadBook = (path: string): Promise<LoadResult> =>
  call(sdk.load({body: {path}}));

export function useLoadBook() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: loadBook,
    onSuccess: () => {
      void qc.invalidateQueries({queryKey: keys.chapters});
      void qc.invalidateQueries({queryKey: keys.status});
    },
  });
}

/**
 * The chapter verbs, each aimed at a named book.
 *
 * This is the request where getting the book wrong is expensive rather than
 * merely wrong: one tap can queue 74 chapters, and on the wrong novel that is
 * the render worker's afternoon. The reader has no way to close that race on its
 * own - the server can, and does, with a 409.
 */
export function useChapterActions(book: string | null) {
  const qc = useQueryClient();
  const bump = () => void qc.invalidateQueries({queryKey: keys.chapters});
  const on = book ?? undefined;
  return {
    /**
     * Queue renders, and - with `pack` - leave a standing order to pack each one
     * as it completes.
     *
     * `pack: true` is what makes a download survive the app being closed. Without
     * it the server renders all night and packs nothing, because packing was the
     * client's move and iOS suspends the client seconds after the screen goes
     * off. With it the server chains render -> pack itself and writes the order
     * to disk, so a restart does not cancel it either. See src/intents.rs and
     * lib/reconcile.ts, which is the device half of the same promise.
     */
    render: useMutation<RenderResult, Error, {chapters: number[]; pack?: boolean}>({
      mutationFn: ({chapters, pack}) =>
        call(sdk.chaptersRender({body: {chapters, pack, book: on}})),
      onSuccess: bump,
    }),
    build: useMutation<BuildResult, Error, number[]>({
      mutationFn: (chapters) =>
        call(sdk.chaptersBuild({body: {chapters, book: on}})),
      onSuccess: bump,
    }),
    cancel: useMutation<CancelResult, Error, number[] | undefined>({
      mutationFn: (chapters) =>
        call(sdk.chaptersCancel({body: chapters ? {chapters, book: on} : {book: on}})),
      onSuccess: bump,
    }),
  };
}

/** The chapter list, fetched once rather than polled — what a download run walks. */
export const fetchChapters = (book: string | null): Promise<ChaptersResult> =>
  call(sdk.chaptersList({query: {book: book ?? undefined}}));

/** Chapter text, straight from the endpoint (the cached-shard route is in state). */
export const fetchChapterText = (key: string | null, ci: number): Promise<ChapterText> =>
  get<ChapterText>(chapterTextUrl(key, ci));

// ----------------------------------------------------------- playback reports
// Each of these names its book, so a report meant for one cannot move another
// one's render frontier or file a position under the wrong name.

export const openChapter = (book: string | null, chapter: number, chunk: number) =>
  call(sdk.openChapter({body: {chapter, chunk, book: book ?? undefined}}));

export const reportPlayhead = (book: string | null, chunk: number) =>
  call(sdk.playhead({body: {chunk, book: book ?? undefined}}));

export const tellPause = () => tell(sdk.pause());
export const tellResume = () => tell(sdk.resume());

// `/api/note` and `/api/position` are deliberately *not* here: they are the
// outbox's, and lib/flush.ts calls them with plain fetch because it needs the
// raw status and body to decide whether a recording was filed. "A 2xx carrying
// {ok, file, text, language} and nothing else counts as delivered" is a rule
// about a response, not about a value, and an unwrapped envelope is the wrong
// shape for it - the audio is on a phone and nowhere else until that call
// succeeds.
