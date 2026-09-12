//! The work queue survives the process.
//!
//! One tap in the chapter drawer can ask for 74 chapters, which on the A1 is an
//! overnight job, and this box restarts on a deploy, on the watchdog's third
//! failed probe, on a `docker pull`. Until now every one of those threw the list
//! away in silence — the worker came back with nothing to do and the drawer came
//! back looking exactly as it had, because an abandoned queue and a queue nobody
//! ever asked for are the same picture.
//!
//! What is asserted here is the whole contract of `src/wishlist.rs`: the list
//! comes back and is worked without a client saying anything, a torn file costs
//! the list and nothing else, a cancel cannot be undone by a restart, a chapter
//! that can never render is eventually parked instead of spinning forever behind
//! a watchdog, a file that names another book is never worked on, and resumed
//! work stands down for a voice memo exactly like work that was asked for a
//! moment ago.

mod harness;

use std::path::Path;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use harness::Harness;
use narrator::cache;
use serde_json::{json, Value};

/// Wait for a predicate, or fail saying what was being waited for.
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

fn queue_file(h: &Harness) -> std::path::PathBuf {
    narrator::wishlist::path(&h.work(), &key_of(h))
}

/// `/api/chapters`, and the row for one chapter.
async fn rows(h: &Harness) -> Value {
    let (code, body) = h.get_json("/api/chapters").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    body
}

fn row(body: &Value, ci: usize) -> Value {
    body["chapters"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["i"] == json!(ci)))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The whole point, end to end: ask for chapters, lose the process, and get them
/// anyway — with no client having said a word after the restart.
#[tokio::test]
async fn a_queued_download_survives_a_restart_and_finishes_itself() {
    let mut h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);

    let (code, body) = h
        .post_json(
            "/api/chapters/render",
            json!({"chapters": [2, 1], "pack": false}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["queue"], json!([2, 1]), "{body}");
    assert!(queue_file(&h).exists(), "the ask is on disk before the 200");

    h.restart().await;

    // The drawer tells the truth the moment the server is answering again: these
    // chapters are still coming, in the order they were asked for.
    let body = rows(&h).await;
    assert_eq!(body["queue"], json!([2, 1]), "{body}");
    assert_eq!(row(&body, 2)["queued"], json!(true), "{body}");

    // And they are worked on, with nothing having opened, played or asked.
    let first = cache::chunk_path(&h.work(), &key, 2, 0);
    until("the queued chapter to render itself", 30.0, || {
        first.exists()
    })
    .await;
    let n = h.state.session().plan[2].chunks.len();
    let last = cache::chunk_path(&h.work(), &key, 2, n - 1);
    until("the chapter to finish", 30.0, || last.exists()).await;
    // Finished work leaves the file, or the next restart would re-do it.
    until("chapter 2 to leave the queue", 30.0, || {
        !h.state.session().queue.contains(&2)
    })
    .await;
}

/// `pack: true` is the "download" verb, and the flag has to survive with the
/// chapter — otherwise a resumed download renders the audio and stops one step
/// short of the m4a that was the entire reason for asking.
///
/// And the chaining is the assertion that matters: the worker hands a chapter to
/// the packer the moment its last chunk lands, because `build_want` says to.
/// That is existing behaviour; what is new is that it still happens after the
/// process that took the request is gone, with no client asking a second time.
#[tokio::test]
async fn a_download_renders_then_packs_itself_after_a_restart() {
    let mut h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let (code, body) = h
        .post_json(
            "/api/chapters/render",
            json!({"chapters": [3], "pack": true}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    h.restart().await;
    assert!(
        h.state.session().build_want.contains(&3),
        "the chapter came back without the reason it was asked for"
    );
    let body = rows(&h).await;
    assert_eq!(row(&body, 3)["pack_queued"], json!(true), "{body}");

    let n = h.state.session().plan[3].chunks.len();
    let last = cache::chunk_path(&h.work(), &key, 3, n - 1);
    until("the chapter to render", 30.0, || last.exists()).await;
    if which("ffmpeg").is_none() {
        eprintln!("skipping the pack: no ffmpeg");
        return;
    }
    let (m4a, _) = narrator::chapters::chapter_files(&h.work(), &key, 3);
    until("the chapter to pack itself", 60.0, || m4a.exists()).await;
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// A restart must not resurrect work that was cancelled.
///
/// The mirror image of the bug being fixed, and the more expensive one: the
/// queue file is what the next boot believes, so a cancel that only happened in
/// memory would put 74 chapters back on a box that was told to stop.
#[tokio::test]
async fn a_cancel_outlives_the_process_that_took_it() {
    let mut h = Harness::new().await;
    h.load().await;
    h.post_json("/api/chapters/render", json!({"chapters": [1, 2, 3]}))
        .await;
    let (code, body) = h
        .post_json("/api/chapters/cancel", json!({"chapters": [2]}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    h.restart().await;
    assert_eq!(
        h.state.session().queue,
        vec![1, 3],
        "{:?}",
        h.state.session()
    );

    // And the empty body, which clears everything.
    let (code, _) = h.post_json("/api/chapters/cancel", json!({})).await;
    assert_eq!(code, StatusCode::OK);
    h.restart().await;
    assert!(
        h.state.session().queue.is_empty(),
        "a cleared queue came back"
    );
    let body = rows(&h).await;
    assert_eq!(body["queue"], json!([]), "{body}");
}

/// A file that will not parse costs the list and nothing else.
///
/// Half-written by a disk that filled, truncated by a kill that beat the rename
/// (which the rename is there to make impossible, but the file is on a
/// filesystem this code does not own), hand-edited at three in the morning: none
/// of those may keep the server from coming up on its book.
#[tokio::test]
async fn a_torn_queue_file_costs_the_queue_and_nothing_else() {
    let mut h = Harness::new().await;
    h.load().await;
    h.post_json("/api/chapters/render", json!({"chapters": [1, 2]}))
        .await;
    let p = queue_file(&h);
    let whole = std::fs::read(&p).expect("the queue file");
    // Exactly what a torn write would leave: the front of a good file.
    std::fs::write(&p, &whole[..whole.len() / 2]).expect("truncate");

    h.restart().await;

    let (code, st) = h.get_json("/api/status").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(st["book"], json!("Fixture (2026).epub"), "{st}");
    assert!(
        h.state.session().queue.is_empty(),
        "an unreadable queue is no queue"
    );
    // And the next ask writes a good file over the bad one.
    let (code, _) = h
        .post_json("/api/chapters/render", json!({"chapters": [3]}))
        .await;
    assert_eq!(code, StatusCode::OK);
    h.restart().await;
    assert_eq!(h.state.session().queue, vec![3]);
}

/// A queue file that belongs to another book is never worked on.
///
/// Two ways it can happen: the file sits in another book's cache directory (the
/// normal case — the box was asked for chapters of one book and is now holding
/// another), or a work directory copied from somewhere else has a file in the
/// right place whose contents are about something else entirely.
#[tokio::test]
async fn a_queue_file_for_another_book_is_never_worked_on() {
    let mut h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);

    // Another book's list, in that book's own directory.
    let elsewhere = narrator::wishlist::path(&h.work(), "Some Other Book (2011)");
    std::fs::create_dir_all(elsewhere.parent().expect("parent")).expect("dir");
    std::fs::write(
        &elsewhere,
        json!({"version": 1, "book": "/books/Some Other Book (2011).epub",
               "key": "Some Other Book (2011)", "updated": "2026-09-11T22:00:00+02:00",
               "items": [{"chapter": 1, "pack": true}]})
        .to_string(),
    )
    .expect("write");
    // ... and a list in the loaded book's directory that says it is not this
    // book's, which is the only shape a copied work directory can take.
    std::fs::write(
        narrator::wishlist::path(&h.work(), &key),
        json!({"version": 1, "book": "/books/Some Other Book (2011).epub",
               "key": "Some Other Book (2011)", "updated": "2026-09-11T22:00:00+02:00",
               "items": [{"chapter": 2, "pack": true}]})
        .to_string(),
    )
    .expect("write");

    h.restart().await;
    assert!(
        h.state.session().queue.is_empty(),
        "work was taken from a book this session is not holding"
    );
    assert!(h.state.session().build_want.is_empty());
    // Nothing of the other book's was touched either — it is that book's to keep.
    assert!(elsewhere.exists());
}

/// A chapter that can never render is parked rather than retried forever.
///
/// The failure this prevents needs two ingredients the box actually has: a queue
/// that survives restarts, and a watchdog that restarts the container after
/// three failed health probes. A chapter that will not render makes the renderer
/// look stalled, the watchdog restarts it, the queue comes back, and the box
/// spends the week on one chapter. The in-process backoff bounds the spin;
/// nothing bounded the loop across processes, because the attempt count died
/// with the process.
#[cfg(unix)]
#[tokio::test]
async fn a_chapter_that_never_renders_is_parked_and_can_be_asked_for_again() {
    use std::os::unix::fs::PermissionsExt;

    let ro = |p: &Path, yes: bool| {
        std::fs::set_permissions(
            p,
            std::fs::Permissions::from_mode(if yes { 0o555 } else { 0o755 }),
        )
    };

    let mut h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    // Every write into chapter 5 fails: a read-only mount, or a disk that is
    // full, seen from inside `render_one`.
    let dir = cache::chapter_dir(&h.work(), &key, 3);
    std::fs::create_dir_all(&dir).expect("dir");
    ro(&dir, true).expect("chmod");
    let probe = dir.join(".probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        let _ = ro(&dir, false);
        eprintln!("skipping: the directory is writable anyway (root?)");
        return;
    }

    h.post_json("/api/chapters/render", json!({"chapters": [3]}))
        .await;
    for _ in 0..narrator::wishlist::MAX_ATTEMPTS {
        h.restart().await;
    }

    // Out of attempts: not queued, not forgotten, and the drawer says so.
    assert!(
        h.state.session().queue.is_empty(),
        "a parked chapter must not be handed back to the worker"
    );
    let body = rows(&h).await;
    assert_eq!(row(&body, 3)["parked"], json!(true), "{body}");
    assert_eq!(row(&body, 3)["queued"], json!(false), "{body}");
    // Every other row is unchanged — parking is per item, not a stop switch.
    assert_eq!(row(&body, 2)["parked"], Value::Null, "{body}");

    // Asking again is the retry, and it needs no endpoint of its own.
    let _ = ro(&dir, false);
    let (code, body) = h
        .post_json("/api/chapters/render", json!({"chapters": [3]}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["queue"], json!([3]), "{body}");
    let body = rows(&h).await;
    assert_eq!(row(&body, 3)["parked"], Value::Null, "{body}");
    // ... and it renders now that the cause is gone.
    let first = cache::chunk_path(&h.work(), &key, 3, 0);
    until("the un-parked chapter", 30.0, || first.exists()).await;
}

/// Progress is what clears the poison count, not time.
///
/// A download that lives through six deploys is not a failing job and must not
/// be parked as one: the count is "restarts since this chapter last produced a
/// chunk", so the first chunk that lands puts it back to zero. Driven here by
/// taking the cause of the failure away half way through — three restarts with
/// the chapter unrenderable, then one with it renderable, and the count that was
/// one short of parking is gone.
#[cfg(unix)]
#[tokio::test]
async fn a_chunk_that_lands_clears_the_count_before_it_can_park() {
    use std::os::unix::fs::PermissionsExt;

    let ro = |p: &Path, yes: bool| {
        std::fs::set_permissions(
            p,
            std::fs::Permissions::from_mode(if yes { 0o555 } else { 0o755 }),
        )
    };

    let mut h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let dir = cache::chapter_dir(&h.work(), &key, 2);
    std::fs::create_dir_all(&dir).expect("dir");
    ro(&dir, true).expect("chmod");
    let probe = dir.join(".probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        let _ = ro(&dir, false);
        eprintln!("skipping: the directory is writable anyway (root?)");
        return;
    }

    h.post_json("/api/chapters/render", json!({"chapters": [2]}))
        .await;
    for n in 1..narrator::wishlist::MAX_ATTEMPTS - 1 {
        h.restart().await;
        assert_eq!(h.state.wishlist().attempts(2), n, "restart {n}");
    }

    // Whatever was wrong with the box is fixed, and the chapter renders.
    let _ = ro(&dir, false);
    h.restart().await;
    let first = cache::chunk_path(&h.work(), &key, 2, 0);
    until("the chapter once it can be written", 30.0, || {
        first.exists()
    })
    .await;
    until("the count to clear", 10.0, || {
        h.state.wishlist().attempts(2) == 0
    })
    .await;

    // ... and the chapter finishes and leaves the list, so no number of
    // restarts after that can park it.
    until("chapter 2 to leave the queue", 30.0, || {
        !h.state.session().queue.contains(&2)
    })
    .await;
    for _ in 0..(narrator::wishlist::MAX_ATTEMPTS + 1) {
        h.restart().await;
    }
    assert!(!h.state.wishlist().is_parked(2));
    let body = rows(&h).await;
    assert_eq!(row(&body, 2)["parked"], Value::Null, "{body}");
}

/// Resumed work is ordinary work, and stands down for a voice memo like any
/// other.
///
/// A memo exists in exactly one place — IndexedDB on the phone that recorded it
/// — until `/api/note` answers, and the startup sweep may be finishing one the
/// last process was in the middle of. A queue that came back with the process
/// and went straight for both of the box's two cores would be competing with
/// precisely that.
#[tokio::test]
async fn resumed_work_stands_down_for_a_voice_memo() {
    let mut h = Harness::with(|c| c.queue_resume_delay_s = 1.0).await;
    h.load().await;
    let key = key_of(&h);
    h.post_json("/api/chapters/render", json!({"chapters": [1]}))
        .await;

    h.restart().await;
    // Whatever the first process rendered before it was killed goes with it, so
    // the only thing that can put a chunk here now is the resumed queue.
    for ci in [0usize, 1] {
        let _ = std::fs::remove_dir_all(cache::chapter_dir(&h.work(), &key, ci));
    }
    // Taken before the resume delay is up, the way the memo sweep takes it: the
    // sweep is spawned while the router is being built, and the queue is
    // deliberately later.
    let busy = h.state.whisper.gate().enter();
    let first = cache::chunk_path(&h.work(), &key, 0, 0);
    let queued = cache::chunk_path(&h.work(), &key, 1, 0);
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(
        !first.exists() && !queued.exists(),
        "a resumed queue must not take a core from a transcription"
    );

    drop(busy);
    until("the resumed queue once the memo is filed", 30.0, || {
        queued.exists()
    })
    .await;
}

/// The reader opening the book it was already on must not throw the queue away.
///
/// `/api/load` clears the queues, correctly — they are indices into the plan it
/// is replacing. But the reader re-loads the book it was on when the app starts,
/// so without the wishlist coming back with it, a download resumed by a restart
/// would be wiped thirty seconds later by a phone coming out of a pocket.
#[tokio::test]
async fn re_opening_the_book_keeps_the_queue() {
    let mut h = Harness::new().await;
    h.load().await;
    h.post_json(
        "/api/chapters/render",
        json!({"chapters": [2, 3], "pack": true}),
    )
    .await;
    h.restart().await;
    assert_eq!(h.state.session().queue, vec![2, 3]);

    // What the reader does on every app start.
    h.load().await;
    assert_eq!(
        h.state.session().queue,
        vec![2, 3],
        "the load wiped the list it was supposed to pick up"
    );
    assert!(h.state.session().build_want.contains(&2));
    // Twice, because a reconnecting reader can do it twice.
    h.load().await;
    assert_eq!(h.state.session().queue, vec![2, 3], "the list doubled");
}

/// A book with nothing owed writes nothing, and a first boot has nothing to
/// read. The empty case has to be free: most books never have a queue at all.
#[tokio::test]
async fn a_book_nobody_queued_anything_for_has_no_file() {
    let mut h = Harness::new().await;
    h.load().await;
    assert!(!queue_file(&h).exists());
    h.restart().await;
    assert!(h.state.session().queue.is_empty());
    let (code, body) = h.get_json("/api/chapters").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["queue"], json!([]), "{body}");
}
