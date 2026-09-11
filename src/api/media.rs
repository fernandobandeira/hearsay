//! Audio off disk: one chunk, one packed chapter, its manifest, its HLS.
//!
//! `?book=` is always the cache key, sanitised to one path segment. It is what
//! lets a phone hold chapter 3 of one book while the server has another open,
//! and it is why a cached URL can never answer for the wrong book. Omitted, it
//! means the loaded book.

use std::sync::Arc;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{err, json_file, ranged, ApiError};
use crate::cache;
use crate::chapters as pack;
use crate::state::AppState;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct BookQuery {
    /// The cache key. Omitted, it means the loaded book.
    pub book: Option<String>,
}

fn key_for(st: &AppState, q: &BookQuery) -> String {
    let given = q.book.as_deref().map(cache::safe_key).unwrap_or_default();
    if given.is_empty() {
        st.session().key_or_x()
    } else {
        given
    }
}

/// One rendered chunk. GET or HEAD; **404 means "not rendered yet", not an
/// error** — the reader polls this while the renderer catches up.
#[utoipa::path(
    method(get, head), path = "/api/chunk/{ci}/{i}.wav", tag = "chapters",
    params(("ci" = usize, Path), ("i" = usize, Path)),
    responses(
        (status = 200, content_type = "audio/wav", body = Vec<u8>),
        (status = 404, body = ApiError, description = "not rendered yet"),
    )
)]
pub async fn chunk_wav(
    State(st): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    AxPath((ci, name)): AxPath<(usize, String)>,
) -> Response {
    let Some(i) = name
        .strip_suffix(".wav")
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return err(StatusCode::NOT_FOUND, "not ready");
    };
    let key = st.session().key_or_x();
    let p = cache::chunk_path(&st.cfg.work, &key, ci, i);
    if !p.exists() {
        return err(StatusCode::NOT_FOUND, "not ready");
    }
    ranged(&headers, method == Method::HEAD, &p, "audio/wav").await
}

/// The packed chapter: AAC-LC mono 64 kbit/s, 24 kHz, `+faststart`. **Serves
/// byte ranges** — iOS probes with a Range before it will play anything. This is
/// the *download* format.
#[utoipa::path(
    method(get, head), path = "/api/chapters/{ci}.m4a", tag = "chapters",
    params(("ci" = usize, Path), BookQuery),
    responses(
        (status = 200, content_type = "audio/mp4", body = Vec<u8>),
        (status = 206, description = "byte range"),
        (status = 404, body = ApiError),
        (status = 416, description = "unsatisfiable range"),
    )
)]
pub async fn chapter_audio(
    State(st): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    AxPath(name): AxPath<String>,
    Query(q): Query<BookQuery>,
) -> Response {
    let Some(ci) = name
        .strip_suffix(".m4a")
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return err(StatusCode::NOT_FOUND, "not built");
    };
    let (p, _) = pack::chapter_files(&st.cfg.work, &key_for(&st, &q), ci);
    if !p.exists() {
        return err(StatusCode::NOT_FOUND, "not built");
    }
    ranged(&headers, method == Method::HEAD, &p, "audio/mp4").await
}

/// chunk index -> start second for the packed chapter. Without this the m4a is
/// one opaque blob and every reading position in the book is lost.
#[utoipa::path(
    method(get, head), path = "/api/chapters/{ci}.json", tag = "chapters",
    params(("ci" = usize, Path), BookQuery),
    responses((status = 200, body = crate::chapters::Manifest), (status = 404, body = ApiError))
)]
pub async fn chapter_manifest(
    State(st): State<Arc<AppState>>,
    AxPath(name): AxPath<String>,
    Query(q): Query<BookQuery>,
) -> Response {
    let Some(ci) = name
        .strip_suffix(".json")
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return err(StatusCode::NOT_FOUND, "not built");
    };
    let (_, p) = pack::chapter_files(&st.cfg.work, &key_for(&st, &q), ci);
    json_file(&p, "public, max-age=31536000", "not built").await
}

/// An HLS playlist for the packed chapter, built on first request.
///
/// This is the streaming path: iOS plays it natively in a plain `<audio>`, so the
/// OS owns buffering and segment retry — a blip re-fetches one six-second segment
/// instead of killing the whole load. The m4a stays the download format, because
/// native HLS bypasses the service worker and can never be the offline one.
#[utoipa::path(
    method(get, head), path = "/api/chapters/{ci}.m3u8", tag = "chapters",
    params(("ci" = usize, Path), BookQuery),
    responses(
        (status = 200, content_type = "application/vnd.apple.mpegurl", body = String),
        (status = 404, body = ApiError), (status = 500, body = ApiError),
    )
)]
pub async fn chapter_hls(
    State(st): State<Arc<AppState>>,
    AxPath(name): AxPath<String>,
    Query(q): Query<BookQuery>,
) -> Response {
    let Some(ci) = name
        .strip_suffix(".m3u8")
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return err(StatusCode::NOT_FOUND, "not built");
    };
    let key = key_for(&st, &q);
    let (m4a, _) = pack::chapter_files(&st.cfg.work, &key, ci);
    if !m4a.exists() {
        return err(StatusCode::NOT_FOUND, "not built");
    }
    let base = format!(
        "/api/hls/{}/{ci}/",
        percent_encoding::utf8_percent_encode(&key, percent_encoding::NON_ALPHANUMERIC)
    );
    let st2 = st.clone();
    let k2 = key.clone();
    let b2 = base.clone();
    let built = tokio::task::spawn_blocking(move || pack::build_hls(&st2.cfg, &k2, ci, &b2)).await;
    match built {
        Ok(Ok(p)) => match tokio::fs::read(&p).await {
            Ok(b) => (
                StatusCode::OK,
                [
                    (
                        axum::http::header::CONTENT_TYPE,
                        "application/vnd.apple.mpegurl",
                    ),
                    (axum::http::header::CACHE_CONTROL, "public, max-age=3600"),
                ],
                b,
            )
                .into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        },
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("hls: {e}")),
    }
}

/// One fMP4 segment (or the init segment) of a chapter's HLS rendition. `name`
/// is validated against `init.mp4|segNNNNN.m4s`; anything else is a 404.
#[utoipa::path(
    method(get, head), path = "/api/hls/{book}/{ci}/{name}", tag = "chapters",
    params(("book" = String, Path), ("ci" = usize, Path), ("name" = String, Path)),
    responses((status = 200, content_type = "video/mp4", body = Vec<u8>), (status = 404, body = ApiError))
)]
pub async fn hls_segment(
    State(st): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    AxPath((book, ci, name)): AxPath<(String, usize, String)>,
) -> Response {
    if !valid_segment(&name) {
        return err(StatusCode::NOT_FOUND, "no such segment");
    }
    let p = pack::hls_dir(&st.cfg.work, &cache::safe_key(&book), ci).join(&name);
    if !p.exists() {
        return err(StatusCode::NOT_FOUND, "no such segment");
    }
    ranged(&headers, method == Method::HEAD, &p, "video/mp4").await
}

fn valid_segment(name: &str) -> bool {
    name == "init.mp4"
        || (name.len() == 12
            && name.starts_with("seg")
            && name.ends_with(".m4s")
            && name[3..8].chars().all(|c| c.is_ascii_digit()))
}

/// The one axum route behind `/api/chapters/{ci}.m4a`, `.json` and `.m3u8`.
///
/// matchit matches a whole path segment or nothing, so three routes that differ
/// only by extension cannot be registered separately; the dispatch happens here
/// instead. `/api/chapters/render`, `/build` and `/cancel` are static segments
/// and still match before this one.
pub async fn chapter_file(
    st: State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    name: AxPath<String>,
    q: Query<BookQuery>,
) -> Response {
    match name.0.rsplit_once('.') {
        Some((_, "m4a")) => chapter_audio(st, method, headers, name, q).await,
        Some((_, "json")) => chapter_manifest(st, name, q).await,
        Some((_, "m3u8")) => chapter_hls(st, name, q).await,
        _ => err(StatusCode::NOT_FOUND, "not built"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_segment_shapes_are_served() {
        assert!(valid_segment("init.mp4"));
        assert!(valid_segment("seg00000.m4s"));
        assert!(valid_segment("seg12345.m4s"));
        assert!(!valid_segment("seg1.m4s"));
        assert!(!valid_segment("seg00000.m4a"));
        assert!(!valid_segment("../init.mp4"));
        assert!(!valid_segment("segabcde.m4s"));
        assert!(!valid_segment(""));
    }
}
