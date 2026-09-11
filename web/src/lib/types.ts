/**
 * The server's API, as types.
 *
 * This is the contract the Obsidian plugin already depends on and the one a
 * future Rust rewrite has to re-implement, so it is written down here rather than
 * inferred at each call site.
 */
export interface BookFile { path: string; name: string; mb: number; key?: string }

export interface ChapMeta {
  i: number; title: string; n: number; est_min: number | null; shard?: number;
}

/** What `save_position()` writes into the vault, as `/api/load` hands it back. */
export interface SavedPosition {
  chapter: number; chunk: number;
  chapter_title?: string; chunks_total?: number; chapters_total?: number;
  /** naive local ISO, e.g. `2026-09-11T14:22:07` */
  updated?: string;
}

export interface LoadResult {
  title: string; key: string; total_min: number;
  position?: SavedPosition | null;
  chapters: ChapMeta[];
}

export interface ChapRow extends ChapMeta {
  rendered: number;
  m4a: boolean;
  bytes: number | null;
  duration: number | null;
  queued?: boolean;
  packing?: boolean;
  pack_queued?: boolean;
}

export interface ChaptersResult {
  book: string | null; key: string | null; title: string | null; chapter?: number;
  chapters: ChapRow[]; queue?: number[]; building?: number | null;
  build_error?: string | null; chapters_gb?: number; chapters_cap_gb?: number;
}

export interface Status {
  status: string; error: string | null; chapter: number;
  book: string | null; title: string | null; key: string | null;
  render_idx: number; playhead: number; total: number; chapters: number;
  model_ready: boolean; rtf: number | null; rendered_min: number; voice: string;
  prerender: number | null; prerender_chapters: number;
  prerender_hours: number | null; prerender_span: number;
  book_min: number | null; done_min: number | null;
  disk_gb: number; disk_cap_gb: number;
  queue: number[]; building: number | null; build_error: string | null;
}

export interface BookIndex {
  key: string; name: string; title: string; total_min: number;
  shards: number; text_bytes: number; chapters: ChapMeta[];
}

export interface TextShard {
  shard: number; from: number; to: number;
  chapters: {i: number; paras: number[]; chunks: string[]}[];
}

export interface ChapterText { title: string; chunks: string[]; paras?: number[] }

export interface NoteResult { ok: boolean; file: string; text: string; language: string }
