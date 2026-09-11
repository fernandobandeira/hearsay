//! The HTTP API — a frozen contract.
//!
//! Two other things depend on this surface: the Obsidian plugin and the web
//! reader, both already shipped against the python server. Every path, method,
//! field name, type and nullability here is that server's, and the OpenAPI
//! document generated from these handlers is what the TypeScript client is
//! generated from, so a drift in either direction shows up as a type error
//! rather than as a reader that silently stops working.

pub mod chapters;
pub mod health;
pub mod media;
pub mod notes;
pub mod session;
pub mod stream;
pub mod text;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::state::AppState;

/// Every error body in this API: `{"error": "..."}`, and nothing else. The
/// reader's `get()` reads exactly this field.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ApiError {
    pub error: String,
}

pub fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ApiError { error: msg.into() })).into_response()
}

/// `{"ok": true}` plus whatever else a handler wants to say.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Ok2 {
    pub ok: bool,
}

pub fn ok() -> Json<Ok2> {
    Json(Ok2 { ok: true })
}

pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

// ------------------------------------------------------------ ranged serving
// iOS will not play an <audio> source that cannot answer a Range request: it
// probes with one before it plays anything, and a 200 with the whole body makes
// it give up on seeking.

fn parse_range(raw: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let rest = raw.trim().strip_prefix("bytes=")?;
    let (lo, hi) = rest.split_once('-')?;
    let (lo, hi) = (lo.trim(), hi.trim());
    if !lo.chars().all(|c| c.is_ascii_digit()) || !hi.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if lo.is_empty() {
        // Suffix form: the last N bytes.
        let n: u64 = hi.parse().unwrap_or(0);
        return Some(Ok((size.saturating_sub(n), size.saturating_sub(1))));
    }
    let start: u64 = lo.parse().unwrap_or(0);
    let end: u64 = if hi.is_empty() {
        size.saturating_sub(1)
    } else {
        hi.parse().unwrap_or(0)
    };
    if start >= size || start > end {
        return Some(Err(()));
    }
    Some(Ok((start, end.min(size.saturating_sub(1)))))
}

/// Serve a file with byte ranges, honouring HEAD.
pub async fn ranged(
    req_headers: &HeaderMap,
    is_head: bool,
    path: &std::path::Path,
    media_type: &str,
) -> Response {
    let Ok(md) = tokio::fs::metadata(path).await else {
        return err(StatusCode::NOT_FOUND, "not built");
    };
    let size = md.len();
    let base = [
        (header::ACCEPT_RANGES, "bytes".to_string()),
        (header::CACHE_CONTROL, "public, max-age=31536000".into()),
        (header::CONTENT_TYPE, media_type.to_string()),
    ];
    if is_head {
        let mut r = Response::builder().status(StatusCode::OK);
        for (k, v) in base {
            r = r.header(k, v);
        }
        return r
            .header(header::CONTENT_LENGTH, size.to_string())
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    let raw = req_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();

    let window = if raw.is_empty() {
        None
    } else {
        // An unrecognised range *unit* must be ignored, not rejected
        // (RFC 9110 14.2); only a syntactically valid but unsatisfiable byte
        // range is a 416.
        match parse_range(&raw, size) {
            None => None,
            Some(Err(())) => {
                let mut r = Response::builder().status(StatusCode::RANGE_NOT_SATISFIABLE);
                for (k, v) in base {
                    r = r.header(k, v);
                }
                return r
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                    .body(Body::empty())
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
            Some(Ok(w)) => Some(w),
        }
    };

    let (start, end) = window.unwrap_or((0, size.saturating_sub(1)));
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return err(StatusCode::NOT_FOUND, "not built");
    };
    use tokio::io::AsyncSeekExt;
    if start > 0 && f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "seek failed");
    }
    let len = end.saturating_sub(start).saturating_add(1);
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(f.take(len)));
    let mut r = Response::builder().status(if window.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    });
    for (k, v) in base {
        r = r.header(k, v);
    }
    if window.is_some() {
        r = r.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    r.header(header::CONTENT_LENGTH, len.to_string())
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

use tokio::io::AsyncReadExt;

/// Serve a small file whole, with a cache header.
pub async fn json_file(path: &std::path::Path, cache: &str, missing: &str) -> Response {
    match tokio::fs::read(path).await {
        Ok(b) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, cache),
            ],
            b,
        )
            .into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, missing),
    }
}

// --------------------------------------------------------------- the document

#[derive(OpenApi)]
#[openapi(
    info(
        title = "narrator",
        description = "Self-hosted TTS book reader. One global session: the server holds one \
                       book, one chapter and one playhead across every client, which is why \
                       anything cacheable is scoped by ?book= and positions can be written by \
                       name. No auth, no TLS, wildcard CORS - it assumes a private machine \
                       reached over Tailscale.",
        version = env!("CARGO_PKG_VERSION"),
    ),
    paths(
        // Routed by hand below: matchit allows only a whole-segment parameter,
        // and these paths carry a file extension after theirs. The contract is
        // the URL the reader and the plugin already call, so the *document*
        // keeps it and the router matches `{file}` and splits the suffix off.
        media::chunk_wav,
        media::chapter_audio,
        media::chapter_manifest,
        media::chapter_hls,
        text::book_text,
    ),
    tags(
        (name = "session", description = "The loaded book, the playhead and the renderer"),
        (name = "chapters", description = "Chapter audio: render, pack, stream, download"),
        (name = "text", description = "The book as text, for offline reading"),
        (name = "vault", description = "Writes into the Obsidian vault"),
        (name = "health", description = "A check that can actually fail"),
    )
)]
pub struct ApiDoc;

pub fn router(state: Arc<AppState>) -> (axum::Router, utoipa::openapi::OpenApi) {
    let (r, mut api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(session::books))
        .routes(routes!(session::load))
        .routes(routes!(session::chapter))
        .routes(routes!(session::open_chapter))
        .routes(routes!(session::playhead))
        .routes(routes!(session::position))
        .routes(routes!(session::pause))
        .routes(routes!(session::resume))
        .routes(routes!(session::renderer))
        .routes(routes!(session::prerender))
        .routes(routes!(session::status))
        .routes(routes!(text::book_index))
        .routes(routes!(chapters::chapters_list))
        .routes(routes!(chapters::chapters_render))
        .routes(routes!(chapters::chapters_build))
        .routes(routes!(chapters::chapters_cancel))
        .routes(routes!(media::hls_segment))
        .routes(routes!(notes::note))
        .routes(routes!(stream::events))
        .routes(routes!(health::healthz))
        .with_state(state.clone())
        .split_for_parts();

    // The generated document is served as the contract; the TS client is built
    // straight from it (scripts/gen-client.sh).
    api.servers = Some(vec![utoipa::openapi::ServerBuilder::new().url("/").build()]);
    let spec = api.clone();
    let spec_json = serde_json::to_string_pretty(&api).unwrap_or_else(|_| "{}".into());

    let web = state.cfg.web.clone();
    let app = r
        .route(
            "/api/chunk/{ci}/{file}",
            axum::routing::get(media::chunk_wav).head(media::chunk_wav),
        )
        .route(
            "/api/chapters/{file}",
            axum::routing::get(media::chapter_file).head(media::chapter_file),
        )
        .route(
            "/api/text/{file}",
            axum::routing::get(text::book_text).head(text::book_text),
        )
        .with_state(state.clone())
        .route(
            "/openapi.json",
            axum::routing::get(move || {
                let s = spec_json.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], s) }
            }),
        )
        // The Obsidian plugin is a second client of this API, calling from the
        // app:// origin; without CORS the browser reader works and the plugin
        // silently cannot. The server binds a private machine - a wildcard is
        // fine here.
        .layer(CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // Last on purpose: this is a catch-all and every /api route above has to
        // match first.
        .fallback_service(web_service(&web));
    (app, spec)
}

/// `GET /` and everything else falls through to the built reader.
fn web_service(dir: &std::path::Path) -> axum::routing::MethodRouter {
    let dir = dir.to_path_buf();
    let index = dir.join("index.html");
    let serve = tower_http::services::ServeDir::new(&dir)
        .append_index_html_on_directories(true)
        .fallback(tower_http::services::ServeFile::new(index.clone()));
    axum::routing::any_service(tower::service_fn(move |req: Request| {
        let mut serve = serve.clone();
        let index = index.clone();
        async move {
            if !index.exists() {
                return Ok::<_, std::convert::Infallible>(not_built());
            }
            use tower::ServiceExt;
            match ServiceExt::<Request>::oneshot(&mut serve, req).await {
                Ok(r) => Ok(r.into_response()),
                Err(_) => Ok(not_built()),
            }
        }
    }))
}

fn not_built() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><meta charset=utf-8><title>narrator</title>\
         <body style='font:16px system-ui;background:#0a0a0a;color:#ccc;padding:3rem'>\
         <h1>narrator</h1><p>The web app is not built.</p>\
         <pre>cd web &amp;&amp; npm install &amp;&amp; npm run build</pre>\
         <p>or point <code>NARRATOR_WEB</code> at a built copy.</p>",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_the_forms_ios_actually_sends() {
        assert_eq!(parse_range("bytes=0-", 100), Some(Ok((0, 99))));
        assert_eq!(parse_range("bytes=0-0", 100), Some(Ok((0, 0))));
        assert_eq!(parse_range("bytes=10-19", 100), Some(Ok((10, 19))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=90-1000", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=50-20", 100), Some(Err(())));
        // An unknown unit is ignored, not rejected.
        assert_eq!(parse_range("items=0-5", 100), None);
        assert_eq!(parse_range("nonsense", 100), None);
    }
}
