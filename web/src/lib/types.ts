/**
 * The server's API, as types — **generated, not written down**.
 *
 * This file used to be the reader's own transcription of the contract, kept in
 * step with the server by hand and by a drift check that compared the two. It is
 * now a thin view over `src/client`, which `scripts/gen-client.sh` generates
 * from the server's OpenAPI document (which is itself generated from the
 * handlers). So a field that changes shape in Rust is a TypeScript error here,
 * in the same change, rather than a runtime surprise in three weeks.
 *
 * What survives is the *naming*: the reader's components say `ChapRow` and
 * `SavedPosition`, and those names are good, so they are aliases rather than a
 * rename across forty call sites.
 */
export type {
  BookIndex,
  ChapMeta,
  ChaptersResult,
  ChapterText,
  LoadResult,
  NoteResult,
  Status,
  TextShard,
  BuildResult,
  BuildRefusal,
  RenderResult,
  CancelResult,
} from '@/client';

import type {
  BookFile as ServerBookFile, ChapterIndexRow, ChapterRow, LibraryBook, LibraryResult,
  StampedPosition,
} from '@/client';

/**
 * A book in the library.
 *
 * `key` is the reader's own addition, not the server's: the library falls back
 * to the books this device has opened before (localStorage) when the server
 * cannot be reached, and those entries know their cache key.
 */
export type BookFile = ServerBookFile & {key?: string};

/** A row of the chapter manager. `ChapterRow` server-side. */
export type ChapRow = ChapterRow;

/** What `save_position()` writes into the vault, as `/api/load` hands it back. */
export type SavedPosition = StampedPosition;

/**
 * One book's readiness, as `/api/library` reports it — how much is rendered,
 * how much is packed, and where the reading position is.
 *
 * Re-exported here rather than imported from `@/client` at the use site for the
 * same reason everything else in this file is: this is the one place the reader
 * says what it calls the server's types, so a field that moves in Rust is a
 * TypeScript error in one file rather than in five.
 */
export type LibraryRow = LibraryBook;
export type Library = LibraryResult;
/** A chapter's scanned state. Only present with `?book=&chapters=true`. */
export type ChapterIndex = ChapterIndexRow;
