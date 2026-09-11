//! The live stream and the position-writing rules.

mod harness;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use harness::Harness;
use narrator::vault;
use serde_json::json;
use tower::ServiceExt;

/// Open `/api/events` and read whatever has been flushed within `ms`.
async fn read_stream(h: &Harness, ms: u64) -> String {
    let res = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("stream");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    // nginx buffers a proxied response by default, which turns a live stream
    // into a stream that arrives all at once, later.
    assert_eq!(
        res.headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no")
    );
    let mut body = Box::pin(res.into_body().into_data_stream());
    let mut out = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, body.next()).await {
        out.push_str(&String::from_utf8_lossy(&chunk));
    }
    out
}

#[tokio::test]
async fn the_stream_opens_with_a_comment_a_retry_and_a_hello() {
    let h = Harness::new().await;
    let text = read_stream(&h, 300).await;
    let mut lines = text.split("\n\n");
    assert_eq!(lines.next(), Some(": narrator live"));
    assert_eq!(lines.next(), Some("retry: 3000"));
    let hello = lines.next().unwrap_or("");
    assert!(hello.starts_with("id: 0\nevent: hello\ndata: {"), "{hello}");
    assert!(hello.contains("\"heartbeat_s\""), "{hello}");
}

#[tokio::test]
async fn a_position_write_reaches_the_other_devices() {
    let h = Harness::new().await;
    h.load().await;
    // Subscribe first; the bus is free to call when nobody is listening, which
    // means an event published before the stream opens is simply not sent.
    let mut rx = h.state.bus.subscribe();
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 3}))
        .await;
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("no event")
        .expect("closed");
    // /api/open forces a position save, which is what publishes `position`.
    let names: Vec<&str> = std::iter::once(ev.name).collect();
    assert!(
        names.contains(&"position") || names.contains(&"render"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_named_position_is_recorded_without_loading_the_book() {
    let h = Harness::new().await;
    let (code, _) = h
        .post_json(
            "/api/position",
            json!({"book": "Some Other Book.epub", "chapter": 4, "chunk": 9,
                   "chapter_title": "IV", "chunks_total": 30, "chapters_total": 12}),
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    let pos = vault::load_positions(&h.state.cfg.positions_dir);
    let rec = &pos["Some Other Book.epub"];
    assert_eq!(rec["chapter"], json!(4));
    assert_eq!(rec["chunk"], json!(9));
    assert_eq!(rec["chapter_title"], json!("IV"));

    // Last write wins, and fields not supplied are kept.
    h.post_json(
        "/api/position",
        json!({"book": "Some Other Book.epub", "chunk": 11}),
    )
    .await;
    let pos = vault::load_positions(&h.state.cfg.positions_dir);
    let rec = &pos["Some Other Book.epub"];
    assert_eq!(rec["chunk"], json!(11));
    assert_eq!(rec["chapter"], json!(4), "kept from the existing record");
    assert_eq!(rec["chunks_total"], json!(30), "kept");

    // And the Reading Log was rewritten beside it.
    let log = std::fs::read_to_string(vault::log_file(&h.state.cfg.positions_dir)).expect("log");
    assert!(
        log.contains("| Some Other Book | 5/12 IV | 12/30 |"),
        "{log}"
    );
}

#[tokio::test]
async fn the_playhead_save_is_throttled_but_open_and_pause_are_not() {
    let h = Harness::new().await;
    h.load().await;
    let f = vault::positions_file(&h.state.cfg.positions_dir);

    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    assert!(f.exists(), "open forces a write");
    let first = std::fs::read_to_string(&f).expect("read");

    // A burst of playhead reports inside the 15 s window writes nothing more.
    for i in 1..6 {
        h.post_json("/api/playhead", json!({"chunk": i})).await;
    }
    assert_eq!(
        std::fs::read_to_string(&f).expect("read"),
        first,
        "the playhead save is throttled to one write per 15 s"
    );

    // Pause forces one.
    h.post_json("/api/pause", json!({})).await;
    let after = std::fs::read_to_string(&f).expect("read");
    assert!(after.contains("\"chunk\": 5"), "{after}");
}

#[tokio::test]
async fn render_progress_is_coalesced_but_the_edges_are_not() {
    let h = Harness::with(|c| c.sse_render_min_s = 3600.0).await;
    h.load().await;
    let mut rx = h.state.bus.subscribe();
    h.state.bus.emit_render("progress", json!({"chapter": 0}));
    h.state.bus.emit_render("progress", json!({"chapter": 0}));
    h.state.bus.emit_render("complete", json!({"chapter": 0}));
    h.state.bus.emit_render("packed", json!({"chapter": 0}));
    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        kinds.push(
            ev.data
                .get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }
    assert_eq!(kinds, vec!["progress", "complete", "packed"]);
}
