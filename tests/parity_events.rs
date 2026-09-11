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

/// A live `/api/events` connection, read the way a browser's `EventSource`
/// reads one: incrementally, and only when it gets around to it.
struct Stream {
    body: std::pin::Pin<Box<axum::body::BodyDataStream>>,
    buf: String,
}

impl Stream {
    /// Pull for up to `ms`, appending whatever arrives, and stop early once
    /// `want` shows up. Returns everything read so far.
    async fn read_for(&mut self, ms: u64, want: Option<&str>) -> &str {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(w) = want {
                if self.buf.contains(w) {
                    break;
                }
            }
            match tokio::time::timeout_at(deadline, self.body.next()).await {
                Ok(Some(Ok(chunk))) => self.buf.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(_) => break,
                Err(_) => break,
            }
        }
        &self.buf
    }
}

/// Open `/api/events`, asserting the headers a live stream needs, and hand back
/// the connection.
async fn open_stream(h: &Harness) -> Stream {
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
    Stream {
        body: Box::pin(res.into_body().into_data_stream()),
        buf: String::new(),
    }
}

#[tokio::test]
async fn the_stream_opens_with_a_comment_a_retry_and_a_hello() {
    let h = Harness::new().await;
    let mut s = open_stream(&h).await;
    let text = s.read_for(300, Some("event: hello")).await;
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

/// The whole feature, end to end over the wire: a client holding the stream
/// open is *told* when something changes, rather than finding out on its next
/// poll. Everything else in this file tests the bus; this tests the pipe.
#[tokio::test]
async fn a_live_client_is_told_about_a_position_write_over_the_wire() {
    let h = Harness::new().await;
    let mut s = open_stream(&h).await;
    s.read_for(500, Some("event: hello")).await;

    let (code, _) = h
        .post_json(
            "/api/position",
            json!({"book": "Another Book.epub", "chapter": 7, "chunk": 21}),
        )
        .await;
    assert_eq!(code, StatusCode::OK);

    let text = s.read_for(2_000, Some("event: position")).await;
    let frame = text
        .split("\n\n")
        .find(|f| f.contains("event: position"))
        .unwrap_or_default();
    assert!(frame.starts_with("id: "), "events are numbered: {frame}");
    let data = frame
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(data).expect("one line of JSON: {data}");
    assert_eq!(v["book"], json!("Another Book.epub"));
    assert_eq!(v["chapter"], json!(7));
    assert_eq!(v["chunk"], json!(21));
    // Which device wrote it, so a reader can tell its own echo from news.
    assert_eq!(v["source"], json!("api"));
}

/// The rule that keeps a phone in a pocket from stopping the renderer: the bus
/// never blocks and never fails on a subscriber that has stopped reading. It
/// drops the events that subscriber missed and tells it how many.
#[tokio::test]
async fn a_slow_subscriber_is_dropped_rather_than_backpressuring_the_renderer() {
    use tokio::sync::broadcast::error::TryRecvError;
    let h = Harness::with(|c| {
        c.sse_queue = 4;
        c.sse_render_min_s = 0.0;
    })
    .await;
    let mut rx = h.state.bus.subscribe();
    // Twenty events into a four-deep queue, with nobody reading.
    for i in 0..20 {
        assert!(
            h.state.bus.emit("position", json!({"chunk": i})),
            "emit {i} must not fail on a full queue"
        );
    }
    // The reader is told it lagged, once, and then carries on with what is left.
    match rx.try_recv() {
        Err(TryRecvError::Lagged(n)) => assert!(n >= 16, "lagged by {n}"),
        other => panic!("expected a lag, got {other:?}"),
    }
    let mut seen = 0;
    while let Ok(ev) = rx.try_recv() {
        assert_eq!(ev.name, "position");
        seen += 1;
    }
    assert_eq!(seen, 4, "only the last queue-full survives");
}

/// And what that looks like on the wire: a comment, not a closed connection.
/// The client refetches, because every event only ever means "go and look".
#[tokio::test]
async fn a_client_that_stopped_reading_is_told_it_lagged_and_keeps_its_connection() {
    let h = Harness::with(|c| c.sse_queue = 2).await;
    let mut s = open_stream(&h).await;
    s.read_for(500, Some("event: hello")).await;
    // Nothing is polling the body now, so these queue up behind a 2-deep
    // channel and the stream's own receiver falls behind.
    for i in 0..30 {
        h.state.bus.emit("position", json!({"chunk": i}));
    }
    let text = s.read_for(2_000, Some(": lagged")).await;
    assert!(text.contains(": lagged"), "{text}");
    // Still open: an event published after the lag still arrives.
    h.state.bus.emit("note", json!({"file": "20260911-a.md"}));
    let text = s.read_for(2_000, Some("event: note")).await;
    assert!(text.contains("20260911-a.md"), "{text}");
}

/// A new epub on disk is a `books` event, with no git coupling and nothing to
/// remember to call. This is the watcher, which is otherwise only exercised in
/// production.
#[tokio::test]
async fn an_epub_appearing_in_the_library_becomes_a_books_event() {
    let h = Harness::with(|c| c.watch_books = true).await;
    let mut rx = h.state.bus.subscribe();
    narrator::watch::spawn(h.state.clone());
    // The watcher registers asynchronously; a file created in the same
    // microsecond can genuinely be missed, and that is not what is under test.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let dropped = h.state.cfg.books[0].join("Dropped In (2026).epub");
    std::fs::copy(harness::fixture_epub(), &dropped).expect("copy");

    let ev = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.name == "books" => return Some(ev),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .expect("no books event inside ten seconds")
    .expect("the bus closed");
    assert_eq!(ev.data["changed"][0], json!("Dropped In (2026).epub"));
    assert_eq!(ev.data["count"], json!(1));
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
