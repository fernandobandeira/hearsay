//! The nine things the reader asked the rewrite for.
//!
//! The reader kept a list of requirements written against the python server —
//! each with the client-side mitigation standing in for it meanwhile — and these
//! tests are the other half of that document: one per requirement, so "fixed in
//! the Rust server" is a thing that can be checked rather than claimed. The list
//! itself is gone now that all nine are implemented; what each one *was* is the
//! doc comment on its test.
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

/// 7. The chapter endpoints must take `?book=` / `"book"`.
///
/// The one where a race is expensive rather than merely wrong: a single tap can
/// queue 74 chapters of rendering, and if the server swapped books between the
/// poll and the tap, those renders occupy the worker for hours on the wrong
/// novel. The reader had no mitigation that closed it.
#[tokio::test]
async fn every_chapter_endpoint_refuses_a_book_the_session_is_not_holding() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    let mine: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();

    // The view.
    let (code, body) = h.get_json("/api/chapters?book=Another%20Book").await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains(&key)),
        "the refusal says which book is loaded: {body}"
    );
    // The three verbs.
    for path in [
        "/api/chapters/render",
        "/api/chapters/build",
        "/api/chapters/cancel",
    ] {
        let (code, body) = h
            .post_json(path, json!({"chapters": [1, 2], "book": "Another Book"}))
            .await;
        assert_eq!(code, StatusCode::CONFLICT, "{path}: {body}");
    }
    // And nothing was queued by any of them.
    let s = h.get_json("/api/status").await.1;
    assert_eq!(s["queue"], json!([]), "{s}");
    assert_eq!(s["pack_queue"], json!([]));

    // The right key works, and so does no key at all - which is the python
    // behaviour the Obsidian plugin still relies on.
    let (code, body) = h.get_json(&format!("/api/chapters?book={mine}")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["key"].as_str(), Some(key.as_str()));
    let (code, body) = h
        .post_json(
            "/api/chapters/render",
            json!({"chapters": [2], "book": key}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["queue"], json!([2]));
    let (code, body) = h
        .post_json("/api/chapters/cancel", json!({"chapters": [2]}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["queue"], json!([]));

    // With no book loaded at all, a request that names one is still a 409 and
    // not a confident answer about nothing.
    let empty = Harness::new().await;
    let (code, _) = empty.get_json("/api/chapters?book=Anything").await;
    assert_eq!(code, StatusCode::CONFLICT);
}

/// 8. A chapter's packed size must be knowable before it is packed.
///
/// The confirm bar has to say how big a download will be while every chapter in
/// it is still an estimate, and the client used to hard-code the 64 kbit/s
/// default — so changing `CHAPTER_BITRATE` on the box made every figure in the
/// UI wrong by that ratio, silently.
#[tokio::test]
async fn the_packed_bitrate_and_a_per_chapter_size_estimate_are_reported() {
    let h = Harness::with(|c| c.chapter_bitrate = "128k".into()).await;
    h.load().await;

    let s = h.get_json("/api/status").await.1;
    assert_eq!(s["bitrate"], json!("128k"));
    // 128 kbit/s is 960 kB a minute; the client should not have to parse `k`.
    assert_eq!(s["bitrate_bytes_per_min"], json!(960_000.0));

    let rows = h.get_json("/api/chapters").await.1["chapters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!rows.is_empty());
    for r in &rows {
        let min = r["est_min"].as_f64().expect("est_min");
        let bytes = r["est_bytes"].as_f64().expect("est_bytes");
        assert_eq!(bytes, (min * 960_000.0).round(), "{r}");
    }

    // The default is still the default.
    let plain = Harness::new().await;
    plain.load().await;
    let s = plain.get_json("/api/status").await.1;
    assert_eq!(s["bitrate"], json!("64k"));
    assert_eq!(s["bitrate_bytes_per_min"], json!(480_000.0));
}

/// 9. `POST /api/chapters/build` must say what it refused, and why.
///
/// It answered `{built, building, rendering}`, and a chapter it would not pack
/// appeared in none of the three — indistinguishable from one nobody asked
/// about. The reader's mitigation was to ignore the response entirely and
/// re-ask every twenty seconds.
#[tokio::test]
async fn build_reports_what_it_refused_and_why() {
    let h = Harness::new().await;
    let load = h.load().await;
    let n = load["chapters"].as_array().map(Vec::len).unwrap_or(0);
    let key = load["key"].as_str().unwrap_or("").to_string();

    // Chapter 1 is untouched; chapter 0 gets a complete set of chunk wavs, the
    // way the render worker would have left it.
    let chunks = load["chapters"][0]["n"].as_u64().unwrap_or(0) as usize;
    let dir = narrator::cache::chapter_dir(&h.work(), &key, 0);
    std::fs::create_dir_all(&dir).expect("chapter dir");
    for i in 0..chunks {
        narrator::cache::write_silence_wav(&dir.join(format!("{i:05}.wav")), 0.1, 1, 24_000, 2)
            .expect("silence");
    }

    let (code, body) = h
        .post_json(
            "/api/chapters/build",
            json!({"chapters": [0, 1, 9999], "book": key}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    // The complete one was taken.
    assert_eq!(body["building"], json!([0]), "{body}");
    // The half-rendered one was not, and says so - with its progress, so the
    // client can tell "working on it" from "stuck".
    let refused = body["refused"].as_array().cloned().unwrap_or_default();
    let one = refused
        .iter()
        .find(|r| r["chapter"] == json!(1))
        .unwrap_or_else(|| panic!("chapter 1 in {body}"));
    assert_eq!(one["reason"], json!("not_rendered"));
    assert_eq!(one["rendered"], json!(0));
    assert!(one["n"].as_u64().unwrap_or(0) > 0, "{one}");
    assert!(
        body["rendering"]
            .as_array()
            .is_some_and(|r| r.contains(&json!(1))),
        "and it was queued to render: {body}"
    );
    // An index this book does not have is named rather than dropped.
    let far = refused
        .iter()
        .find(|r| r["chapter"] == json!(9999))
        .unwrap_or_else(|| panic!("9999 in {body}"));
    assert_eq!(far["reason"], json!("out_of_range"));
    assert!(n < 9999);
}

// -------------------------------------------------------- 10. and a restart

/// 10. A restart must not leave the reader talking to a server with no session.
///
/// Not on the original list, because it was not understood until it happened:
/// Fernando's reader stalled mid-chapter around a deploy and healed itself some
/// time later. Playback is one global in-memory session, and a container restart
/// empties it. A reader that is *already* mid-chapter never calls `/api/load` —
/// that is what picking a book does — so nothing refilled it, and in the meantime
/// `/api/chunk` 404ed (it is session-scoped), `/api/open`, `/api/playhead` and
/// `/api/chapters?book=` all answered 409, and the renderer had no plan to work
/// from. Every ingredient of the heal was already on disk; the only thing missing
/// was the name of the book, which is now `work/session.json`.
#[tokio::test]
async fn the_server_comes_back_on_the_book_it_was_reading() {
    let mut h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    let n = h.state.session().plan[1].chunks.len();
    assert!(n >= 3);

    h.post_json("/api/open", json!({"chapter": 1, "chunk": 2}))
        .await;
    let rendered = narrator::cache::chunk_path(&h.work(), &key, 1, 2);
    until("the opened chunk", 20.0, || rendered.exists()).await;

    // The redeploy.
    h.restart().await;

    // The session is back, at the position the vault holds.
    let (code, st) = h.get_json("/api/status").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(st["key"], json!(key), "{st}");
    assert_eq!(st["book"], json!("Fixture (2026).epub"), "{st}");
    assert_eq!(st["chapter"], json!(1), "{st}");
    assert_eq!(st["playhead"], json!(2), "{st}");
    assert!(st["chapters"].as_u64().unwrap_or(0) > 1, "the plan: {st}");

    // The audio the reader was in the middle of is servable again. Before this,
    // the key was "x" and every chunk in the book was a 404.
    let (code, body) = h.get("/api/chunk/1/00002.wav").await;
    assert_eq!(code, StatusCode::OK, "{}", body.len());

    // And the endpoints that name the book stop refusing.
    let (code, _) = h
        .post_json("/api/playhead", json!({"chunk": 3, "book": key}))
        .await;
    assert_eq!(code, StatusCode::OK, "a playhead report must not 409");
    // A playhead report is the only signal a mid-chapter reader gives a restarted
    // process, so it is what has to start the worker.
    assert!(narrator::render::render_alive(&h.state));
    let (code, _) = h
        .post_json("/api/open", json!({"chapter": 1, "chunk": 3, "book": key}))
        .await;
    assert_eq!(code, StatusCode::OK);
    let enc: String =
        percent_encoding::utf8_percent_encode(&key, percent_encoding::NON_ALPHANUMERIC).to_string();
    let (code, _) = h.get(&format!("/api/chapters?book={enc}")).await;
    assert_eq!(code, StatusCode::OK);

    // The chunk the reader is waiting on renders, with nobody having loaded
    // anything.
    let p = narrator::cache::chunk_path(&h.work(), &key, 1, 3);
    until("the chunk after the restart", 20.0, || p.exists()).await;
}

/// ... and it restores only what is still true. A book that has changed on disk
/// since it was loaded is a `/api/load`'s business, not a restart's: re-chunking
/// it here would silently move every stored position in it.
#[tokio::test]
async fn a_book_that_changed_is_not_restored() {
    let mut h = Harness::new().await;
    h.load().await;
    let epub = std::path::PathBuf::from(h.book_path());
    let mut bytes = std::fs::read(&epub).expect("read");
    bytes.extend_from_slice(b"and then some");
    std::fs::write(&epub, &bytes).expect("grow");

    h.restart().await;
    let (_, st) = h.get_json("/api/status").await;
    assert_eq!(st["key"], json!(null), "{st}");
    assert_eq!(st["book"], json!(null), "{st}");
    // Which is the old behaviour, and the reader's own heal covers it: it sees a
    // `hello` naming no book and loads one.
    let (code, _) = h
        .post_json(
            "/api/open",
            json!({"chapter": 0, "chunk": 0, "book": "Fixture (2026)"}),
        )
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
}

/// A fresh work directory has nothing to restore and must not care.
#[tokio::test]
async fn a_first_boot_restores_nothing() {
    let mut h = Harness::new().await;
    h.restart().await;
    let (code, st) = h.get_json("/api/status").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(st["book"], json!(null), "{st}");
    assert_eq!(st["status"], json!("idle"), "{st}");
}

/// Wait for a predicate, or fail.
async fn until(what: &str, timeout_s: f64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < timeout_s {
        if f() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}
