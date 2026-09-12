//! The one loss this system cannot tolerate: a voice memo that is never filed.
//!
//! A memo lives in IndexedDB on a phone and nowhere else until `POST /api/note`
//! answers 2xx with the note it wrote. On the 2-core box a transcription is
//! minutes long, and for that whole window the phone may lock its screen, the
//! PWA may be backgrounded, the tunnel may blip — and every one of those drops
//! the request future. What is asserted here is that none of it costs the note,
//! and that the retry the reader then makes is a replay rather than a second
//! multi-minute transcription and a duplicate note in the vault.
//!
//! These need a transcript, which means `fake_stt`: the recording's own bytes
//! stand in for whisper (`NARRATOR_FAKE_STT`), exactly as `NARRATOR_FAKE_TTS`
//! stands in for Kokoro. Everything after it — the vault write, the note's file
//! name, the delivery contract, the idempotency record — is the real thing.

mod harness;

use std::time::Duration;

use axum::http::StatusCode;
use base64::Engine as _;
use harness::Harness;
use serde_json::{json, Value};

async fn speaking() -> Harness {
    Harness::with(|c| c.fake_stt = true).await
}

fn recorded(words: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(words)
}

fn notes(h: &Harness) -> Vec<String> {
    let mut v = std::fs::read_dir(&h.state.cfg.notes_dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn recordings(h: &Harness) -> usize {
    std::fs::read_dir(h.work().join("notes-audio"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// Wait for a side effect a detached task is responsible for.
async fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test]
async fn a_memo_is_filed_once_however_many_times_it_is_posted() {
    let h = speaking().await;
    h.load().await;
    let body = json!({"audio": recorded("the whole book turns on this"),
                      "mime": "audio/webm", "chapter": 0, "chunk": 1,
                      "id": "9f3b4c7a-2e11-4f00-9a10-7b1d2c3e4f55"});

    let (code, first) = h.post_json("/api/note", body.clone()).await;
    assert_eq!(code, StatusCode::OK, "{first}");
    assert_eq!(first["ok"], json!(true));
    assert_eq!(first["text"], json!("the whole book turns on this"));
    assert_eq!(first["language"], json!("en"));
    let file = first["file"].as_str().unwrap_or_default().to_string();
    assert!(file.ends_with(".md"), "{first}");
    assert_eq!(notes(&h), vec![file.clone()]);

    // The reader never saw that answer — the screen locked — so it asks again
    // with the same id. The same body comes back, and nothing else happens:
    // no second whisper run, no second note, no second recording on disk.
    let (again, second) = h.post_json("/api/note", body).await;
    assert_eq!(again, StatusCode::OK, "{second}");
    assert_eq!(first, second, "a replay is the same answer, to the byte");
    assert_eq!(notes(&h), vec![file], "one memo, one note");
    assert_eq!(recordings(&h), 1, "one memo, one recording");

    // And the note in the vault really is the words that were recorded.
    let md = std::fs::read_to_string(h.state.cfg.notes_dir.join(&notes(&h)[0])).expect("the note");
    assert!(md.contains("the whole book turns on this"), "{md}");
}

#[tokio::test]
async fn a_client_that_stops_listening_still_gets_its_note_filed() {
    // The production bug, reproduced: the request future is dropped while the
    // handler is waiting on the transcription. Before the fix that cancelled the
    // filing outright — the blocking job ran on and its result was thrown away —
    // and the memo could never be delivered, because every retry met the same
    // fate on a connection that never survived the window.
    let h = speaking().await;
    h.load().await;
    let body = json!({"audio": recorded("this thought must survive"),
                      "mime": "audio/webm", "id": "dropped-mid-flight"});

    let dropped = tokio::time::timeout(
        Duration::ZERO,
        h.post_json("/api/note", body.clone()),
    )
    .await;
    assert!(
        dropped.is_err(),
        "the request has to still be in flight when the client goes away"
    );

    // Nobody is listening any more. The note is written anyway.
    assert!(
        eventually(|| !notes(&h).is_empty()).await,
        "a dropped client must not cost the note"
    );
    let filed = notes(&h);
    assert_eq!(filed.len(), 1, "{filed:?}");

    // And when the phone comes back and re-posts what it still holds, it is
    // handed the note that was already filed — which is what finally lets the
    // outbox delete its copy.
    let (code, got) = h.post_json("/api/note", body).await;
    assert_eq!(code, StatusCode::OK, "{got}");
    assert_eq!(got["file"], json!(filed[0]));
    assert_eq!(got["text"], json!("this thought must survive"));
    assert_eq!(notes(&h), filed, "still one note");
    assert_eq!(recordings(&h), 1, "still one recording");
}

#[tokio::test]
async fn a_memo_with_no_id_is_identified_by_its_own_bytes() {
    // Today's reader and the Obsidian plugin send no id. They are deduplicated
    // anyway, because the recording is the memo.
    let h = speaking().await;
    h.load().await;
    let body = json!({"audio": recorded("no id on this one"), "mime": "audio/webm"});
    let (_, first) = h.post_json("/api/note", body.clone()).await;
    let (code, second) = h.post_json("/api/note", body).await;
    assert_eq!(code, StatusCode::OK, "{second}");
    assert_eq!(first, second);
    assert_eq!(notes(&h).len(), 1);
    assert_eq!(recordings(&h), 1);

    // A *different* recording is a different thought, however close together the
    // two were posted.
    let (code, other) = h
        .post_json(
            "/api/note",
            json!({"audio": recorded("a second, different thought"), "mime": "audio/webm"}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{other}");
    assert_ne!(other["file"], first["file"]);
    assert_eq!(notes(&h).len(), 2);
}

#[tokio::test]
async fn the_record_of_a_filed_memo_survives_a_restart() {
    // The registry of in-flight jobs is memory and the record is not, because a
    // container restarted between the vault write and the phone hearing about it
    // is exactly when a duplicate would be filed.
    let mut h = speaking().await;
    h.load().await;
    let body = json!({"audio": recorded("written before the restart"),
                      "mime": "audio/webm", "id": "outlives-the-process"});
    let (_, first) = h.post_json("/api/note", body.clone()).await;
    let filed = notes(&h);
    assert_eq!(filed.len(), 1);

    h.restart().await;
    let (code, after) = h.post_json("/api/note", body).await;
    assert_eq!(code, StatusCode::OK, "{after}");
    assert_eq!(first, after, "the same answer from a new process");
    assert_eq!(notes(&h), filed, "and no second note");
}

#[tokio::test]
async fn two_posts_of_one_memo_at_the_same_time_file_one_note() {
    // Returning to the PWA fires visibilitychange *and* focus, which is how two
    // identical uploads landed a second apart in production. The reader now
    // coalesces them; the server does not have to trust that it does.
    let h = speaking().await;
    h.load().await;
    let body = json!({"audio": recorded("said once"), "mime": "audio/webm",
                      "id": "posted-twice-at-once"});
    let (a, b) = tokio::join!(
        h.post_json("/api/note", body.clone()),
        h.post_json("/api/note", body),
    );
    assert_eq!(a.0, StatusCode::OK, "{:?}", a.1);
    assert_eq!(b.0, StatusCode::OK, "{:?}", b.1);
    assert_eq!(a.1, b.1, "the second attached to the first's job");
    assert_eq!(notes(&h).len(), 1);
    assert_eq!(recordings(&h), 1);
}

#[tokio::test]
async fn a_memo_that_was_never_filed_is_not_replayed() {
    // Only a filed note short-circuits. A memo whisper heard nothing in is a
    // 400, is recorded nowhere, and is transcribed again next time — which
    // matters, because "nothing heard" can be a decode that failed rather than a
    // silence, and the recording is still the only copy.
    let h = speaking().await;
    h.load().await;
    let body = json!({"audio": recorded("   "), "mime": "audio/webm", "id": "heard-nothing"});
    for _ in 0..2 {
        let (code, got) = h.post_json("/api/note", body.clone()).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{got}");
        assert!(
            got["error"]
                .as_str()
                .unwrap_or_default()
                .contains("heard nothing"),
            "{got}"
        );
    }
    assert!(notes(&h).is_empty());
    // The recording itself is kept, and kept once.
    assert_eq!(recordings(&h), 1);
}

#[tokio::test]
async fn a_refusal_does_not_claim_the_memos_id() {
    // A memo naming a book this server has no text for is a 404 and stays in the
    // outbox. The id must be free afterwards: a retry once the book is loaded
    // has to be able to run, not attach to a job nobody is running.
    let h = speaking().await;
    h.load().await;
    let mut body = json!({"audio": recorded("recorded elsewhere"), "mime": "audio/webm",
                          "book": "A Book Never Loaded Here", "id": "refused-then-filed"});
    let (code, got) = h.post_json("/api/note", body.clone()).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{got}");
    assert_eq!(recordings(&h), 0, "nothing was written for a refused memo");

    body["book"] = Value::Null;
    let (code, got) = h.post_json("/api/note", body).await;
    assert_eq!(code, StatusCode::OK, "{got}");
    assert_eq!(notes(&h).len(), 1);
}
