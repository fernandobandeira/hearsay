//! The book as text: two-tier offline, tier one.
//!
//! The words are cheap and the audio is not, so the text is taken whole on first
//! open and the audio is downloaded per chapter on purpose.

use std::sync::Arc;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;

use super::{err, gz_json_file, ApiError};
use crate::cache;
use crate::state::AppState;
use crate::text::text_dir;

use super::media::BookQuery;

fn key_for(st: &AppState, q: &BookQuery) -> String {
    let given = q.book.as_deref().map(cache::safe_key).unwrap_or_default();
    if given.is_empty() {
        st.session().key_or_x()
    } else {
        given
    }
}

/// The index: small (123 kB even for 1433 chapters) and always cached. It is
/// what lets the reader show a book it cannot reach.
#[utoipa::path(
    method(get, head), path = "/api/book.json", tag = "text",
    params(BookQuery),
    responses((status = 200, body = crate::text::BookIndex), (status = 404, body = ApiError))
)]
pub async fn book_index(
    State(st): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    Query(q): Query<BookQuery>,
) -> Response {
    let p = text_dir(&st.cfg.work, &key_for(&st, &q)).join("index.json");
    gz_json_file(
        &p,
        &headers,
        method == Method::HEAD,
        "public, max-age=3600",
        "not loaded yet",
    )
    .await
}

/// One shard: a run of whole chapters, ~1.5 MB at most. Cache every shard and
/// the whole book is readable with no network and no rendered audio; cache none
/// and the reader still lists it.
#[utoipa::path(
    method(get, head), path = "/api/text/{s}.json", tag = "text",
    params(("s" = usize, Path, description = "shard index"), BookQuery),
    responses((status = 200, body = crate::text::TextShard), (status = 404, body = ApiError))
)]
pub async fn book_text(
    State(st): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    AxPath(name): AxPath<String>,
    Query(q): Query<BookQuery>,
) -> Response {
    let Some(s) = name
        .strip_suffix(".json")
        .and_then(|x| x.parse::<usize>().ok())
    else {
        return err(StatusCode::NOT_FOUND, "no such shard");
    };
    let p = text_dir(&st.cfg.work, &key_for(&st, &q)).join(format!("{s:03}.json"));
    gz_json_file(
        &p,
        &headers,
        method == Method::HEAD,
        "public, max-age=3600",
        "no such shard",
    )
    .await
}
