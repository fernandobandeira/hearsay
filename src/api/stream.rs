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

use crate::api::device::{Connected, Device};
use crate::events::{comment, retry, sse};
use crate::state::AppState;

#[utoipa::path(
    get, path = "/api/events", tag = "session",
    params(
        ("device" = Option<String>, Query,
         description = "This device's id. A query parameter rather than the \
`X-Narrator-Device` header every other endpoint takes, because `EventSource` \
cannot set request headers — and this is the endpoint that tells the server which \
devices are connected right now."),
        ("device_name" = Option<String>, Query, description = "A label for that device, display only."),
    ),
    responses((status = 200, content_type = "text/event-stream", body = String,
               description = "hello, then position/render/books/note events and heartbeats"))
)]
pub async fn events(State(st): State<Arc<AppState>>, dev: Device) -> Response {
    let mut rx = st.bus.subscribe();
    let hello = {
        let s = st.session();
        json!({"heartbeat_s": st.cfg.sse_heartbeat_s, "book": s.book_name(),
               "key": s.key(), "chapter": s.chapter})
    };
    // Who is listening, for as long as they are listening. The guard is moved
    // into the stream below, so the roster entry lives exactly as long as the
    // connection does — including when the connection ends by the task being
    // dropped, which is what a phone locking mid-stream looks like and is the
    // case a tidy-up at the end of the handler would miss.
    //
    // This is what `/api/events` knows that no other endpoint can: `EventSource`
    // cannot send a header, so the id arrives as `?device=` — see
    // `crate::api::device`.
    let here = Connected::new(st.roster.clone(), &dev, st.session().book_name());
    if dev.known() {
        tracing::debug!("device {} connected ({})", dev.id, dev.name);
    }
    let heartbeat = Duration::from_secs_f64(st.cfg.sse_heartbeat_s.max(1.0));
    let retry_ms = st.cfg.sse_retry_ms;

    let stream = async_stream::stream! {
        // Held for the life of the stream and dropped with it. Named `_here`
        // rather than `_`, which would drop it immediately and defeat the point.
        let _here = here;
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
