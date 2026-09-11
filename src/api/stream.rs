//! `GET /api/events` — Server-Sent Events: `position`, `render`, `books`,
//! `note`.
//!
//! A stream, not a contract for *state*: every event says "this changed", and the
//! client refetches the endpoint that owns it. That is what makes a missed event
//! — a dropped queue entry, a reconnect — a non-event, and it is why nothing is
//! replayed and `Last-Event-ID` is accepted and ignored.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::events::{comment, retry, sse};
use crate::state::AppState;

#[utoipa::path(
    get, path = "/api/events", tag = "session",
    responses((status = 200, content_type = "text/event-stream", body = String,
               description = "hello, then position/render/books/note events and heartbeats"))
)]
pub async fn events(State(st): State<Arc<AppState>>) -> Response {
    let mut rx = st.bus.subscribe();
    let hello = {
        let s = st.session();
        json!({"heartbeat_s": st.cfg.sse_heartbeat_s, "book": s.book_name(),
               "key": s.key(), "chapter": s.chapter})
    };
    let heartbeat = Duration::from_secs_f64(st.cfg.sse_heartbeat_s.max(1.0));
    let retry_ms = st.cfg.sse_retry_ms;

    let stream = async_stream::stream! {
        // Open with a comment and a hello so anything buffering in the middle has
        // to flush something immediately.
        yield Ok::<_, std::convert::Infallible>(comment("narrator live"));
        yield Ok(retry(retry_ms));
        yield Ok(sse("hello", &hello, Some(0)));
        loop {
            match tokio::time::timeout(heartbeat, rx.recv()).await {
                Err(_) => yield Ok(comment("ping")),
                Ok(Ok(ev)) => yield Ok(sse(ev.name, &ev.data, Some(ev.id))),
                Ok(Err(RecvError::Lagged(n))) => {
                    // A client that stopped reading is a phone in someone's
                    // pocket, not an error. Say so and carry on; it refetches.
                    yield Ok(comment(&format!("lagged {n}")));
                }
                Ok(Err(RecvError::Closed)) => break,
            }
        }
    };

    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache, no-store, no-transform"),
            (header::CONNECTION, "keep-alive"),
            // nginx buffers a proxied response by default, which turns a live
            // stream into a stream that arrives all at once, later.
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}
