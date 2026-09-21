//! `GET /api/library` — what the whole box has ready, as of the last scan.
//!
//! The one endpoint here that does **not** describe the loaded book.
//! `/api/chapters` cannot: it is the session's view, and the session holds one
//! book. This is the view a phone needs before it has picked anything — which of
//! these novels is rendered, which chapters are packed, how much it would weigh
//! to take them, and where each one was left.
//!
//! # It reports a scan, and says how old it is
//!
//! Every count in this response comes out of `chapter_index`, which is **a cache
//! of a filesystem scan and not truth** — see [`crate::library`] and
//! [`crate::store`]'s module doc. The gc can evict chunks a second after a scan
//! and nothing here will know until [`crate::library::scan_all`] runs again. So
//! the response carries [`LibraryResult::scanned_ms`], the *oldest* stamp in the
//! answer, and the honest reading of every number below it is "this was true
//! then". A reader that acts on a row — taps download — is not harmed by a stale
//! one: the request goes to `/api/chapters/build`, which walks the actual disk
//! and answers `refused: not_rendered` if the row was lying.
//!
//! Nothing in the renderer or the packer reads this table, and nothing should
//! start.
//!
//! # Degrading
//!
//! With no store ([`AppState::store`] is `None` — a work directory gone
//! read-only, a `state.db` that is not a database) this answers `200` with an
//! empty library and a null stamp rather than an error. That is the same policy
//! the rest of the crate follows: the extra answers go, the reader does not.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::cache;
use crate::state::AppState;
use crate::store::{ChapterIndexRow, PositionRow};
use crate::vault;

use super::round1;

/// One book on this box.
///
/// The counts are aggregates of that book's cached scan rows; `chapters` is what
/// the `book` table records, so a book whose plan is on disk but which has never
/// been scanned still says how long it is.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LibraryBook {
    /// The cache directory name — what `?book=` takes everywhere else.
    pub key: String,
    /// The epub file name, which is what positions are keyed by.
    pub name: String,
    /// Absolute path on the server, or empty for a book whose audio is here and
    /// whose source file is not.
    pub path: String,
    pub title: String,
    pub chapters: usize,
    /// When this book was last opened, or null for one that has only ever been
    /// found on disk.
    #[schema(required = true)]
    pub last_open_ms: Option<i64>,
    /// Chapters every chunk of which was on disk at the scan. A chapter with no
    /// chunks at all does not count: "nothing to render" is not "rendered".
    pub rendered_chapters: usize,
    /// Chapters with a *usable* packed m4a. A manifest that disagrees with the
    /// plan is not one; see [`crate::library::scan_book`].
    pub packed_chapters: usize,
    /// What those weigh, measured — not estimated.
    pub packed_bytes: u64,
    pub total_chunks: usize,
    pub rendered_chunks: usize,
    /// The whole book's spoken length, from the chunker's character counts.
    /// Null only when nothing has been scanned yet.
    #[schema(required = true)]
    pub est_min: Option<f64>,
    /// Where this book was left, by **file name** — the store's newest record
    /// across every device, falling back to the vault's own map for a book that
    /// predates the store. Null for one nobody has opened.
    #[schema(required = true)]
    pub position: Option<vault::StampedPosition>,
    /// Is this the book the session is holding? The one field here that is a
    /// live fact rather than a scan result.
    pub loaded: bool,
    /// The per-chapter rows, present only for `?book=<key>&chapters=true`.
    ///
    /// Deliberately absent from the all-books answer: *Lord of Mysteries* alone
    /// is 1433 of these, and a library response that carried them for every book
    /// would be megabytes over a tunnel to answer a question about shelves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter_index: Option<Vec<ChapterIndexRow>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LibraryResult {
    pub books: Vec<LibraryBook>,
    /// The **oldest** scan in this answer, so the reader can say how fresh it
    /// is — and so one book that has not been walked since yesterday cannot hide
    /// behind eleven that were walked a minute ago. Null when nothing in the
    /// answer has ever been scanned.
    #[schema(required = true)]
    pub scanned_ms: Option<i64>,
}

/// `?book=&chapters=`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct LibraryQuery {
    /// One book's cache key. Unknown keys answer with an empty list rather than
    /// a 404: this endpoint describes what is here, and "not here" is an answer.
    pub book: Option<String>,
    /// Include the per-chapter rows. Only meaningful alongside `book`; it is
    /// ignored for the all-books answer, which would otherwise be enormous.
    pub chapters: Option<bool>,
}

/// A store position row as the API hands positions back.
///
/// The store keeps epoch milliseconds because ordering two devices' writes is
/// its whole job; the API's shape is the vault's record plus that instant. So
/// the naive local string is regenerated here from the millisecond, in this
/// server's own zone — the same direction [`vault::Position::stamped`] resolves
/// it in, and the only one available from a row that never held the string.
fn from_row(row: PositionRow) -> vault::StampedPosition {
    let updated = chrono::DateTime::from_timestamp_millis(row.updated_ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default();
    vault::StampedPosition {
        chapter: row.chapter,
        chunk: row.chunk,
        chapter_title: row.chapter_title,
        chunks_total: row.chunks_total,
        chapters_total: row.chapters_total,
        updated,
        updated_ms: Some(row.updated_ms),
    }
}

/// This book's position: the store's newest record, or the vault map behind it.
///
/// The fallback is not belt and braces — it is the adoption path. A work
/// directory carried over from the python server has a full
/// `.narrator-positions.json` and an empty `position` table, and a library view
/// that opened every book at chapter one would be worse than no library view.
fn position_for(st: &Arc<AppState>, name: &str) -> Option<vault::StampedPosition> {
    if let Some(db) = st.store() {
        match db.newest_position(name) {
            Ok(Some((_device, row))) => return Some(from_row(row)),
            Ok(None) => {}
            Err(e) => tracing::warn!("library: could not read the position for {name}: {e}"),
        }
    }
    st.positions()
        .get(name)
        .cloned()
        .and_then(|v| serde_json::from_value::<vault::Position>(v).ok())
        .map(|p| p.stamped())
}

/// Read the table and shape the answer. Blocking: it is a handful of SQLite
/// reads, and on a box whose other core is inside Kokoro they belong off the
/// runtime that is serving audio ranges.
fn collect(st: &Arc<AppState>, want: Option<String>, with_rows: bool) -> LibraryResult {
    let mut out = LibraryResult {
        books: Vec::new(),
        scanned_ms: None,
    };
    let Some(db) = st.store() else {
        return out;
    };
    let books = match &want {
        Some(k) => match db.book(k) {
            Ok(b) => b.into_iter().collect(),
            Err(e) => {
                tracing::warn!("library: could not read {k}: {e}");
                Vec::new()
            }
        },
        None => db.books().unwrap_or_else(|e| {
            tracing::warn!("library: could not list the books: {e}");
            Vec::new()
        }),
    };
    let loaded = st.session().key();
    for b in books {
        let rows = db.chapter_index(&b.key).unwrap_or_else(|e| {
            tracing::warn!("library: could not read the scan of {}: {e}", b.key);
            Vec::new()
        });
        let mut agg = LibraryBook {
            key: b.key.clone(),
            name: b.name.clone(),
            path: b.path,
            title: b.title,
            chapters: b.chapters,
            last_open_ms: b.last_open_ms,
            rendered_chapters: 0,
            packed_chapters: 0,
            packed_bytes: 0,
            total_chunks: 0,
            rendered_chunks: 0,
            est_min: None,
            position: position_for(st, &b.name),
            loaded: loaded.as_deref() == Some(b.key.as_str()),
            chapter_index: None,
        };
        let mut est_s = 0.0;
        let mut any_est = false;
        for r in &rows {
            agg.total_chunks += r.n;
            // Clamped: a row from a scan that raced a render can report more
            // wavs than the plan has chunks, and a book that is 101 % rendered
            // is a number nobody can act on.
            agg.rendered_chunks += r.rendered.min(r.n);
            if r.n > 0 && r.rendered >= r.n {
                agg.rendered_chapters += 1;
            }
            if r.m4a {
                agg.packed_chapters += 1;
                agg.packed_bytes = agg.packed_bytes.saturating_add(r.bytes.unwrap_or(0));
            }
            if let Some(e) = r.est_s {
                est_s += e;
                any_est = true;
            }
            out.scanned_ms = Some(match out.scanned_ms {
                Some(o) => o.min(r.scanned_ms),
                None => r.scanned_ms,
            });
        }
        if any_est {
            agg.est_min = Some(round1(est_s / 60.0));
        }
        if with_rows {
            agg.chapter_index = Some(rows);
        }
        out.books.push(agg);
    }
    out
}

/// Every book on this box, with what is rendered and packed for each.
#[utoipa::path(
    get, path = "/api/library", tag = "library",
    params(LibraryQuery),
    responses((status = 200, body = LibraryResult))
)]
pub async fn library(State(st): State<Arc<AppState>>, Query(q): Query<LibraryQuery>) -> Response {
    // Through the same sanitiser every other `?book=` goes through, so a key
    // that arrived with a slash in it cannot become a lookup for something else.
    let want = q
        .book
        .as_deref()
        .map(cache::safe_key)
        .filter(|k| !k.is_empty());
    let with_rows = q.chapters.unwrap_or(false) && want.is_some();
    let st2 = st.clone();
    let res = tokio::task::spawn_blocking(move || collect(&st2, want, with_rows))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("library: the read panicked: {e}");
            LibraryResult {
                books: Vec::new(),
                scanned_ms: None,
            }
        });
    Json(res).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_row_becomes_a_position_with_both_stamps() {
        let ms = 1_700_000_000_000i64;
        let p = from_row(PositionRow {
            chapter: 3,
            chunk: 17,
            chapter_title: "Three".into(),
            chunks_total: 40,
            chapters_total: 12,
            updated_ms: ms,
            seq: 9,
        });
        assert_eq!((p.chapter, p.chunk), (3, 17));
        assert_eq!(p.updated_ms, Some(ms));
        // The naive string is this machine's zone, and it round-trips through
        // the same parser the vault records go through.
        assert_eq!(vault::epoch_ms(&p.updated), Some(ms - ms % 1000));
    }
}
