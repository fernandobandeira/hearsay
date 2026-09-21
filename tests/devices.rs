//! Device identity, end to end: the header in, the `position` event out.
//!
//! The bug behind all of this was reported as "the phone follows another device
//! to a chapter that is behind it". The reader's half is `arbitrate`
//! (`web/src/lib/live.ts`), and it cannot do its job on an event that does not
//! say **who** moved the position or **when** — which is what this suite is
//! about. Every assertion here is a field the reader now refuses to work
//! without, so a server that quietly stopped sending one would be a reader that
//! quietly went back to following its own echo.
//!
//! The other half of the contract is that none of it is required. A client that
//! sends no identity — the Obsidian plugin, the python-era reader, `curl` — gets
//! exactly the behaviour it got before any of this existed, and that is asserted
//! here too rather than assumed.

mod harness;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use harness::Harness;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Read `/api/events` for a while and return every `position` payload seen.
///
/// Incremental, like a browser's `EventSource`: the stream never ends, so the
/// only way to read it is to stop asking.
async fn positions_seen(h: &Harness, ms: u64, uri: &str) -> Vec<Value> {
    let res = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("stream");
    assert_eq!(res.status(), StatusCode::OK);
    let mut body = Box::pin(res.into_body().into_data_stream());
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, body.next()).await {
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
    let mut out = Vec::new();
    let mut in_position = false;
    for line in buf.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            in_position = name.trim() == "position";
        } else if let Some(data) = line.strip_prefix("data: ") {
            if in_position {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    out.push(v);
                }
            }
        }
    }
    out
}

/// Drive a position write from a named device while a stream is listening, and
/// hand back the event it produced.
async fn write_and_capture(h: &Harness, device: Option<&str>) -> Value {
    let headers: Vec<(&str, &str)> = match device {
        Some(d) => vec![("x-narrator-device", d)],
        None => vec![],
    };
    let stream = positions_seen(h, 700, "/api/events");
    let write = async {
        // A beat, so the stream is subscribed before the event is emitted. The
        // bus deliberately replays nothing, so an event sent before the
        // subscription genuinely never arrives.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let (code, _) = h
            .post_json_from("/api/open", json!({"chapter": 1, "chunk": 3}), &headers)
            .await;
        assert_eq!(code, StatusCode::OK);
    };
    let (seen, ()) = tokio::join!(stream, write);
    seen.into_iter()
        .next()
        .expect("a position event followed the open")
}

#[tokio::test]
async fn a_position_event_says_who_moved_it() {
    let h = Harness::new().await;
    h.load().await;
    let ev = write_and_capture(&h, Some("phone-1")).await;
    assert_eq!(
        ev.get("device").and_then(Value::as_str),
        Some("phone-1"),
        "the reader's own-echo test is an equality check on this field"
    );
}

#[tokio::test]
async fn a_position_event_carries_an_unambiguous_instant() {
    let h = Harness::new().await;
    h.load().await;
    let ev = write_and_capture(&h, Some("phone-1")).await;

    // `updated` stays the naive local stamp, byte-identical to what goes in the
    // vault — the python server's format, which the Obsidian plugin reads.
    let updated = ev.get("updated").and_then(Value::as_str).expect("updated");
    assert!(
        updated.len() == 19 && updated.contains('T') && !updated.ends_with('Z'),
        "updated must stay the naive local stamp the vault holds, got {updated:?}"
    );
    // ...and the instant rides beside it, which is the part a browser can order.
    let ms = ev
        .get("updated_ms")
        .and_then(Value::as_i64)
        .expect("updated_ms");
    assert!(ms > 1_600_000_000_000, "an epoch-ms stamp, got {ms}");
}

#[tokio::test]
async fn the_sequence_number_only_ever_goes_up() {
    let h = Harness::new().await;
    h.load().await;
    let a = write_and_capture(&h, Some("phone-1")).await;
    let b = write_and_capture(&h, Some("phone-1")).await;
    let seq = |v: &Value| v.get("seq").and_then(Value::as_u64).expect("seq");
    assert!(
        seq(&a) >= 1,
        "never zero: absent and zero are different claims"
    );
    assert!(
        seq(&b) > seq(&a),
        "the tie-break under updated_ms has to be monotonic, got {} then {}",
        seq(&a),
        seq(&b)
    );
}

#[tokio::test]
async fn two_devices_are_told_apart() {
    let h = Harness::new().await;
    h.load().await;
    let phone = write_and_capture(&h, Some("phone-1")).await;
    let laptop = write_and_capture(&h, Some("laptop-2")).await;
    assert_eq!(phone.get("device").and_then(Value::as_str), Some("phone-1"));
    assert_eq!(
        laptop.get("device").and_then(Value::as_str),
        Some("laptop-2")
    );
}

#[tokio::test]
async fn a_client_that_sends_no_identity_still_works() {
    // The whole additive claim, in one test: no header, same 200, same event,
    // and an empty device rather than a refusal or a missing field.
    let h = Harness::new().await;
    h.load().await;
    let ev = write_and_capture(&h, None).await;
    assert_eq!(
        ev.get("device").and_then(Value::as_str),
        Some(""),
        "the anonymous device is a value, not an absence"
    );
    assert!(ev.get("chapter").is_some() && ev.get("chunk").is_some());
    assert!(
        ev.get("updated_ms").is_some(),
        "still ordered, just not named"
    );
}

#[tokio::test]
async fn a_malformed_identity_is_cleaned_rather_than_refused() {
    // A device id is decoration on a playhead report. Turning a bad one into a
    // 400 would lose the report, which is the thing that actually matters.
    let h = Harness::new().await;
    h.load().await;
    let ev = write_and_capture(&h, Some("../../etc/passwd")).await;
    let id = ev.get("device").and_then(Value::as_str).expect("device");
    assert!(
        !id.contains('/') && !id.contains('.'),
        "an id becomes a primary key and is echoed to other devices, got {id:?}"
    );
    assert_eq!(id, "etcpasswd");
}

#[tokio::test]
async fn a_named_write_carries_its_device_too() {
    // /api/position is the offline queue's route back, and it is the one write
    // that names a book the session may not hold. It has to be attributable for
    // the same reason the session's own writes do.
    let h = Harness::new().await;
    h.load().await;
    let stream = positions_seen(&h, 700, "/api/events");
    let write = async {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let (code, _) = h
            .post_json_from(
                "/api/position",
                json!({"book": "Elsewhere.epub", "chapter": 4, "chunk": 9}),
                &Harness::as_device("phone-1"),
            )
            .await;
        assert_eq!(code, StatusCode::OK);
    };
    let (seen, ()) = tokio::join!(stream, write);
    let ev = seen.into_iter().next().expect("a position event");
    assert_eq!(ev.get("source").and_then(Value::as_str), Some("api"));
    assert_eq!(ev.get("device").and_then(Value::as_str), Some("phone-1"));
    assert_eq!(
        ev.get("book").and_then(Value::as_str),
        Some("Elsewhere.epub")
    );
    assert!(ev.get("updated_ms").is_some());
}

#[tokio::test]
async fn the_event_stream_takes_a_device_in_its_query() {
    // `EventSource` cannot set a header — the one reason the query spelling
    // exists at all. If this ever stops working, the server stops knowing which
    // devices are connected, and the scheduler's "the book somebody is actually
    // reading" goes back to being a guess.
    let h = Harness::new().await;
    h.load().await;
    let res = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?device=phone-1&device_name=iPhone")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("stream");
    assert_eq!(res.status(), StatusCode::OK);
    // The guard is created inside the stream body, so nothing is on the roster
    // until the stream is actually polled. Read the opening `hello` to get there
    // — and keep the stream alive across the assertion, which is the whole point.
    let mut body = Box::pin(res.into_body().into_data_stream());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let mut buf = String::new();
    while !buf.contains("hello") {
        match tokio::time::timeout_at(deadline, body.next()).await {
            Ok(Some(Ok(c))) => buf.push_str(&String::from_utf8_lossy(&c)),
            _ => break,
        }
    }
    let live = h.state.roster.active(Duration::from_secs(60));
    assert_eq!(
        live.len(),
        1,
        "a stream names its device, which is how presence is known at all"
    );
    assert_eq!(live[0].0, "phone-1");
    assert_eq!(live[0].1.name, "iPhone");
    drop(body);
}

#[tokio::test]
async fn presence_is_forgotten_when_the_stream_goes() {
    // Presence is the one piece of device state that is deliberately not durable:
    // a server that believes three devices are listening when none are would
    // render ahead for readers who are not there. The guard is RAII, so the
    // stream ending *any* way takes the entry with it — including the way that
    // actually happens, which is the task being dropped when a phone locks.
    let h = Harness::new().await;
    h.load().await;
    {
        let res = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events?device=ghost")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("stream");
        let mut body = Box::pin(res.into_body().into_data_stream());
        let _ = tokio::time::timeout(Duration::from_millis(300), body.next()).await;
        assert_eq!(h.state.roster.active(Duration::from_secs(60)).len(), 1);
    }
    assert!(
        h.state.roster.active(Duration::from_secs(60)).is_empty(),
        "a closed stream leaves nobody behind"
    );
}

#[tokio::test]
async fn an_anonymous_stream_puts_nobody_on_the_roster() {
    // It could never be taken off again — there is no id to remove — so a
    // phantom reader would sit there for the life of the process and the
    // scheduler would render for it.
    let h = Harness::new().await;
    h.load().await;
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
    let mut body = Box::pin(res.into_body().into_data_stream());
    let _ = tokio::time::timeout(Duration::from_millis(300), body.next()).await;
    assert!(h.state.roster.active(Duration::from_secs(60)).is_empty());
    drop(body);
}
