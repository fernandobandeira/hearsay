//! How the built reader is handed out, and why the headers matter.
//!
//! The reader is a PWA: the phone keeps it. So a deploy is only real when the
//! device goes and looks again, and what decides whether it does is
//! `Cache-Control`. With none at all — which is what tower-http's ServeDir
//! leaves — `index.html` and `sw.js` are heuristically cacheable off their
//! `last-modified` age, and a shipped fix can sit unseen behind a phone's own
//! cache for an unbounded time. That happened. These tests are the fence.

mod harness;

use axum::http::StatusCode;
use harness::Harness;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// A reader on disk, shaped like a real `npm run build`.
async fn reader() -> Harness {
    let h = Harness::new().await;
    let web = h.state.cfg.web.clone();
    let assets = web.join("assets");
    std::fs::create_dir_all(&assets).expect("web/assets");
    std::fs::write(
        web.join("index.html"),
        "<!doctype html><title>HearSay</title>",
    )
    .expect("index.html");
    std::fs::write(web.join("sw.js"), "// service worker").expect("sw.js");
    std::fs::write(web.join("registerSW.js"), "// register").expect("registerSW.js");
    std::fs::write(web.join("manifest.webmanifest"), "{}").expect("manifest");
    std::fs::write(assets.join("index-uhN6I5NN.js"), "console.log(1)").expect("js");
    std::fs::write(assets.join("index-CO5pRAZL.css"), "body{}").expect("css");
    h
}

async fn cache_control(h: &Harness, path: &str) -> (StatusCode, String) {
    let res = h.raw("GET", path, &[]).await;
    let cc = res
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    (res.status(), cc)
}

/// Content-hashed, therefore safe to pin: a new build is a new name, so nothing
/// a device holds can ever be the wrong answer.
#[tokio::test]
async fn a_hashed_asset_is_immutable() {
    let h = reader().await;
    for p in ["/assets/index-uhN6I5NN.js", "/assets/index-CO5pRAZL.css"] {
        let (code, cc) = cache_control(&h, p).await;
        assert_eq!(code, StatusCode::OK, "{p}");
        assert_eq!(cc, IMMUTABLE, "{p}");
    }
}

/// The app shell. This is the one the iPhone got wrong: `/` is where the PWA
/// launches, and a stale answer there is a stale reader.
#[tokio::test]
async fn the_app_shell_revalidates() {
    let h = reader().await;
    for p in [
        "/",
        "/index.html",
        "/sw.js",
        "/registerSW.js",
        "/manifest.webmanifest",
    ] {
        let (code, cc) = cache_control(&h, p).await;
        assert_eq!(code, StatusCode::OK, "{p}");
        assert_eq!(cc, "no-cache", "{p}");
    }
}

/// Every route the reader has is a fallback to `index.html` — which is most of
/// the site, and all of it after the first navigation. The header is decided on
/// the request path precisely so these land on the shell's side of the split.
#[tokio::test]
async fn an_unknown_route_falls_back_to_the_shell_and_revalidates() {
    let h = reader().await;
    let res = h.raw("GET", "/some/deep/route", &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    assert!(
        String::from_utf8_lossy(&body).contains("<title>HearSay</title>"),
        "the SPA fallback must serve index.html"
    );
}

/// A header that only rides on the 200 is a header the client stops seeing the
/// moment caching starts working — the 304 is what re-arms the freshness rule
/// for the next launch.
#[tokio::test]
async fn a_revalidation_carries_the_header_too() {
    let h = reader().await;
    let first = h.raw("GET", "/index.html", &[]).await;
    let lm = first
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(!lm.is_empty(), "ServeDir always dates the shell");

    let res = h
        .raw("GET", "/index.html", &[("if-modified-since", &lm)])
        .await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        res.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
}

/// And on a partial response, which is how a big asset arrives at all.
#[tokio::test]
async fn a_range_response_carries_the_header_too() {
    let h = reader().await;
    let res = h
        .raw(
            "GET",
            "/assets/index-uhN6I5NN.js",
            &[("range", "bytes=0-3")],
        )
        .await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        res.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some(IMMUTABLE)
    );
}

/// The API is not the reader: nothing above may reach an endpoint, least of all
/// one whose answer changes every second.
#[tokio::test]
async fn the_api_is_untouched_by_the_reader_headers() {
    let h = reader().await;
    let (code, cc) = cache_control(&h, "/api/status").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(cc, "", "the fallback service must never see /api");
}
