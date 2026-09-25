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
//!
//! The second half of the file is the same contract against the second record.
//! The order now also lives in `state.db`'s `intent` table, because the file per
//! book cannot answer "what does this box owe, across the library, in the order
//! it was promised" — so what is asserted there is that the file the running box
//! already has is adopted rather than thrown away and then left where it is,
//! that an order outlives a restart with no file to read, that a book the
//! session is not holding is still visible and still in the order it was asked,
//! and that with no database at all every one of the promises above holds
//! unchanged.

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
///
/// Without a store, where the file is the whole record. With one, the file is a
/// copy and tearing it costs nothing at all — see
/// `a_damaged_copy_costs_nothing_while_the_table_has_the_order`.
#[tokio::test]
async fn a_torn_queue_file_costs_the_queue_and_nothing_else() {
    let mut h = Harness::with(|c| {
        std::fs::create_dir_all(c.work.join("state.db")).expect("block state.db");
    })
    .await;
    assert!(
        h.state.store().is_none(),
        "this test is about the file alone"
    );
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

// ---------------------------------------------------------------- the store

/// A second book in the library: the same words under a different name, which
/// is a different cache key and therefore a different standing order.
fn second_book(h: &Harness) -> String {
    let p = h.state.cfg.books[0].join("Second Fixture (2026).epub");
    std::fs::copy(harness::fixture_epub(), &p).expect("copy fixture epub");
    p.to_string_lossy().to_string()
}

/// Place a standing order without waking the renderer.
///
/// `/api/chapters/render` does exactly this and then starts the worker, which
/// with the fake engine finishes a fixture chapter well inside the next line of
/// the test — and a finished chapter leaves the list, which is right and is the
/// wrong thing to be racing. The tests above are about the endpoint; these are
/// about what the two records hold when nothing is moving.
fn order(h: &Harness, chapters: &[usize], pack: bool) {
    {
        let mut s = h.state.session();
        for c in chapters {
            if !s.queue.contains(c) {
                s.queue.push(*c);
            }
            if pack {
                s.build_want.insert(*c);
            }
        }
    }
    narrator::wishlist::asked(&h.state, chapters);
}

/// The book's standing order as the table holds it.
fn intents(h: &Harness, key: &str) -> Vec<narrator::store::IntentRow> {
    h.state
        .store()
        .expect("state.db")
        .intents(key)
        .expect("intents")
}

/// The file the running box already has is picked up, not thrown away.
///
/// This is the only migration there is: the A1 is holding orders in
/// `queue.json` right now and has never had a row in `intent`. The first boot of
/// a binary with the table has to read the file, keep the order it was asked in,
/// keep the pack flags that say "download" rather than "render", and keep the
/// attempt counts — dropping those last would hand a chapter that has already
/// wedged the box four times a fresh five tries.
#[tokio::test]
async fn an_existing_queue_file_is_adopted_into_the_store() {
    // Nothing may render: a chunk landing clears the very counts under test, and
    // what is being asserted is the state of both records the instant boot ends.
    let mut h = Harness::with(|c| c.queue_resume_delay_s = 600.0).await;
    h.load().await;
    let key = key_of(&h);
    assert!(
        intents(&h, &key).is_empty(),
        "nothing has been asked for yet"
    );

    // Exactly what the previous binary leaves behind, counts and all.
    let p = queue_file(&h);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
    std::fs::write(
        &p,
        json!({"version": 1, "book": h.book_path(), "key": key,
               "updated": "2026-09-20T22:00:00+02:00",
               "items": [{"chapter": 3, "pack": true, "attempts": 2},
                         {"chapter": 1, "pack": false}]})
        .to_string(),
    )
    .expect("write");

    h.restart().await;

    let rows = intents(&h, &key);
    assert_eq!(
        rows.iter().map(|r| (r.chapter, r.pack)).collect::<Vec<_>>(),
        vec![(3, true), (1, false)],
        "the order asked and the reason for asking"
    );
    assert!(
        rows[0].seq < rows[1].seq,
        "`seq` is the order, not the index"
    );
    // Two attempts came out of the file and this boot is the third; a count that
    // started again at zero here is the whole poison bound undone.
    assert_eq!(rows.iter().map(|r| r.tries).collect::<Vec<_>>(), vec![3, 1]);
    assert_eq!(h.state.wishlist().attempts(3), 3);
    assert_eq!(h.state.session().queue, vec![3, 1]);
    assert!(h.state.session().build_want.contains(&3));
}

/// ... and the file it was adopted from is still sitting there.
///
/// Deliberately not deleted, and not an oversight: `state.db` is new and the
/// binary that predates it is one `docker pull` away, reading only this file. A
/// rollback that cost an overnight download would be a worse failure than
/// anything the table was added to fix, so the file keeps being written and the
/// way back keeps working.
#[tokio::test]
async fn the_adopted_file_is_left_behind_as_the_way_back() {
    let mut h = Harness::with(|c| c.queue_resume_delay_s = 600.0).await;
    h.load().await;
    let key = key_of(&h);
    order(&h, &[2, 1], true);
    h.restart().await;

    // The table has it...
    assert_eq!(
        intents(&h, &key)
            .iter()
            .map(|r| r.chapter)
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    // ... and so does the file, with the flag that makes it a download.
    let raw = std::fs::read(queue_file(&h)).expect("queue.json survived");
    let doc: Value = serde_json::from_slice(&raw).expect("parse");
    assert_eq!(doc["key"], json!(key), "{doc}");
    assert_eq!(
        doc["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| (i["chapter"].clone(), i["pack"].clone()))
            .collect::<Vec<_>>(),
        vec![(json!(2), json!(true)), (json!(1), json!(true))],
        "{doc}"
    );
}

/// The order comes back with no file to read it from.
///
/// The file is the way back, not the way forward. Taking it away leaves the
/// table as the only record of the night's work, which is what a box that has
/// been running the new binary for a week actually looks like once something has
/// tidied `work/` — and the queue has to survive that.
#[tokio::test]
async fn an_order_survives_a_restart_through_the_store_alone() {
    let mut h = Harness::with(|c| c.queue_resume_delay_s = 600.0).await;
    h.load().await;
    order(&h, &[3, 1], true);
    std::fs::remove_file(queue_file(&h)).expect("remove the file");

    h.restart().await;

    assert_eq!(h.state.session().queue, vec![3, 1], "the order was lost");
    assert!(
        h.state.session().build_want.contains(&3),
        "the chapter came back without the reason it was asked for"
    );
    // And the boot writes the file back out, so the way back is never missing
    // for longer than one restart.
    assert!(queue_file(&h).exists());
}

/// A standing order on a book nobody is holding is still owed.
///
/// This is the question a file per book cannot answer and the reason the table
/// exists: the scheduler has to see everything the box owes at once, and a
/// download on the novel Fernando was reading last week is invisible to anything
/// that only looks at the session.
#[tokio::test]
async fn all_outstanding_sees_a_book_the_session_is_not_holding() {
    let h = Harness::with(|c| c.queue_resume_delay_s = 600.0).await;
    h.load().await;
    let first = key_of(&h);
    order(&h, &[2], true);

    // Another book takes the session. The first one's order is untouched by
    // that — a `/api/load` is somebody changing what they are reading, not
    // cancelling a download.
    let (code, body) = h
        .post_json("/api/load", json!({"path": second_book(&h)}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let second = key_of(&h);
    assert_ne!(first, second);
    order(&h, &[1], false);

    let all = narrator::wishlist::all_outstanding(&h.state);
    assert_eq!(
        all.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec![first, second]
    );
    assert_eq!(
        all[0]
            .1
            .iter()
            .map(|i| (i.chapter, i.pack))
            .collect::<Vec<_>>(),
        vec![(2, true)],
        "the book the session let go of"
    );
    assert_eq!(
        all[1].1.iter().map(|i| i.chapter).collect::<Vec<_>>(),
        vec![1]
    );
}

/// Across two books, the order is the order asked.
///
/// `seq` is global and monotonic, so "what was promised first" has an answer
/// that spans the library rather than one that restarts at each book. Coming
/// back to a book adds to the end of its list rather than moving it to the
/// front, which is the same rule inside one book and across all of them.
#[tokio::test]
async fn the_order_across_two_books_is_the_order_asked() {
    let h = Harness::with(|c| c.queue_resume_delay_s = 600.0).await;
    h.load().await;
    let first = key_of(&h);
    order(&h, &[3], true);

    let (code, body) = h
        .post_json("/api/load", json!({"path": second_book(&h)}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let second = key_of(&h);
    order(&h, &[2, 1], true);

    // Back to the first book, and one more chapter of it.
    h.load().await;
    assert_eq!(key_of(&h), first);
    assert_eq!(
        h.state.session().queue,
        vec![3],
        "re-opening the book lost its order"
    );
    order(&h, &[0], true);

    let all = narrator::wishlist::all_outstanding(&h.state);
    assert_eq!(
        all.iter()
            .map(|(k, items)| (
                k.clone(),
                items.iter().map(|i| i.chapter).collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        vec![(first, vec![3, 0]), (second, vec![2, 1])],
        "a book asked for first comes first, and a second ask goes to the back"
    );
}

/// With no database at all, every promise this module makes still holds.
///
/// `state.db` is optional on purpose — a work directory gone read-only or a file
/// that is not a database must never be a server that will not start — and the
/// fallback is not a reduced version of the queue, it is the file-per-book
/// arrangement that shipped before the table existed, unchanged. Simulated the
/// least invasive way there is: a *directory* where the database file goes, so
/// `Store::open` fails exactly as it would on a disk nobody can write.
#[tokio::test]
async fn with_no_database_the_file_is_still_the_whole_contract() {
    let mut h = Harness::with(|c| {
        std::fs::create_dir_all(c.work.join("state.db")).expect("block state.db");
        c.queue_resume_delay_s = 600.0;
    })
    .await;
    assert!(h.state.store().is_none(), "this test has no subject");
    h.load().await;
    order(&h, &[3, 1], true);
    assert!(queue_file(&h).exists(), "the file is the only record left");

    h.restart().await;
    assert!(h.state.store().is_none());
    assert_eq!(h.state.session().queue, vec![3, 1]);
    assert!(h.state.session().build_want.contains(&3));

    // `all_outstanding` answers for the book in front of it and says nothing
    // about any other — not a degraded answer, the only answer a file per book
    // can give, because nothing has read the others and nothing knows to look.
    let all = narrator::wishlist::all_outstanding(&h.state);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, key_of(&h));
    assert_eq!(
        all[0].1.iter().map(|i| i.chapter).collect::<Vec<_>>(),
        vec![3, 1]
    );

    // And a cancel still outlives the process that took it.
    let (code, _) = h
        .post_json("/api/chapters/cancel", json!({"chapters": [3]}))
        .await;
    assert_eq!(code, StatusCode::OK);
    h.restart().await;
    assert_eq!(h.state.session().queue, vec![1]);
    assert!(narrator::wishlist::all_outstanding(&h.state)
        .iter()
        .all(|(_, items)| !items.iter().any(|i| i.chapter == 3)));
}

/// A damaged copy costs nothing while the table holds the order.
///
/// The half of `a_torn_queue_file_costs_the_queue_and_nothing_else` that is
/// about the store. The table is the record and the file is written from it, so
/// a torn file is a torn *copy*: the order comes back from the table, and the
/// boot writes a whole copy over the torn one. (This used to be the other way
/// round — the file was read first and a damaged one emptied the table — which
/// is the arrangement that let a stale file overrule orders placed and
/// cancelled from the library.)
#[tokio::test]
async fn a_damaged_copy_costs_nothing_while_the_table_has_the_order() {
    let mut h = Harness::with(|c| c.queue_resume_delay_s = 600.0).await;
    h.load().await;
    let key = key_of(&h);
    order(&h, &[2, 1], true);
    assert_eq!(intents(&h, &key).len(), 2);

    let p = queue_file(&h);
    let whole = std::fs::read(&p).expect("the queue file");
    std::fs::write(&p, &whole[..whole.len() / 2]).expect("truncate");
    h.restart().await;

    assert_eq!(
        h.state.session().queue,
        vec![2, 1],
        "the table had the order the whole time"
    );
    assert_eq!(intents(&h, &key).len(), 2);
    let raw = std::fs::read(&p).expect("queue.json");
    let doc: Value = serde_json::from_slice(&raw).expect("the boot rewrote a whole copy");
    assert_eq!(doc["projection"], json!(true), "{doc}");
    assert_eq!(doc["items"].as_array().map(|a| a.len()), Some(2), "{doc}");
}
