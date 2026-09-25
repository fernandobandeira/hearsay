//! A pack for a book the session is not holding, and everything it used to get
//! wrong about the book that *is* loaded.
//!
//! The packer's queue carries the book with each chapter (`ChapterRef`) so an
//! order placed from the library can be packed without opening the book. The
//! loaded book's views of it — `building()`, `loaded_pack_queue()` — and its own
//! bare-numbered bookkeeping — `build_want`, the gc's keep lists — must never
//! read a foreign job's chapter number as if it were the loaded book's.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use narrator::book::{Chapter, Chunk};
use narrator::cache;
use narrator::config::Config;
use narrator::state::{AppState, ChapterRef};

fn plan(n: usize) -> Vec<Chapter> {
    (0..n)
        .map(|i| Chapter {
            index: i,
            id: format!("c{i}"),
            title: format!("C{i}"),
            chunks: vec![Chunk {
                text: "hello.".into(),
                para: 0,
                silent: false,
            }],
        })
        .collect()
}

fn state(f: impl FnOnce(&mut Config)) -> (tempfile::TempDir, Arc<AppState>) {
    let d = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::for_test(d.path());
    cfg.vault = None;
    cfg.positions_dir = d.path().join("work");
    f(&mut cfg);
    std::fs::create_dir_all(&cfg.work).expect("work");
    let st = AppState::new(cfg);
    (d, st)
}

/// Load book A — a ten-chapter plan in memory — with nothing queued.
fn load_a(st: &AppState) {
    let mut s = st.session();
    s.book = Some("/books/A.epub".into());
    s.plan = Arc::new(plan(10));
    s.chapter = 0;
}

/// Book B on disk only, as the library leaves a book nobody has open: its
/// `plan.json` and every chunk of `ci` rendered.
fn seed_b(st: &AppState, ci: usize) {
    let p = cache::plan_path(&st.cfg.work, "B");
    std::fs::create_dir_all(p.parent().expect("dir")).expect("dir");
    std::fs::write(&p, serde_json::to_vec(&plan(10)).expect("json")).expect("plan");
    cache::write_wav(
        &cache::chunk_path(&st.cfg.work, "B", ci, 0),
        &vec![0.1f32; 2400],
    )
    .expect("wav");
}

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

fn foreign_done(st: &AppState) -> bool {
    let s = st.session();
    s.pack_queue.is_empty() && s.packing.is_none()
}

#[test]
fn a_foreign_job_is_not_the_loaded_books_chapter_of_the_same_number() {
    let (_d, st) = state(|_| {});
    load_a(&st);
    let mut s = st.session();
    s.packing = Some(ChapterRef::new("B", 7));
    s.pack_queue.push(ChapterRef::new("B", 8));
    s.pack_queue.push(ChapterRef::new("A", 3));
    assert_eq!(s.building(), None, "B's chapter 7 is not A's");
    assert_eq!(s.loaded_pack_queue(), vec![3]);
    s.packing = Some(ChapterRef::new("A", 3));
    assert_eq!(s.building(), Some(3));
    assert_eq!(ChapterRef::new("A", 3).to_string(), "A/ch003");
}

#[test]
fn the_gc_spares_a_foreign_pack_and_a_foreign_order() {
    let (_d, st) = state(|_| {});
    load_a(&st);
    {
        let mut s = st.session();
        s.packing = Some(ChapterRef::new("B", 7));
        s.pack_queue.push(ChapterRef::new("B", 8));
    }
    // A standing order on a third book, placed from the library.
    st.store()
        .expect("store")
        .add_intent("C", &[2], "dev", true, 1)
        .expect("intent");

    let keep = st.gc_keep();
    let dir = |k: &str, c: usize| cache::chapter_dir(&st.cfg.work, k, c);
    assert!(keep.contains(&dir("B", 7)), "the job in the packer's hands");
    assert!(keep.contains(&dir("B", 8)), "a job waiting for the packer");
    assert!(
        keep.contains(&dir("C", 2)),
        "an order being rendered elsewhere"
    );
    assert!(
        !keep.contains(&dir("A", 7)),
        "and not the loaded book's chapter of the same number"
    );

    // With nothing loaded at all, the foreign jobs are still the packer's input.
    st.session().book = None;
    let keep = st.gc_keep();
    assert!(keep.contains(&dir("B", 7)) && keep.contains(&dir("B", 8)));
}

#[tokio::test]
async fn a_foreign_pack_leaves_the_loaded_books_download_alone() {
    let (_d, st) = state(|_| {});
    load_a(&st);
    // Chapter 7 of the loaded book is ordered for download and still rendering.
    {
        let mut s = st.session();
        s.queue = vec![7];
        s.build_want.insert(7);
    }
    // ...and chapter 7 of a book nobody has open is ready to pack.
    seed_b(&st, 7);
    narrator::render::enqueue_build_elsewhere(&st, "B", 7);
    until("the foreign job to finish", 20.0, || foreign_done(&st)).await;
    // With or without ffmpeg the job is over; either way it was B's.
    let s = st.session();
    assert!(
        s.build_want.contains(&7),
        "a foreign chapter 7 took the loaded book's chapter 7 out of build_want"
    );
    assert_eq!(s.queue, vec![7]);
    drop(s);
    st.stop.store(true, Ordering::SeqCst);
}

#[tokio::test]
async fn a_foreign_pack_waits_for_a_stalled_renderer_too() {
    let (_d, st) = state(|_| {});
    load_a(&st);
    seed_b(&st, 3);
    // Somebody is waiting on the chunk under the playhead right now.
    st.set_stalled(true);
    narrator::render::enqueue_build_elsewhere(&st, "B", 3);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        st.session().pack_queue,
        vec![ChapterRef::new("B", 3)],
        "the packer took a foreign encode while the renderer was behind"
    );
    // And it goes the moment the renderer catches up.
    st.set_stalled(false);
    until("the held job to be taken", 20.0, || foreign_done(&st)).await;
    st.stop.store(true, Ordering::SeqCst);
}

#[tokio::test]
async fn the_chapter_gc_after_a_foreign_pack_spares_the_one_being_read() {
    // A cap far below one file, so the packed-chapter gc wants to evict all of
    // them and only its keep list stands in the way.
    let (_d, st) = state(|c| c.max_chapter_gb = 1e-9);
    load_a(&st);
    let (reading, _) = narrator::chapters::chapter_files(&st.cfg.work, "A", 0);
    std::fs::create_dir_all(reading.parent().expect("dir")).expect("dir");
    std::fs::write(&reading, vec![0u8; 4096]).expect("m4a");

    seed_b(&st, 0);
    narrator::render::enqueue_build_elsewhere(&st, "B", 0);
    until("the foreign job to finish", 20.0, || foreign_done(&st)).await;
    // The gc runs after the bookkeeping; give it the moment it needs.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        reading.exists(),
        "the chapter being read was evicted — the keep list named B/ch000, not A/ch000"
    );
    st.stop.store(true, Ordering::SeqCst);
}

#[test]
fn a_chunk_of_another_book_does_not_clear_this_books_count() {
    let (_d, st) = state(|_| {});
    load_a(&st);
    st.session().queue = vec![4];
    narrator::wishlist::save(&st);
    // What a restart does: pick the order back up and count it.
    st.session().queue.clear();
    narrator::wishlist::resume(&st);
    assert_eq!(st.wishlist().attempts(4), 1);

    // Chapter 4 of some other book lands. Nothing about A's chapter 4 changed.
    narrator::wishlist::progress(&st, "B", 4);
    assert_eq!(
        st.wishlist().attempts(4),
        1,
        "another book's chunk reset the loaded book's poison counter"
    );
    // A's own chunk does.
    narrator::wishlist::progress(&st, "A", 4);
    assert_eq!(st.wishlist().attempts(4), 0);
}
