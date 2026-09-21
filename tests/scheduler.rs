//! Never leaving the box idle — and the two ways that goes wrong.
//!
//! The A1 renders at about a quarter of realtime, so it can never keep up with
//! anyone listening: every second the worker spends asleep is a second somebody
//! waits for later. Until now it spent a great many of them, because once the
//! playhead's lookahead and the prerender span were full there was no branch
//! left and the loop slept — with, on *Lord of Mysteries*, fourteen hundred
//! unrendered chapters behind it.
//!
//! Branch 6 fills that gap, and this suite is about the two things that make it
//! dangerous rather than the one that makes it useful:
//!
//! * it must **outrank nothing**. A speculative render that delayed the chunk
//!   under the playhead would trade a second of waiting now for a second of
//!   waiting later, which is not a trade.
//! * it must **stop well short of the gc**. `gc_audio` trims to 90 % of
//!   `MAX_AUDIO_GB` once it passes 100 %, oldest first — which is precisely the
//!   speculative work nobody has listened to yet. A renderer that ran to the cap
//!   would render it, watch it deleted, and render it again for as long as the
//!   process lived: a treadmill with a healthy-looking RTF and a busy log.

mod harness;

use std::time::{Duration, Instant};

use harness::Harness;
use narrator::cache;
use serde_json::json;

async fn until(what: &str, timeout_s: f64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < timeout_s {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn key_of(h: &Harness) -> String {
    h.state.session().key_or_x()
}

/// Fill a chapter's chunks with plausible wavs, as a finished render would.
fn seed_chapter(h: &Harness, key: &str, ci: usize, n: usize, samples: usize) {
    for i in 0..n {
        cache::write_wav(
            &cache::chunk_path(&h.work(), key, ci, i),
            &vec![0.0f32; samples],
        )
        .expect("seed");
    }
}

/// Bytes of wav in one chapter directory.
fn dir_bytes(d: &std::path::Path) -> u64 {
    std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                .sum()
        })
        .unwrap_or(0)
}

fn chapters_of(h: &Harness) -> Vec<usize> {
    h.state
        .session()
        .plan
        .iter()
        .map(|c| c.chunks.len())
        .collect()
}

#[tokio::test]
async fn a_full_buffer_is_not_a_reason_to_stop() {
    // The whole point. Seed the chapter under the playhead and the prerender
    // span, so every branch above the speculative one has nothing to do, and
    // watch a *later* chapter fill anyway.
    let h = Harness::with(|c| {
        c.lookahead = 4;
        c.prerender_chapters = 0;
    })
    .await;
    h.load().await;
    let key = key_of(&h);
    let ns = chapters_of(&h);
    assert!(ns.len() >= 2, "the fixture needs a second chapter");
    seed_chapter(&h, &key, 0, ns[0], 2400);

    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;

    let later = cache::chunk_path(&h.work(), &key, 1, 0);
    until(
        "a chapter past the prerender span to start rendering on its own",
        25.0,
        || later.exists(),
    )
    .await;
}

#[tokio::test]
async fn it_stops_before_the_collector_would_start() {
    // A cap small enough that the seeded cache is already past the 80 % ceiling.
    // Nothing speculative may be rendered while that is true — otherwise the
    // renderer and the gc take turns and the box burns its spare core forever.
    const CAP_MB: f64 = 1.0;
    let h = Harness::with(|c| {
        c.lookahead = 4;
        c.prerender_chapters = 0;
        c.max_audio_gb = CAP_MB / 1024.0;
    })
    .await;
    h.load().await;
    let key = key_of(&h);
    let ns = chapters_of(&h);
    // ~480 kB a chunk, so even the fixture's short first chapter clears 1 MB.
    seed_chapter(&h, &key, 0, ns[0], 240_000);

    // Asserted rather than assumed: a test that silently seeded *under* the
    // ceiling would pass for the wrong reason forever, which is exactly the
    // failure this one is here to catch.
    let seeded = narrator::chapters::total_bytes(&h.work())
        + dir_bytes(&cache::chapter_dir(&h.work(), &key, 0));
    assert!(
        seeded as f64 > CAP_MB * 1024.0 * 1024.0,
        "the seed must exceed the cap or this test proves nothing: {seeded} bytes"
    );

    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    // Long enough that the speculative branch would certainly have produced
    // something if it were going to.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let later = cache::chunk_path(&h.work(), &key, 1, 0);
    assert!(
        !later.exists(),
        "speculation must stand down above the ceiling, or it is a treadmill"
    );
}

#[tokio::test]
async fn the_playhead_still_outranks_it() {
    // Branch 6 sits below every other branch, and the loop re-reads the playhead
    // every iteration — so a chunk the reader is waiting on is rendered next,
    // not after the speculative work in flight.
    let h = Harness::with(|c| {
        c.lookahead = 2;
        c.prerender_chapters = 0;
    })
    .await;
    h.load().await;
    let key = key_of(&h);
    let ns = chapters_of(&h);
    seed_chapter(&h, &key, 0, ns[0], 2400);
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    // Let it get busy rendering ahead.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Now punch a hole under the playhead — the gc's own signature move — and
    // insist it is filled promptly rather than after the whole library.
    let target = ns[0] / 2;
    let p = cache::chunk_path(&h.work(), &key, 0, target);
    std::fs::remove_file(&p).expect("punch a hole");
    h.post_json("/api/playhead", json!({"chunk": target})).await;

    until("the chunk under the playhead to be refilled", 20.0, || {
        p.exists()
    })
    .await;
}

#[tokio::test]
async fn a_speculative_render_is_not_a_stall() {
    // `/healthz` turns a renderer that has stopped putting chunks on disk into a
    // 503 the watchdog restarts the container over. A worker that is rendering
    // ahead is the opposite of stalled, and it had better look that way.
    let h = Harness::with(|c| {
        c.lookahead = 4;
        c.prerender_chapters = 0;
    })
    .await;
    h.load().await;
    let key = key_of(&h);
    let ns = chapters_of(&h);
    seed_chapter(&h, &key, 0, ns[0], 2400);
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    until("something rendered ahead", 25.0, || {
        cache::chunk_path(&h.work(), &key, 1, 0).exists()
    })
    .await;

    let (code, _) = h.get("/healthz").await;
    assert_eq!(
        code,
        axum::http::StatusCode::OK,
        "rendering ahead must not read as a stalled renderer"
    );
}

#[tokio::test]
async fn nothing_is_rendered_for_a_library_that_is_finished() {
    // The branch has to have an end. With every chapter of the only book on
    // disk, the worker reports "ready" and sleeps rather than spinning through
    // the plan for holes that are not there.
    let h = Harness::with(|c| {
        c.lookahead = 4;
        c.prerender_chapters = 0;
    })
    .await;
    h.load().await;
    let key = key_of(&h);
    for (ci, n) in chapters_of(&h).into_iter().enumerate() {
        seed_chapter(&h, &key, ci, n, 2400);
    }
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    until("the worker to settle", 20.0, || {
        h.state.session().status == "ready"
    })
    .await;
    let before = h
        .state
        .render_attempts
        .load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        h.state
            .render_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        before,
        "a finished library is not a reason to keep trying"
    );
}

#[tokio::test]
async fn a_finished_book_is_not_the_end_of_the_library() {
    // The half Fernando asked for in so many words: keep rendering the next
    // chapters, prioritising the book most recently opened. With the loaded book
    // complete, the worker moves on to the *other* book rather than going to
    // sleep with a library of unrendered chapters behind it.
    //
    // This is also the one branch that renders a book the session is not
    // holding, which is why it reads its plan straight off `plan.json` instead
    // of through the parse cache: most of the library is not loaded, and an
    // index of audio already on disk has no business refusing to work because
    // an epub's mtime moved.
    let h = Harness::with(|c| {
        c.lookahead = 4;
        c.prerender_chapters = 0;
    })
    .await;

    // Opened first, so it is the *older* of the two and only reachable once the
    // newer one is finished.
    let other = h.add_book("Other (2026).epub");
    let (code, loaded) = h.post_json("/api/load", json!({"path": other})).await;
    assert_eq!(code, axum::http::StatusCode::OK, "{loaded}");
    let other_key = loaded["key"].as_str().unwrap_or_default().to_string();
    assert_eq!(other_key, "Other (2026)");

    // ...then the book actually being read, which is now the most recent.
    h.load().await;
    let key = key_of(&h);
    assert_ne!(key, other_key);
    for (ci, n) in chapters_of(&h).into_iter().enumerate() {
        seed_chapter(&h, &key, ci, n, 2400);
    }
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;

    let elsewhere = cache::chunk_path(&h.work(), &other_key, 0, 0);
    until("the other book to start rendering", 25.0, || {
        elsewhere.exists()
    })
    .await;
}

#[tokio::test]
async fn an_order_on_another_book_is_not_invisible() {
    // "Download these chapters", then open something else. The order was durable
    // the whole time — `state.db`'s `intent` table, and `queue.json` beside it —
    // but nothing read it until that book was loaded again, so 74 chapters could
    // sit waiting for an `/api/load` that might not come for days.
    //
    // Ranked below the loaded book's own queue and above the speculative branch:
    // an explicit ask beats a guess, and the book in front of the reader beats
    // one that is not.
    let h = Harness::with(|c| {
        c.lookahead = 4;
        c.prerender_chapters = 0;
    })
    .await;

    // Ask for a chapter of one book...
    let other = h.add_book("Ordered (2026).epub");
    let (code, loaded) = h.post_json("/api/load", json!({"path": other})).await;
    assert_eq!(code, axum::http::StatusCode::OK, "{loaded}");
    let other_key = loaded["key"].as_str().unwrap_or_default().to_string();
    let (code, _) = h
        .post_json(
            "/api/chapters/render",
            json!({"chapters": [1], "pack": false}),
        )
        .await;
    assert_eq!(code, axum::http::StatusCode::OK);

    // ...then go and read a different one, whose own work is all done.
    h.load().await;
    let key = key_of(&h);
    assert_ne!(key, other_key);
    for (ci, n) in chapters_of(&h).into_iter().enumerate() {
        seed_chapter(&h, &key, ci, n, 2400);
    }
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;

    let ordered = cache::chunk_path(&h.work(), &other_key, 1, 0);
    until("the order on the other book to be worked on", 25.0, || {
        ordered.exists()
    })
    .await;
}
