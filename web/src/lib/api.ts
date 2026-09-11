/**
 * Every conversation with the server.
 *
 * TanStack Query owns retries, backoff, refetch-on-reconnect and the cache; this
 * module only describes *what* the calls are. Two rules run through it:
 *
 *   networkMode 'offlineFirst' everywhere. The browser saying "offline" does not
 *   mean the request will fail - the service worker answers /api/book.json,
 *   /api/text/* and downloaded chapter audio from Cache Storage. Letting Query
 *   pause those requests would break offline reading, which is the opposite of
 *   what it is for.
 *
 *   The book key rides along as ?book= on everything cacheable. The server holds
 *   one book at a time; the cache is keyed by URL and knows nothing about that,
 *   so without the key a downloaded chapter 3 of one book would answer for
 *   chapter 3 of another.
 */
import {QueryClient, useQuery, useMutation, useQueryClient} from '@tanstack/react-query';
import {delayFor, isRetryable} from './backoff';
import type {
  BookFile, BookIndex, ChaptersResult, ChapterText, LoadResult, Status, TextShard,
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

export async function get<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    throw new ApiError(res.status, (body as {error?: string} | null)?.error ?? `HTTP ${res.status}`);
  }
  return res.json() as Promise<T>;
}

export const post = <T,>(url: string, body?: unknown) =>
  get<T>(url, {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify(body ?? {}),
  });

/** Fire and forget: playback bookkeeping the UI must never wait on or flinch at. */
export function tell(url: string, body?: unknown): void {
  void post(url, body).catch(() => {});
}

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      networkMode: 'offlineFirst',
      refetchOnReconnect: true,
      refetchOnWindowFocus: true,
      retry: (count, error) =>
        count < 5 && isRetryable(error instanceof ApiError ? error.status : 0),
      retryDelay: (attempt) => delayFor(attempt),
      staleTime: 5_000,
    },
    mutations: {networkMode: 'offlineFirst', retry: 1},
  },
});

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
    queryFn: () => get<BookFile[]>('/api/books'),
    staleTime: 60_000,
  });
}

/** The heartbeat. It is also how the reader notices the server came back. */
export function useStatus(enabled = true) {
  return useQuery({
    queryKey: keys.status,
    queryFn: () => get<Status>('/api/status'),
    refetchInterval: 1_000,
    refetchIntervalInBackground: false,
    staleTime: 0,
    retry: 1,
    enabled,
  });
}

/** Per-chapter render/pack state. Polled only while someone is looking. */
export function useChapters(enabled: boolean) {
  return useQuery({
    queryKey: keys.chapters,
    queryFn: () => get<ChaptersResult>('/api/chapters'),
    refetchInterval: enabled ? 2_000 : false,
    enabled,
    retry: 1,
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

export function useLoadBook() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (path: string) => post<LoadResult>('/api/load', {path}),
    onSuccess: () => {
      void qc.invalidateQueries({queryKey: keys.chapters});
      void qc.invalidateQueries({queryKey: keys.status});
    },
  });
}

export function useChapterActions() {
  const qc = useQueryClient();
  const bump = () => void qc.invalidateQueries({queryKey: keys.chapters});
  return {
    render: useMutation({
      mutationFn: (chapters: number[]) =>
        post<{ok: boolean; queue: number[]}>('/api/chapters/render', {chapters}),
      onSuccess: bump,
    }),
    build: useMutation({
      mutationFn: (chapters: number[]) =>
        post<{built: number[]; building: number[]; rendering: number[]}>(
          '/api/chapters/build', {chapters}),
      onSuccess: bump,
    }),
    cancel: useMutation({
      mutationFn: (chapters?: number[]) =>
        post<{ok: boolean}>('/api/chapters/cancel', chapters ? {chapters} : {}),
      onSuccess: bump,
    }),
  };
}

/** Chapter text, preferring the cached shard the whole book was taken in. */
export async function fetchChapterText(key: string | null, ci: number): Promise<ChapterText> {
  return get<ChapterText>(chapterTextUrl(key, ci));
}
