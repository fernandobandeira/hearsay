//! The state store, against a real file on disk.
//!
//! The unit tests inside `src/store.rs` cover the pure helpers; everything here
//! needs SQLite to actually be there, and most of it needs the database to be
//! *closed and reopened*, because "survives a restart" is the entire reason this
//! module exists and an in-memory database cannot prove it.

use std::path::PathBuf;

use narrator::store::{BookRow, ChapterIndexRow, PositionRow, Store, StoreError, SCHEMA_VERSION};

fn db(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("work").join("state.db")
}

fn open(dir: &tempfile::TempDir) -> Store {
    Store::open(&db(dir)).expect("open")
}

fn pos(chapter: i64, chunk: i64, updated_ms: i64) -> PositionRow {
    PositionRow {
        chapter,
        chunk,
        chapter_title: format!("Chapter {chapter}"),
        chunks_total: 40,
        chapters_total: 1433,
        updated_ms,
        // Deliberately a lie: the store stamps this itself and every assertion
        // below depends on it ignoring what the caller put here.
        seq: 999_999,
    }
}

// --------------------------------------------------------------- migrations

#[test]
fn the_migration_runs_once_and_opening_again_changes_nothing() {
    let d = tempfile::tempdir().expect("tempdir");
    {
        let st = open(&d);
        assert_eq!(st.schema_version().expect("version"), SCHEMA_VERSION);
        st.touch_device("phone", "Fernando's phone", 1_000)
            .expect("touch");
    }
    // The parent directory was created for us, which is the whole of what `open`
    // is allowed to do to a work directory that has never had a store in it.
    assert!(db(&d).exists());
    {
        let st = open(&d);
        assert_eq!(st.schema_version().expect("version"), SCHEMA_VERSION);
        let ds = st.devices().expect("devices");
        assert_eq!(ds.len(), 1, "a second open re-ran nothing");
        assert_eq!(ds[0].name, "Fernando's phone");
        assert_eq!((ds[0].first_seen, ds[0].last_seen), (1_000, 1_000));
    }
}

#[test]
fn a_path_that_is_not_a_database_is_an_error_rather_than_a_panic() {
    let d = tempfile::tempdir().expect("tempdir");

    // A directory where a file should be: the open itself fails.
    let dir = d.path().join("a-directory");
    std::fs::create_dir_all(&dir).expect("mkdir");
    assert!(matches!(
        Store::open(&dir),
        Err(StoreError::Sqlite(_)) | Err(StoreError::Io(_))
    ));

    // A file that exists and is not SQLite: the open succeeds lazily and the
    // first pragma is where it comes apart. Either way the caller gets an Err to
    // log, and narrator runs without a store.
    let junk = d.path().join("junk.db");
    std::fs::write(&junk, b"this is not a database, it is a sentence").expect("write");
    assert!(Store::open(&junk).is_err());
}

#[test]
fn a_database_from_a_newer_build_is_refused_and_says_so() {
    let d = tempfile::tempdir().expect("tempdir");
    {
        let st = open(&d);
        st.put_book(&BookRow {
            key: "A Book (2016)".into(),
            name: "A Book (2016).epub".into(),
            path: "/books/A Book (2016).epub".into(),
            title: "A Book".into(),
            chapters: 22,
            last_open_ms: Some(10),
            scanned_ms: Some(10),
        })
        .expect("put_book");
    }
    // What a rollback would find: a file some future narrator migrated.
    let c = rusqlite::Connection::open(db(&d)).expect("raw open");
    c.execute("UPDATE meta SET v = '9' WHERE k = 'schema_version'", [])
        .expect("forge");
    drop(c);
    match Store::open(&db(&d)) {
        Err(StoreError::FromTheFuture(9)) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------- seq

#[test]
fn the_sequence_is_monotonic_and_survives_a_reopen() {
    let d = tempfile::tempdir().expect("tempdir");
    let last = {
        let st = open(&d);
        let a = st.seq().expect("seq");
        let b = st.seq().expect("seq");
        assert!(b > a, "{b} follows {a}");
        b
    };
    let st = open(&d);
    let c = st.seq().expect("seq");
    assert!(c > last, "{c} follows {last} across a restart");
    // And a write takes its number from the same counter, so nothing can hand
    // out a sequence a previous process already used.
    let s = st
        .put_position("A Book (2016).epub", "phone", pos(3, 4, 100))
        .expect("put");
    assert!(s > c, "{s} follows {c}");
}

// ---------------------------------------------------------------- positions

#[test]
fn a_position_round_trips_and_is_stamped_by_the_store() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    let seq = st
        .put_position("A Book (2016).epub", "phone", pos(3, 17, 1_700))
        .expect("put");
    let got = st
        .position("A Book (2016).epub", "phone")
        .expect("position")
        .expect("some");
    assert_eq!(got.seq, seq, "the caller's 999999 was ignored");
    assert_eq!((got.chapter, got.chunk), (3, 17));
    assert_eq!(got.chapter_title, "Chapter 3");
    assert_eq!(got.chapters_total, 1433);
    assert_eq!(got.updated_ms, 1_700);
    assert!(st
        .position("A Book (2016).epub", "laptop")
        .expect("position")
        .is_none());
}

#[test]
fn the_newest_position_is_by_time_then_by_sequence() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    let book = "Lord of Mysteries.epub";
    st.put_position(book, "phone", pos(10, 1, 1_000))
        .expect("a");
    st.put_position(book, "laptop", pos(20, 2, 2_000))
        .expect("b");
    let (dev, row) = st.newest_position(book).expect("newest").expect("some");
    assert_eq!((dev.as_str(), row.chapter), ("laptop", 20), "later wins");

    // The same millisecond, which a tunnel delivering a backlog produces for
    // real: the tie-break is the order the writes landed in, not whichever row
    // the planner reached first.
    st.put_position(book, "phone", pos(30, 3, 2_000))
        .expect("c");
    let (dev, row) = st.newest_position(book).expect("newest").expect("some");
    assert_eq!((dev.as_str(), row.chapter), ("phone", 30));

    // Every device is still there, newest first.
    let all = st.positions_for_book(book).expect("for book");
    assert_eq!(
        all.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>(),
        vec!["phone", "laptop"]
    );

    // And the projection over the whole library is one row per book.
    st.put_position("Another.epub", "phone", pos(1, 1, 500))
        .expect("d");
    let newest = st.all_newest_positions().expect("all newest");
    assert_eq!(
        newest
            .iter()
            .map(|(b, dev, r)| (b.as_str(), dev.as_str(), r.chapter))
            .collect::<Vec<_>>(),
        vec![
            ("Another.epub", "phone", 1),
            ("Lord of Mysteries.epub", "phone", 30),
        ]
    );
}

#[test]
fn a_position_is_one_row_per_device_per_book_and_the_latest_replaces_it() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    for (ch, ms) in [(1, 100), (2, 200), (3, 300)] {
        st.put_position("A.epub", "phone", pos(ch, 0, ms))
            .expect("put");
    }
    assert_eq!(st.positions_for_book("A.epub").expect("rows").len(), 1);
    assert_eq!(
        st.position("A.epub", "phone")
            .expect("position")
            .expect("some")
            .chapter,
        3
    );
}

// ----------------------------------------------------------------- furthest

#[test]
fn the_furthest_mark_only_ever_moves_forward() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    let (book, dev) = ("A.epub", "phone");

    assert!(st.bump_furthest(book, dev, 5, 10, 1_000).expect("first"));
    assert!(
        st.bump_furthest(book, dev, 5, 11, 1_100).expect("forward"),
        "one chunk on is further"
    );
    assert!(
        !st.bump_furthest(book, dev, 5, 4, 1_200).expect("back"),
        "re-reading the scene does not shrink it"
    );
    assert!(
        !st.bump_furthest(book, dev, 4, 999, 1_300)
            .expect("back a chapter"),
        "a later chunk of an earlier chapter is still behind"
    );
    assert!(
        !st.bump_furthest(book, dev, 5, 11, 1_400).expect("same"),
        "standing still is not a move, so the timestamp holds"
    );
    let f = st.furthest(book, dev).expect("furthest").expect("some");
    assert_eq!((f.chapter, f.chunk, f.updated_ms), (5, 11, 1_100));

    // Across a chapter boundary: chunk 0 of the next chapter beats chunk 11 of
    // this one, which a naive comparison on `chunk` alone gets wrong.
    assert!(st.bump_furthest(book, dev, 6, 0, 1_500).expect("next"));
    let f = st.furthest(book, dev).expect("furthest").expect("some");
    assert_eq!((f.chapter, f.chunk, f.updated_ms), (6, 0, 1_500));

    // It is per device, and per book.
    assert!(st.furthest(book, "laptop").expect("furthest").is_none());
    assert!(st.furthest("B.epub", dev).expect("furthest").is_none());
}

// ------------------------------------------------------------------- intent

#[test]
fn the_order_asked_is_the_order_kept_and_it_survives_a_reopen() {
    let d = tempfile::tempdir().expect("tempdir");
    let asked = [7, 3, 4, 70, 12];
    {
        let st = open(&d);
        st.add_intent("Lord of Mysteries", &asked, "phone", true, 1_000)
            .expect("add");
        assert_eq!(
            st.intents("Lord of Mysteries")
                .expect("intents")
                .iter()
                .map(|i| i.chapter)
                .collect::<Vec<_>>(),
            asked,
            "not sorted, not a set — the order somebody tapped them in"
        );
    }
    let st = open(&d);
    let rows = st.intents("Lord of Mysteries").expect("intents");
    assert_eq!(
        rows.iter().map(|i| i.chapter).collect::<Vec<_>>(),
        asked,
        "an overnight download outlives the process that took the request"
    );
    assert!(rows.iter().all(|i| i.pack && i.device == "phone"));
    assert!(rows.iter().all(|i| i.created_ms == 1_000));
    assert!(
        rows.windows(2).all(|w| w[0].seq < w[1].seq),
        "the order is a number, because rows have none"
    );

    // A second book interleaves in one global order, which is the order the box
    // should work through them in.
    st.add_intent("7 Powers (2016)", &[1, 2], "laptop", false, 2_000)
        .expect("add");
    let all = st.all_intents().expect("all");
    assert_eq!(
        all.iter()
            .map(|i| (i.book.as_str(), i.chapter))
            .collect::<Vec<_>>(),
        vec![
            ("Lord of Mysteries", 7),
            ("Lord of Mysteries", 3),
            ("Lord of Mysteries", 4),
            ("Lord of Mysteries", 70),
            ("Lord of Mysteries", 12),
            ("7 Powers (2016)", 1),
            ("7 Powers (2016)", 2),
        ]
    );
}

#[test]
fn asking_again_keeps_the_place_in_the_queue_and_never_downgrades_the_pack() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    st.add_intent("B", &[5, 6], "phone", true, 1_000)
        .expect("add");
    let first = st.intents("B").expect("intents");
    // The reconciler re-places the same order every couple of minutes.
    st.add_intent("B", &[6, 5], "phone", false, 9_000)
        .expect("again");
    let again = st.intents("B").expect("intents");
    assert_eq!(
        again.iter().map(|i| i.chapter).collect::<Vec<_>>(),
        vec![5, 6],
        "a repeat of last night's ask does not go to the back of tonight's queue"
    );
    assert_eq!(
        again.iter().map(|i| i.seq).collect::<Vec<_>>(),
        first.iter().map(|i| i.seq).collect::<Vec<_>>()
    );
    assert!(
        again.iter().all(|i| i.pack),
        "a render-only repeat must not cancel a download somebody asked for"
    );
    assert!(again.iter().all(|i| i.created_ms == 1_000));
}

#[test]
fn the_poison_counter_counts_and_asking_again_clears_it() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    st.add_intent("B", &[4], "", true, 1_000).expect("add");
    assert_eq!(st.intents("B").expect("intents")[0].tries, 0);
    assert_eq!(st.bump_tries("B", 4).expect("bump"), 1);
    assert_eq!(st.bump_tries("B", 4).expect("bump"), 2);
    assert_eq!(st.bump_tries("B", 4).expect("bump"), 3);
    assert_eq!(st.intents("B").expect("intents")[0].tries, 3);

    st.reset_tries("B", 4).expect("reset");
    assert_eq!(st.intents("B").expect("intents")[0].tries, 0);

    st.bump_tries("B", 4).expect("bump");
    st.add_intent("B", &[4], "", true, 2_000).expect("again");
    assert_eq!(
        st.intents("B").expect("intents")[0].tries,
        0,
        "asking again is the retry"
    );

    // A chapter nobody asked for has failed nothing.
    assert_eq!(st.bump_tries("B", 99).expect("bump"), 0);
    assert_eq!(st.bump_tries("Nope", 4).expect("bump"), 0);
    st.reset_tries("Nope", 4).expect("reset of nothing");
}

#[test]
fn an_order_can_be_cancelled_one_chapter_or_a_book_at_a_time() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    st.add_intent("B", &[1, 2, 3], "", true, 1_000)
        .expect("add");
    st.add_intent("C", &[9], "", true, 1_000).expect("add");
    assert!(st.drop_intent("B", 2).expect("drop"));
    assert!(!st.drop_intent("B", 2).expect("drop again"), "already gone");
    assert_eq!(
        st.intents("B")
            .expect("intents")
            .iter()
            .map(|i| i.chapter)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(st.drop_intents_for_book("B").expect("drop all"), 2);
    assert!(st.intents("B").expect("intents").is_empty());
    assert_eq!(
        st.all_intents().expect("all").len(),
        1,
        "the other book is untouched"
    );
}

// ---------------------------------------------------------------- inventory

#[test]
fn inventory_is_reported_wholesale_and_keeps_what_did_not_move() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    let (dev, book) = ("phone", "Lord of Mysteries");

    st.put_inventory(dev, book, 1, Some(6_100_000), 1_000)
        .expect("put");
    st.put_inventory(dev, book, 2, Some(5_900_000), 1_000)
        .expect("put");
    st.put_inventory(dev, book, 3, None, 1_000).expect("put");
    st.put_inventory("laptop", book, 1, Some(6_100_000), 1_000)
        .expect("put");

    // The reader's sweep asks Cache Storage and reports the answer: 2 is still
    // there, 3 was trimmed, 4 is new. 1 was evicted by the quota.
    st.replace_inventory(dev, book, &[2, 3, 4], 9_000)
        .expect("replace");
    let rows = st.inventory_for_book(book).expect("inventory");
    assert_eq!(
        rows.iter()
            .map(|r| (r.device.as_str(), r.chapter))
            .collect::<Vec<_>>(),
        vec![("laptop", 1), ("phone", 2), ("phone", 3), ("phone", 4)],
        "wholesale for one device, and only that device"
    );
    let phone: Vec<_> = rows.iter().filter(|r| r.device == dev).collect();
    assert_eq!(
        (phone[0].bytes, phone[0].stored_ms),
        (Some(5_900_000), 1_000),
        "a chapter that did not move keeps its size and when it was stored"
    );
    assert_eq!((phone[2].bytes, phone[2].stored_ms), (None, 9_000));

    // A device that has given everything back says so with an empty list.
    st.replace_inventory(dev, book, &[], 10_000).expect("empty");
    assert_eq!(
        st.inventory_for_book(book)
            .expect("inventory")
            .iter()
            .map(|r| r.device.as_str())
            .collect::<Vec<_>>(),
        vec!["laptop"]
    );

    assert!(st.drop_inventory("laptop", book, 1).expect("drop"));
    assert!(!st.drop_inventory("laptop", book, 1).expect("drop again"));
    assert!(st.inventory_for_book(book).expect("inventory").is_empty());
}

#[test]
fn a_re_reported_size_updates_and_a_missing_one_does_not_erase() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    st.put_inventory("phone", "B", 1, Some(100), 1_000)
        .expect("put");
    st.put_inventory("phone", "B", 1, None, 2_000).expect("put");
    let r = &st.inventory_for_book("B").expect("inventory")[0];
    assert_eq!(
        (r.bytes, r.stored_ms),
        (Some(100), 2_000),
        "'it is here and I did not measure it' is not 'it is zero bytes'"
    );
    st.put_inventory("phone", "B", 1, Some(200), 3_000)
        .expect("put");
    assert_eq!(st.inventory_for_book("B").expect("inv")[0].bytes, Some(200));
}

// ------------------------------------------------------------ chapter index

#[test]
fn the_chapter_index_round_trips_whole_and_by_the_window() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    let rows: Vec<ChapterIndexRow> = (0..10)
        .map(|i| ChapterIndexRow {
            chapter: i,
            n: 33 + i,
            rendered: i,
            m4a: i % 2 == 0,
            bytes: if i % 2 == 0 { Some(6_000_000) } else { None },
            duration: if i % 2 == 0 { Some(742.5) } else { None },
            title: format!("Chapter {i}"),
            est_s: Some(700.0 + i as f64),
            scanned_ms: 1_000,
        })
        .collect();
    st.put_chapter_index("LoM", &rows).expect("put");

    let back = st.chapter_index("LoM").expect("index");
    assert_eq!(back, rows, "every column, including the nulls");

    let w = st.chapter_index_window("LoM", 3, 5).expect("window");
    assert_eq!(
        w.iter().map(|r| r.chapter).collect::<Vec<_>>(),
        vec![3, 4, 5],
        "inclusive at both ends, like ?from=&to="
    );
    assert_eq!(w[0], rows[3]);
    assert_eq!(
        st.chapter_index_window("LoM", 8, 99).expect("window").len(),
        2,
        "clamped to what exists"
    );
    assert!(st
        .chapter_index_window("LoM", 5, 3)
        .expect("window")
        .is_empty());
    assert!(st.chapter_index("Nothing").expect("index").is_empty());

    // A rescan replaces what it saw. This is a *cache of a scan*, so a later
    // observation simply wins; nothing here decides whether to render.
    st.put_chapter_index(
        "LoM",
        &[ChapterIndexRow {
            rendered: 33,
            m4a: true,
            scanned_ms: 2_000,
            ..rows[0].clone()
        }],
    )
    .expect("rescan");
    let back = st.chapter_index("LoM").expect("index");
    assert_eq!(
        back.len(),
        10,
        "a one-chapter scan is not a one-chapter book"
    );
    assert_eq!(
        (back[0].rendered, back[0].m4a, back[0].scanned_ms),
        (33, true, 2_000)
    );
}

// ------------------------------------------------------------------- books

#[test]
fn recent_books_puts_the_unopened_last() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    let book = |key: &str, last: Option<i64>| BookRow {
        key: key.into(),
        name: format!("{key}.epub"),
        path: format!("/books/{key}.epub"),
        title: key.into(),
        chapters: 22,
        last_open_ms: last,
        scanned_ms: Some(1),
    };
    st.put_book(&book("never", None)).expect("put");
    st.put_book(&book("old", Some(1_000))).expect("put");
    st.put_book(&book("new", Some(3_000))).expect("put");
    st.put_book(&book("also-never", None)).expect("put");

    assert_eq!(
        st.recent_books(10)
            .expect("recent")
            .iter()
            .map(|b| b.key.as_str())
            .collect::<Vec<_>>(),
        vec!["new", "old", "also-never", "never"],
        "SQLite sorts NULL first under a plain DESC; this must not"
    );
    assert_eq!(
        st.recent_books(2)
            .expect("recent")
            .iter()
            .map(|b| b.key.as_str())
            .collect::<Vec<_>>(),
        vec!["new", "old"]
    );
    assert_eq!(st.books().expect("books").len(), 4);
    assert_eq!(
        st.book("old").expect("book").expect("some").name,
        "old.epub"
    );
    assert!(st.book("gone").expect("book").is_none());
}

#[test]
fn a_rescan_does_not_forget_when_a_book_was_last_opened() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    let mut row = BookRow {
        key: "LoM".into(),
        name: "01 - Lord of Mysteries.epub".into(),
        path: "/books/01 - Lord of Mysteries.epub".into(),
        title: String::new(),
        chapters: 0,
        last_open_ms: None,
        scanned_ms: Some(1_000),
    };
    st.put_book(&row).expect("put");
    st.touch_book_open("LoM", 5_000).expect("open");

    // The watcher sees the file again — a vault git sync, a re-parse — and
    // knows nothing about when anybody opened it.
    row.title = "Lord of Mysteries".into();
    row.chapters = 1433;
    row.scanned_ms = Some(6_000);
    st.put_book(&row).expect("rescan");

    let got = st.book("LoM").expect("book").expect("some");
    assert_eq!(got.last_open_ms, Some(5_000), "the scan did not null it");
    assert_eq!(
        (got.chapters, got.title.as_str()),
        (1433, "Lord of Mysteries")
    );
    assert_eq!(got.scanned_ms, Some(6_000));

    // Touching a book nobody has recorded is a no-op, not an error.
    st.touch_book_open("unknown", 7_000).expect("touch");
    assert!(st.book("unknown").expect("book").is_none());
}

// ----------------------------------------------------------------- devices

#[test]
fn a_device_keeps_its_first_sighting_and_its_name() {
    let d = tempfile::tempdir().expect("tempdir");
    let st = open(&d);
    st.touch_device("phone", "", 1_000).expect("touch");
    st.touch_device("phone", "Fernando's phone", 2_000)
        .expect("touch");
    st.touch_device("phone", "", 3_000).expect("touch");
    st.touch_device("", "", 2_500).expect("the legacy client");

    let ds = st.devices().expect("devices");
    assert_eq!(
        ds.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        vec!["phone", ""],
        "most recently seen first, and the plugin gets a row like anyone else"
    );
    assert_eq!(ds[0].first_seen, 1_000);
    assert_eq!(ds[0].last_seen, 3_000);
    assert_eq!(
        ds[0].name, "Fernando's phone",
        "a client that stops sending a label does not erase one somebody typed"
    );

    assert_eq!(
        st.active_devices(2_600)
            .expect("active")
            .iter()
            .map(|x| x.id.as_str())
            .collect::<Vec<_>>(),
        vec!["phone"]
    );
    assert!(st.active_devices(9_000).expect("active").is_empty());
}

// -------------------------------------------------------------- everything

#[test]
fn every_table_survives_a_restart_together() {
    let d = tempfile::tempdir().expect("tempdir");
    {
        let st = open(&d);
        st.touch_device("phone", "phone", 1).expect("device");
        st.put_position("A.epub", "phone", pos(2, 3, 100))
            .expect("position");
        st.bump_furthest("A.epub", "phone", 9, 0, 100)
            .expect("furthest");
        st.add_intent("A", &[9, 10], "phone", true, 100)
            .expect("intent");
        st.put_inventory("phone", "A", 9, Some(42), 100)
            .expect("inventory");
        st.put_chapter_index(
            "A",
            &[ChapterIndexRow {
                chapter: 9,
                n: 33,
                rendered: 33,
                m4a: true,
                bytes: Some(42),
                duration: Some(1.5),
                title: "Nine".into(),
                est_s: Some(1.4),
                scanned_ms: 100,
            }],
        )
        .expect("index");
        st.put_book(&BookRow {
            key: "A".into(),
            name: "A.epub".into(),
            path: "/books/A.epub".into(),
            title: "A".into(),
            chapters: 22,
            last_open_ms: Some(100),
            scanned_ms: Some(100),
        })
        .expect("book");
    }
    let st = open(&d);
    assert_eq!(st.devices().expect("d").len(), 1);
    assert_eq!(
        st.position("A.epub", "phone")
            .expect("p")
            .expect("some")
            .chunk,
        3
    );
    assert_eq!(
        st.furthest("A.epub", "phone")
            .expect("f")
            .expect("some")
            .chapter,
        9
    );
    assert_eq!(st.intents("A").expect("i").len(), 2);
    assert_eq!(st.inventory_for_book("A").expect("v").len(), 1);
    assert_eq!(st.chapter_index("A").expect("c").len(), 1);
    assert_eq!(st.recent_books(5).expect("b").len(), 1);
}
