//! `state.db` is the record of a standing order; `queue.json` is its copy.
//!
//! The file used to be read first for the loaded book, and `sync` then made the
//! table agree with it. But three paths change an order in the table alone —
//! an order placed or cancelled from the library for a book that is not loaded,
//! and the worker or packer retiring a foreign order — so the file they never
//! touched overruled them the moment that book was opened: a library order
//! vanished, a library cancel was undone.

use std::sync::Arc;

use narrator::book::{Chapter, Chunk};
use narrator::config::Config;
use narrator::state::AppState;

mod harness;

fn plan(n: usize) -> Arc<Vec<Chapter>> {
    Arc::new(
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
            .collect(),
    )
}

/// What `/api/load` does to the session before it adopts: a new book, empty
/// queues.
fn load(st: &AppState, book: &str) {
    let mut s = st.session();
    s.book = Some(book.into());
    s.plan = plan(10);
    s.queue.clear();
    s.build_want.clear();
    s.pack_queue.clear();
}

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    let d = tempfile::tempdir().expect("tempdir");
    let st = AppState::new(Config::for_test(d.path()));
    assert!(
        st.store().is_some(),
        "these are about the store being there"
    );
    (d, st)
}

fn file(st: &AppState, key: &str) -> serde_json::Value {
    let raw = std::fs::read(narrator::wishlist::path(&st.cfg.work, key)).expect("queue.json");
    serde_json::from_slice(&raw).expect("json")
}

fn chapters_in_file(st: &AppState, key: &str) -> Vec<u64> {
    file(st, key)["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["chapter"].as_u64()).collect())
        .unwrap_or_default()
}

#[test]
fn an_order_placed_from_the_library_survives_opening_the_book() {
    let (_d, st) = state();
    load(&st, "/books/A.epub");
    // An autopack, a finished queue — anything that leaves `queue.json` = [].
    narrator::wishlist::save(&st);
    load(&st, "/books/B.epub");
    // `/api/chapters/render {"book": "A", ...}` while B is loaded.
    let db = st.store().expect("store").clone();
    db.add_intent("A", &[3, 4, 5], "dev", true, 1)
        .expect("intent");

    load(&st, "/books/A.epub");
    let taken = narrator::wishlist::adopt(&st);
    assert_eq!(taken, vec![3, 4, 5], "the library's order was wiped");
    assert_eq!(db.intents("A").expect("rows").len(), 3);
    let s = st.session();
    assert_eq!(s.queue, vec![3, 4, 5]);
    assert!(s.build_want.contains(&4), "and it is still a download");
}

#[test]
fn a_cancel_from_the_library_is_not_undone_by_opening_the_book() {
    let (_d, st) = state();
    load(&st, "/books/A.epub");
    st.session().queue = vec![3, 4];
    narrator::wishlist::save(&st);
    load(&st, "/books/B.epub");
    // `/api/chapters/cancel {"book": "A"}` while B is loaded.
    let db = st.store().expect("store").clone();
    db.drop_intents_for_book("A").expect("cancel");

    load(&st, "/books/A.epub");
    let taken = narrator::wishlist::adopt(&st);
    assert!(taken.is_empty(), "cancelled chapters came back: {taken:?}");
    assert!(db.intents("A").expect("rows").is_empty());
}

#[test]
fn a_file_the_store_never_saw_is_adopted_once() {
    // The A1's `queue.json` on the first boot of a binary with a store, or a
    // rollback's on the way forward again: no `projection` mark, so its asks may
    // be ones the table has never heard of.
    let (_d, st) = state();
    load(&st, "/books/A.epub");
    let p = narrator::wishlist::path(&st.cfg.work, "A");
    std::fs::create_dir_all(p.parent().expect("dir")).expect("dir");
    std::fs::write(
        &p,
        br#"{"version": 1, "book": "/books/A.epub", "key": "A",
            "updated": "2026-09-11T22:00:00+02:00",
            "items": [{"chapter": 2, "pack": true}, {"chapter": 6}]}"#,
    )
    .expect("seed");

    assert_eq!(narrator::wishlist::adopt(&st), vec![2, 6]);
    let db = st.store().expect("store").clone();
    let rows = db.intents("A").expect("rows");
    assert_eq!(
        rows.iter().map(|r| (r.chapter, r.pack)).collect::<Vec<_>>(),
        vec![(2, true), (6, false)],
        "the table took the file in, order and pack flags"
    );
    assert_eq!(
        file(&st, "A")["projection"],
        serde_json::json!(true),
        "and the file now says it is a copy"
    );

    // Adopted once: a cancel in the table afterwards is not undone by it.
    db.drop_intent("A", 6).expect("cancel");
    load(&st, "/books/A.epub");
    assert_eq!(narrator::wishlist::adopt(&st), vec![2]);
}

#[test]
fn a_file_written_without_a_store_is_what_it_always_was() {
    // No `projection` key at all when there is nothing for the file to be a
    // copy of — the rollback binary reads it as it always has.
    let (_d, st) = state();
    load(&st, "/books/A.epub");
    st.session().queue = vec![1];
    narrator::wishlist::save(&st);
    assert_eq!(file(&st, "A")["projection"], serde_json::json!(true));

    let d = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::for_test(d.path());
    // A work directory the store cannot be opened in.
    std::fs::create_dir_all(cfg.work.join("state.db")).expect("block");
    cfg.vault = None;
    let bare = AppState::new(cfg);
    assert!(bare.store().is_none());
    load(&bare, "/books/A.epub");
    bare.session().queue = vec![1];
    narrator::wishlist::save(&bare);
    assert!(file(&bare, "A").get("projection").is_none());
    // ...and without a store the file is still the whole record.
    load(&bare, "/books/A.epub");
    assert_eq!(narrator::wishlist::adopt(&bare), vec![1]);
}

#[test]
fn a_library_change_brings_the_copy_up_to_date() {
    let (_d, st) = state();
    load(&st, "/books/A.epub");
    st.session().queue = vec![3, 4];
    narrator::wishlist::save(&st);
    load(&st, "/books/B.epub");
    let db = st.store().expect("store").clone();
    db.drop_intent("A", 3).expect("retire");
    narrator::wishlist::project(&st, "A");
    assert_eq!(chapters_in_file(&st, "A"), vec![4]);
    assert_eq!(file(&st, "A")["book"], serde_json::json!("/books/A.epub"));
}

#[tokio::test]
async fn over_http_a_library_order_is_there_when_the_book_is_opened() {
    // The same thing end to end: place an order on a book from the library,
    // cancel part of it, open the book, and find exactly what is left.
    let h = harness::Harness::with(|c| {
        c.lookahead = 4;
        c.prerender_chapters = 0;
    })
    .await;
    let other = h.add_book("Later (2026).epub");
    let (code, loaded) = h
        .post_json("/api/load", serde_json::json!({"path": other}))
        .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{loaded}");
    let other_key = loaded["key"].as_str().unwrap_or_default().to_string();
    // Ask for something and take it back while it is loaded, which leaves
    // Later's queue.json behind, empty — the copy that used to win.
    let (code, _) = h
        .post_json("/api/chapters/render", serde_json::json!({"chapters": [0]}))
        .await;
    assert_eq!(code, axum::http::StatusCode::OK);
    let (code, _) = h
        .post_json("/api/chapters/cancel", serde_json::json!({}))
        .await;
    assert_eq!(code, axum::http::StatusCode::OK);
    assert!(narrator::wishlist::path(&h.work(), &other_key).exists());
    h.load().await;

    let (code, body) = h
        .post_json(
            "/api/chapters/render",
            serde_json::json!({"book": other_key, "chapters": [1, 2, 3], "pack": true}),
        )
        .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{body}");
    let (code, body) = h
        .post_json(
            "/api/chapters/cancel",
            serde_json::json!({"book": other_key, "chapters": [2]}),
        )
        .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{body}");

    let (code, loaded) = h
        .post_json("/api/load", serde_json::json!({"path": other}))
        .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{loaded}");
    let s = h.state.session();
    let mut q = s.queue.clone();
    q.sort_unstable();
    // Chapters may already have left the queue if the worker finished them;
    // what must not happen is the order vanishing or chapter 2 coming back.
    assert!(!q.contains(&2), "a cancelled chapter came back: {q:?}");
    let owed: Vec<usize> = q
        .iter()
        .copied()
        .chain(s.pack_queue.iter().copied())
        .chain(s.build_want.iter().copied())
        .collect();
    drop(s);
    let rendered = |ci: usize| {
        let n = narrator::plancache::read_raw(&h.work(), &other_key)
            .and_then(|p| p.get(ci).map(|c| c.chunks.len()))
            .unwrap_or(0);
        (0..n).all(|i| narrator::cache::chunk_path(&h.work(), &other_key, ci, i).exists())
    };
    for ci in [1usize, 3] {
        assert!(
            owed.contains(&ci) || rendered(ci),
            "chapter {ci} of the library's order was lost: queue {q:?}"
        );
    }
}
