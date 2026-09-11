//! The six things the reader asked the rewrite for.
//!
//! `~/git/narrator/web/RUST-NOTES.md` is a list of requirements written against
//! the python server by the person porting the reader, each with the client-side
//! mitigation standing in for it. These tests are the other half of that
//! document: one per requirement, so "fixed in the Rust server" is a thing that
//! can be checked rather than claimed.
//!
//! Every one of them is **additive**. A client that sends none of the new
//! parameters gets exactly the python behaviour, which is what keeps the
//! Obsidian plugin working untouched.

mod harness;

use std::time::Instant;

use axum::http::StatusCode;
use harness::Harness;
use serde_json::json;

/// 1. `/api/load` must not re-parse a book that has not changed.
#[tokio::test]
async fn a_second_load_reuses_the_plan_instead_of_re_parsing() {
    let h = Harness::new().await;
    let first = h.load().await;
    let key = first["key"].as_str().unwrap_or("").to_string();

    // The proof that no parse happened: replace the file's *contents* with junk
    // while keeping its size and mtime. Nothing can parse that; a server that
    // still answers with the same plan read it from the cache.
    let epub = std::path::PathBuf::from(h.book_path());
    let saved = std::fs::read(&epub).expect("read");
    let times = std::fs::metadata(&epub).expect("stat");
    let junk = vec![b'\0'; saved.len()];
    std::fs::write(&epub, &junk).expect("clobber");
    restore_mtime(&epub, &times);

    let t0 = Instant::now();
    let (code, second) = h
        .post_json("/api/load", json!({"path": h.book_path()}))
        .await;
    let elapsed = t0.elapsed();
    assert_eq!(code, StatusCode::OK, "{second}");
    assert_eq!(second["key"].as_str(), Some(key.as_str()));
    assert_eq!(second["chapters"], first["chapters"], "the same plan");
    assert!(elapsed.as_secs() < 5, "took {elapsed:?}");

    // A file whose size changed is *not* trusted: the cache is a statement that
    // the content has not changed, and the junk is still junk.
    std::fs::write(&epub, [junk.as_slice(), b"  "].concat()).expect("grow");
    let (code, third) = h
        .post_json("/api/load", json!({"path": h.book_path()}))
        .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{third}");

    // Put it back and the cache is valid again.
    std::fs::write(&epub, &saved).expect("restore");
    let (code, fourth) = h
        .post_json("/api/load", json!({"path": h.book_path()}))
        .await;
    assert_eq!(code, StatusCode::OK, "{fourth}");
    assert_eq!(fourth["chapters"], first["chapters"]);
}

/// Keep an mtime across a rewrite, so only the *content* differs.
fn restore_mtime(p: &std::path::Path, md: &std::fs::Metadata) {
    if let (Ok(f), Ok(m)) = (std::fs::File::options().write(true).open(p), md.modified()) {
        let _ = f.set_times(std::fs::FileTimes::new().set_modified(m));
    }
}

#[tokio::test]
async fn a_book_that_was_never_parsed_is_still_parsed() {
    let h = Harness::new().await;
    // No cache at all: the first load must do the work.
    let (code, body) = h
        .post_json("/api/load", json!({"path": h.book_path()}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(
        body["chapters"].as_array().is_some_and(|c| !c.is_empty()),
        "{body}"
    );
    // The stamp beside plan.json is what makes the *next* one free.
    let key = body["key"].as_str().unwrap_or("");
    assert!(narrator::cache::book_dir(&h.work(), key)
        .join("parse.json")
        .exists());
}

/// 2. `/api/chapter/{ci}` must take `?book=`.
#[tokio::test]
async fn chapter_text_is_servable_for_a_book_the_session_is_not_holding() {
    let h = Harness::new().await;
    let first = h.load().await;
    let key = first["key"].as_str().unwrap_or("").to_string();
    let want = h.get_json("/api/chapter/1").await.1;

    // Swap the session onto another book entirely.
    let other = h.root().join("books/Other (2026).epub");
    std::fs::copy(harness::fixture_epub(), &other).expect("copy");
    let (code, second) = h
        .post_json("/api/load", json!({"path": other.to_string_lossy()}))
        .await;
    assert_eq!(code, StatusCode::OK, "{second}");
    assert_eq!(second["key"].as_str(), Some("Other (2026)"));

    // The first book's words are still answerable, out of its text bundle.
    let enc: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
    let (code, got) = h.get_json(&format!("/api/chapter/1?book={enc}")).await;
    assert_eq!(code, StatusCode::OK, "{got}");
    assert_eq!(got["chunks"], want["chunks"]);
    assert_eq!(got["paras"], want["paras"]);
    assert_eq!(got["i"], json!(1));

    // A key nobody has ever loaded is a 404, not the loaded book's words.
    let (code, got) = h.get_json("/api/chapter/1?book=Nothing").await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{got}");
    // And no `?book=` still means the session, exactly as before.
    let (code, got) = h.get_json("/api/chapter/1").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(got["chunks"], want["chunks"], "same fixture, same words");
}

/// 3. A position needs an unambiguous instant.
#[tokio::test]
async fn a_returned_position_carries_epoch_milliseconds() {
    let h = Harness::new().await;
    h.load().await;
    h.post_json("/api/open", json!({"chapter": 1, "chunk": 2}))
        .await;
    let again = h.load().await;
    let pos = &again["position"];
    assert!(pos.is_object(), "{again}");
    // The naive stamp is kept byte-identical - it is what goes in the vault -
    // and the instant rides along beside it.
    let naive = pos["updated"].as_str().unwrap_or("");
    assert_eq!(naive.len(), 19, "{naive}");
    let ms = pos["updated_ms"].as_i64().expect("updated_ms");
    let now = chrono::Utc::now().timestamp_millis();
    assert!((now - ms).abs() < 5 * 60_000, "ms {ms} vs now {now}");
    assert_eq!(pos["chapter"], json!(1));
    assert_eq!(pos["chunk"], json!(2));
}

/// 4. Playback endpoints must not be aimable at the wrong book.
#[tokio::test]
async fn open_and_playhead_refuse_a_book_the_session_is_not_holding() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();

    let (code, body) = h
        .post_json(
            "/api/open",
            json!({"chapter": 1, "chunk": 5, "book": "Some Other Book"}),
        )
        .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains(&key)),
        "{body}"
    );
    // Nothing moved.
    let s = h.get_json("/api/status").await.1;
    assert_eq!(s["chapter"], json!(0));
    assert_eq!(s["playhead"], json!(0));

    let (code, _) = h
        .post_json(
            "/api/playhead",
            json!({"chunk": 9, "book": "Some Other Book"}),
        )
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(h.get_json("/api/status").await.1["playhead"], json!(0));

    // The right key, and the omitted key, both work.
    for body in [
        json!({"chapter": 1, "chunk": 3, "book": key}),
        json!({"chapter": 1, "chunk": 3}),
    ] {
        let (code, _) = h.post_json("/api/open", body).await;
        assert_eq!(code, StatusCode::OK);
    }
    assert_eq!(h.get_json("/api/status").await.1["playhead"], json!(3));
}

/// 5. A missing vault must not silently disable positions.
#[tokio::test]
async fn an_unwritable_positions_dir_is_a_health_problem_not_a_silence() {
    let h = Harness::new().await;
    let (code, body) = h.get_json("/healthz").await;
    assert_eq!(code, StatusCode::OK);
    // No vault mounted: say where the writes are actually going.
    assert_eq!(body["vault"], json!(null));
    assert!(
        body["positions_dir"]
            .as_str()
            .is_some_and(|p| !p.is_empty()),
        "{body}"
    );

    // Now make that directory impossible to write.
    let blocked = Harness::with(|c| {
        c.positions_dir = c.work.join("blocked/02 - Studies");
    })
    .await;
    std::fs::create_dir_all(blocked.state.cfg.work.clone()).expect("work");
    std::fs::write(blocked.state.cfg.work.join("blocked"), b"not a directory").expect("file");
    let (code, body) = blocked.get_json("/healthz").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["problems"].as_array().is_some_and(|p| p
            .iter()
            .any(|x| x.as_str().is_some_and(|s| s.contains("positions dir")))),
        "{body}"
    );
}

/// 6. `/api/chapters` must be windowable.
#[tokio::test]
async fn the_chapter_list_takes_a_range() {
    let h = Harness::new().await;
    let load = h.load().await;
    let n = load["chapters"].as_array().map(Vec::len).unwrap_or(0);
    assert!(n >= 3, "the fixture needs a few chapters");

    let (code, all) = h.get_json("/api/chapters").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(all["chapters"].as_array().map(Vec::len), Some(n));
    assert_eq!(all["from"], json!(0));
    assert_eq!(all["to"], json!(n - 1));
    assert_eq!(all["total"], json!(n));

    let (code, win) = h.get_json("/api/chapters?from=1&to=2").await;
    assert_eq!(code, StatusCode::OK);
    let rows = win["chapters"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["i"], json!(1));
    assert_eq!(rows[1]["i"], json!(2));
    assert_eq!(
        win["total"],
        json!(n),
        "the total is the book, not the window"
    );

    // Out-of-range bounds clamp rather than error.
    let (code, win) = h.get_json("/api/chapters?from=999&to=9999").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(win["from"], json!(n - 1));
    assert_eq!(win["chapters"].as_array().map(Vec::len), Some(1));
}
