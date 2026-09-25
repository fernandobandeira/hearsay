//! The HTTP API — a frozen contract.
//!
//! Two other things depend on this surface: the Obsidian plugin and the web
//! reader, both already shipped against the python server. Every path, method,
//! field name, type and nullability here is that server's, and the OpenAPI
//! document generated from these handlers is what the TypeScript client is
//! generated from, so a drift in either direction shows up as a type error
//! rather than as a reader that silently stops working.

pub mod chapters;
pub mod device;
pub mod health;
pub mod library;
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

// ------------------------------------------------------- pre-gzipped serving
// The text bundle is written with a `.gz` beside every `.json`, so the only
// thing left at request time is content negotiation. Hand-rolled and narrow on
// purpose: a compression *layer* would either re-compress ~17 MB of shards on
// every request or would have to be kept away from the audio endpoints by hand,
// and those serve byte ranges (a range of a compressed body is not a range of
// the file iOS asked for).

/// Tolerant `Accept-Encoding` parse: gzip (or `*`) offered and not refused with
/// `q=0`. A malformed q-value is taken as 1.0 rather than as a refusal — the
/// cost of being wrong is a few hundred kB, not a broken reader.
pub fn accepts_gzip(headers: &HeaderMap) -> bool {
    let raw = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut gzip = None;
    let mut star = None;
    for part in raw.split(',') {
        let (name, params) = match part.split_once(';') {
            Some((n, p)) => (n.trim(), p.trim()),
            None => (part.trim(), ""),
        };
        let q = params
            .strip_prefix("q=")
            .map_or(1.0, |v| v.parse::<f64>().unwrap_or(1.0));
        match name {
            "gzip" => gzip = Some(q),
            "*" => star = Some(q),
            _ => {}
        }
    }
    gzip.or(star).unwrap_or(0.0) > 0.0
}

/// Serve a built text file, pre-gzipped when the client takes it.
///
/// Pure content negotiation: the decoded body is byte-identical either way, so
/// the frozen API shape is untouched. A missing `.gz` (a bundle built by an
/// older server) falls back to the plain file rather than failing.
pub async fn gz_json_file(
    path: &std::path::Path,
    headers: &HeaderMap,
    is_head: bool,
    cache: &str,
    missing: &str,
) -> Response {
    // The plain file is what decides 404: a `.gz` can only ever stand in for one.
    if tokio::fs::metadata(path).await.is_err() {
        return err(StatusCode::NOT_FOUND, missing);
    }
    let gz = crate::text::gz_path(path);
    let gzipped = accepts_gzip(headers) && tokio::fs::metadata(&gz).await.is_ok();
    let Ok(body) = tokio::fs::read(if gzipped { &gz } else { path }).await else {
        return err(StatusCode::NOT_FOUND, missing);
    };
    let mut r = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, cache)
        // Sent either way: a cache that saw one form must not serve it to a
        // client that asked for the other.
        .header(header::VARY, "Accept-Encoding")
        .header(header::CONTENT_LENGTH, body.len().to_string());
    if gzipped {
        r = r.header(header::CONTENT_ENCODING, "gzip");
    }
    // HEAD answers with the same headers and no body, so a client can size a
    // download without taking it.
    r.body(if is_head {
        Body::empty()
    } else {
        Body::from(body)
    })
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

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
        (name = "library", description = "Every book on the box, from a cached filesystem scan"),
        (name = "health", description = "A check that can actually fail"),
    )
)]
pub struct ApiDoc;

/// The largest `/api/note` body taken: a base64 memo, so ~48 MB of audio.
///
/// axum's `Json` extractor refuses anything over 2 MB by default, which is about
/// a minute and a half of opus once base64 has had its third — and the reader
/// reads that 413 as a rejection, not as "ask again", so a long memo was never
/// filed and never retried. The ceiling is deliberately far above any memo a
/// person records, and applies to this one route: nothing else here takes a
/// body worth more than a few kilobytes.
pub const NOTE_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// `/api/note`, with its own body limit layered onto the route and nothing else.
fn note_route() -> utoipa_axum::router::UtoipaMethodRouter<Arc<AppState>> {
    let (schemas, paths, method) = routes!(notes::note);
    (
        schemas,
        paths,
        method.layer(axum::extract::DefaultBodyLimit::max(NOTE_BODY_LIMIT)),
    )
}

pub fn router(state: Arc<AppState>) -> (axum::Router, utoipa::openapi::OpenApi) {
    // A voice memo the last process was transcribing when it was killed — a
    // deploy, the watchdog, a restart nobody meant — is owed to the vault and
    // nobody else can pay it: the phone is not required to come back. So every
    // process that is about to serve this API finishes what the last one
    // started. Here rather than in `boot` because this is the function every
    // server path goes through and no other path does.
    notes::resume_unfiled(state.clone());
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
        .routes(routes!(library::library))
        .routes(routes!(media::hls_segment))
        .routes(note_route())
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

// ------------------------------------------------------- serving the reader
// A deploy has to be visible on the *first* relaunch of the PWA, and without an
// explicit `Cache-Control` it is not. tower-http's ServeDir sets `last-modified`
// and nothing about freshness, which leaves every response heuristically
// cacheable: WebKit then reuses a stored copy for a fraction of its
// last-modified age with no request at all. That bit for real — a reader fix
// shipped, the box laid the new build down correctly, and the iPhone kept
// painting the old layout, because the two files that bootstrap everything
// (`index.html` and `sw.js`) were both being served out of the phone's cache.
// A stale `sw.js` is the worse half: the service worker is what would have
// noticed the new build, so a stale one cannot update itself.
//
// So the split, decided on the request path because that is where the name is
// known before anything has been opened:
//
// - `assets/*` is Vite's hashed build output (`index-uhN6I5NN.js`). The name
//   changes when the bytes do, so a year and `immutable` are free and correct —
//   that is the entire point of content hashing, and it keeps the expensive
//   half of the shell off the wire.
// - Everything else — `index.html` and every SPA fallback path (which is most
//   of the site, the PWA launching at `/` included), `sw.js`, `registerSW.js`,
//   `workbox-*.js`, the manifest, the icons — is `no-cache`: *store it, but ask
//   before using it*. The revalidation is a 304 of a few hundred bytes and it is
//   what makes a deploy land immediately. Not `no-store`, which would forbid
//   keeping the shell at all and take the offline reader with it.

/// The `Cache-Control` a path under the reader should carry.
fn web_cache_control(path: &str) -> header::HeaderValue {
    if is_hashed_asset(path) {
        header::HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        header::HeaderValue::from_static("no-cache")
    }
}

/// Is this one of Vite's content-hashed build assets?
///
/// `assets/<name>-<hash><ext>`, the hash being eight characters of rollup's
/// base64url alphabet. The hash is checked rather than assumed from the
/// directory, and the check is deliberately biased: a hashed file this fails to
/// recognise merely revalidates, while an *un*hashed file pinned for a year is a
/// stale reader nothing on the device can fix.
fn is_hashed_asset(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/assets/") else {
        return false;
    };
    let name = rest.rsplit('/').next().unwrap_or(rest);
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() {
        return false;
    }
    // Counted from the end, so a hash that happens to contain a `-` still lines
    // up against the separator.
    let b = stem.as_bytes();
    b.len() >= 9
        && b[b.len() - 9] == b'-'
        && b[b.len() - 8..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
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
        // Read off the request, before the response exists: the SPA fallback
        // answers an unknown path with index.html, so by the time there is a
        // response there is nothing left to say which file was asked for.
        let cache = web_cache_control(req.uri().path());
        async move {
            if !index.exists() {
                return Ok::<_, std::convert::Infallible>(not_built());
            }
            use tower::ServiceExt;
            match ServiceExt::<Request>::oneshot(&mut serve, req).await {
                Ok(r) => {
                    let mut r = r.into_response();
                    // Insert, never overwrite — if the inner service ever grows
                    // an opinion of its own (this tower-http has none), it knows
                    // more about that file than a path prefix does. Applied to
                    // whatever came back, so the 304s and 206s carry it too: a
                    // header only on the 200 would be a header the client stops
                    // seeing the moment caching starts working.
                    r.headers_mut()
                        .entry(header::CACHE_CONTROL)
                        .or_insert(cache);
                    Ok(r)
                }
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

    #[test]
    fn only_vites_hashed_output_is_pinned_forever() {
        // The three files a real `npm run build` puts in web/dist/assets.
        for p in [
            "/assets/index-uhN6I5NN.js",
            "/assets/index-CO5pRAZL.css",
            "/assets/flush-5A8NmuWf.js",
        ] {
            assert!(is_hashed_asset(p), "{p} should be immutable");
        }
        // A hash carrying base64url's own `-` still lines up, because the
        // separator is found by counting back from the extension.
        assert!(is_hashed_asset("/assets/index-CO5p-AZL.js"));
        // Everything the shell is made of, and anything under assets/ that does
        // not actually carry a hash: a revalidation, not a year.
        for p in [
            "/",
            "/index.html",
            "/sw.js",
            "/registerSW.js",
            "/workbox-c85e56c8.js",
            "/manifest.webmanifest",
            "/icon-512.png",
            "/some/deep/route",
            "/assets/logo.svg",
            "/assets/index.js",
            "/assets/index-short.js",
            "/assets/noextension",
            "/assets/index-uhN6I5NN.js.map",
        ] {
            assert!(!is_hashed_asset(p), "{p} should not be immutable");
        }
    }

    #[test]
    fn the_shell_revalidates_and_the_assets_do_not() {
        assert_eq!(web_cache_control("/"), "no-cache");
        assert_eq!(web_cache_control("/sw.js"), "no-cache");
        assert_eq!(
            web_cache_control("/assets/index-uhN6I5NN.js"),
            "public, max-age=31536000, immutable"
        );
    }
}
