//! `GET /api/library` — the view of the box that is not the session's.
//!
//! Every assertion here is about the same distinction: the endpoint reports a
//! **scan**, the scanner does the scanning, and what the scanner writes is what
//! the filesystem said. So each test seeds real files, runs a real scan, and
//! asks the endpoint what it can see — never the other way round. If a count
//! could be produced without the files being there, the test is not testing
//! anything.

mod harness;

use axum::http::StatusCode;
use harness::Harness;
use narrator::{cache, chapters as pack, library};
use serde_json::{json, Value};

/// Fill a chapter's chunk wavs, the way the render worker would have left it.
fn render_chapter(h: &Harness, key: &str, ci: usize, n: usize) {
    let dir = cache::chapter_dir(&h.work(), key, ci);
    std::fs::create_dir_all(&dir).expect("chapter dir");
    for i in 0..n {
        cache::write_silence_wav(&dir.join(format!("{i:05}.wav")), 0.1, 1, 24_000, 2)
            .expect("silence");
    }
}

/// Put a packed m4a and its manifest where the packer would have. `chunks` is
/// what the manifest *claims*, so a test can make it disagree with the plan.
fn pack_chapter(h: &Harness, key: &str, ci: usize, chunks: usize) -> u64 {
    let (m4a, jf) = pack::chapter_files(&h.work(), key, ci);
    std::fs::create_dir_all(m4a.parent().expect("chapters dir")).expect("dir");
    let body = vec![b'm'; 4096];
    std::fs::write(&m4a, &body).expect("m4a");
    let man = pack::Manifest {
        book: key.to_string(),
        chapter: ci,
        title: format!("Chapter {ci}"),
        chunks,
        starts: (0..chunks).map(|i| i as f64 * 2.0).collect(),
        duration: chunks as f64 * 2.0,
        gap: 0.30,
        para_gap: 0.60,
        bitrate: "64k".into(),
        sample_rate: 24_000,
        bytes: body.len() as u64,
        built: "2026-09-21T00:00:00".into(),
        build_s: 1.0,
    };
    std::fs::write(&jf, serde_json::to_vec(&man).expect("manifest")).expect("write manifest");
    body.len() as u64
}

/// The key is a file stem — spaces and parentheses — so it has to be encoded to
/// survive a URI, exactly as the reader's generated client encodes it.
fn q(key: &str) -> String {
    percent_encoding::utf8_percent_encode(key, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn book<'a>(body: &'a Value, key: &str) -> &'a Value {
    body["books"]
        .as_array()
        .and_then(|a| a.iter().find(|b| b["key"] == json!(key)))
        .unwrap_or_else(|| panic!("no {key} in {body}"))
}

/// A box with nothing on it answers, rather than failing. The reader's library
/// screen paints on first launch or it paints never.
#[tokio::test]
async fn an_empty_library_is_an_answer_not_an_error() {
    let h = Harness::new().await;
    let (code, body) = h.get_json("/api/library").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["books"], json!([]), "{body}");
    assert_eq!(body["scanned_ms"], json!(null), "nothing has been scanned");
}

/// The loaded book turns up with its real shape, and says that it is the loaded
/// one — the single live fact in a response made of scan results.
#[tokio::test]
async fn the_loaded_book_appears_with_its_chapter_count() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    let n = load["chapters"].as_array().map(Vec::len).unwrap_or(0);
    assert!(n > 1, "the fixture has chapters");

    let rep = library::scan_all(&h.state);
    assert_eq!(rep.books, 1, "{rep:?}");
    assert_eq!(rep.chapters, n, "{rep:?}");

    let (code, body) = h.get_json("/api/library").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let b = book(&body, &key);
    assert_eq!(b["chapters"], json!(n), "{b}");
    assert_eq!(b["loaded"], json!(true), "{b}");
    assert_eq!(b["name"], json!("Fixture (2026).epub"), "{b}");
    assert!(b["est_min"].as_f64().unwrap_or(0.0) > 0.0, "{b}");
    assert!(b["total_chunks"].as_u64().unwrap_or(0) > 0, "{b}");
    // Nothing has been rendered, and the scan says so rather than guessing from
    // the plan's existence.
    assert_eq!(b["rendered_chunks"], json!(0), "{b}");
    assert_eq!(b["rendered_chapters"], json!(0), "{b}");
    assert_eq!(b["packed_chapters"], json!(0), "{b}");
    assert_eq!(b["packed_bytes"], json!(0), "{b}");
    // And the answer dates itself.
    assert!(body["scanned_ms"].as_i64().unwrap_or(0) > 0, "{body}");
}

/// The counts are the files, not the bookkeeping. This is the whole reason the
/// table is a cache of a scan: seed wavs and an m4a behind the server's back and
/// the next scan sees them, because it looks.
#[tokio::test]
async fn the_counts_come_from_the_files_that_are_actually_there() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    let n0 = load["chapters"][0]["n"].as_u64().unwrap_or(0) as usize;
    let n1 = load["chapters"][1]["n"].as_u64().unwrap_or(0) as usize;
    assert!(n0 > 1 && n1 > 1);

    // Chapter 0: fully rendered and packed. Chapter 1: one chunk short.
    render_chapter(&h, &key, 0, n0);
    let bytes = pack_chapter(&h, &key, 0, n0);
    render_chapter(&h, &key, 1, n1 - 1);
    library::scan_all(&h.state);

    let (code, body) = h.get_json("/api/library").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let b = book(&body, &key);
    assert_eq!(
        b["rendered_chapters"],
        json!(1),
        "only chapter 0 is whole: {b}"
    );
    assert_eq!(b["rendered_chunks"], json!(n0 + n1 - 1), "{b}");
    assert_eq!(b["packed_chapters"], json!(1), "{b}");
    assert_eq!(
        b["packed_bytes"],
        json!(bytes),
        "measured, not estimated: {b}"
    );

    // And the gc taking the audio back is visible on the next scan, which is
    // what "a cache of a scan, never truth" has to mean in practice.
    std::fs::remove_dir_all(cache::chapter_dir(&h.work(), &key, 0)).expect("evict");
    library::scan_all(&h.state);
    let (_, body) = h.get_json("/api/library").await;
    let b = book(&body, &key);
    assert_eq!(b["rendered_chapters"], json!(0), "{b}");
    assert_eq!(
        b["packed_chapters"],
        json!(1),
        "the m4a is still there: {b}"
    );
}

/// A manifest that disagrees with the plan points at the wrong words, so the
/// file is reported as present-but-not-usable. Same rule as `/api/chapters`.
#[tokio::test]
async fn a_manifest_that_disagrees_with_the_plan_is_not_a_packed_chapter() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    let n0 = load["chapters"][0]["n"].as_u64().unwrap_or(0) as usize;

    render_chapter(&h, &key, 0, n0);
    pack_chapter(&h, &key, 0, n0 + 7);
    library::scan_all(&h.state);

    let (code, body) = h
        .get_json(&format!("/api/library?book={}&chapters=true", q(&key)))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let b = book(&body, &key);
    assert_eq!(b["packed_chapters"], json!(0), "{b}");
    assert_eq!(b["packed_bytes"], json!(0), "{b}");
    let row = &b["chapter_index"][0];
    assert_eq!(row["m4a"], json!(false), "{row}");
    assert!(
        row["bytes"].as_u64().unwrap_or(0) > 0,
        "the file is still on the disk and still costs the space: {row}"
    );
}

/// `?book=` narrows to one; `?chapters=true` is the only way to get the rows,
/// and only ever for that one book.
#[tokio::test]
async fn one_book_and_its_rows_are_asked_for_explicitly() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    let n = load["chapters"].as_array().map(Vec::len).unwrap_or(0);
    library::scan_all(&h.state);

    // The all-books answer never carries rows: 1433 of them per book is the
    // response this endpoint exists to avoid.
    let (_, all) = h.get_json("/api/library?chapters=true").await;
    assert_eq!(
        book(&all, &key).get("chapter_index"),
        None,
        "not in the all-books answer: {all}"
    );

    let (code, one) = h.get_json(&format!("/api/library?book={}", q(&key))).await;
    assert_eq!(code, StatusCode::OK, "{one}");
    assert_eq!(one["books"].as_array().map(Vec::len), Some(1), "{one}");
    assert_eq!(book(&one, &key).get("chapter_index"), None, "not asked for");

    let (_, rows) = h
        .get_json(&format!("/api/library?book={}&chapters=true", q(&key)))
        .await;
    let idx = book(&rows, &key)["chapter_index"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(idx.len(), n, "one row per chapter: {rows}");
    assert_eq!(idx[0]["chapter"], json!(0));
    assert!(
        idx[0]["scanned_ms"].as_i64().unwrap_or(0) > 0,
        "{:?}",
        idx[0]
    );

    // A key nothing answers to is an empty library, not a 404: this endpoint
    // describes what is here, and "not here" is an answer.
    let (code, none) = h.get_json("/api/library?book=nothing-by-that-name").await;
    assert_eq!(code, StatusCode::OK, "{none}");
    assert_eq!(none["books"], json!([]), "{none}");
    assert_eq!(none["scanned_ms"], json!(null), "{none}");
}

/// The row carries where the book was left — which is the point of the screen.
/// Without it a library view can tell you a chapter is downloadable and not that
/// you are three chapters past it.
#[tokio::test]
async fn the_position_comes_back_on_the_row() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    assert_eq!(load["position"], json!(null), "nothing read yet");

    // Opening a chapter forces a position write — the vault record and the
    // store's per-device row both.
    let (code, body) = h.post_json("/api/open", json!({"chapter": 1})).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    library::scan_all(&h.state);

    let (code, body) = h.get_json("/api/library").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let p = &book(&body, &key)["position"];
    assert_eq!(p["chapter"], json!(1), "{p}");
    assert!(p["updated_ms"].as_i64().unwrap_or(0) > 0, "{p}");
    assert!(!p["updated"].as_str().unwrap_or("").is_empty(), "{p}");
    assert_eq!(
        p["chapters_total"].as_u64(),
        load["chapters"].as_array().map(|a| a.len() as u64),
        "{p}"
    );
}

/// A cache with a plan and no `book` row is what a work directory adopted from
/// the python server looks like. It has to appear, or a Rust deploy would look
/// like an empty box to every device that opened the library screen.
#[tokio::test]
async fn a_book_found_only_on_disk_is_adopted() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();

    // Copy the loaded book's plan under a second key, the way a second book in
    // the cache would sit — with no `/api/load` ever having registered it.
    let other = "Adopted (1999)";
    let plan = std::fs::read(cache::plan_path(&h.work(), &key)).expect("plan");
    std::fs::create_dir_all(cache::book_dir(&h.work(), other)).expect("dir");
    std::fs::write(cache::plan_path(&h.work(), other), &plan).expect("write plan");

    let rep = library::scan_all(&h.state);
    assert_eq!(rep.registered, 1, "{rep:?}");
    assert_eq!(rep.books, 2, "{rep:?}");

    let (code, body) = h.get_json("/api/library").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let b = book(&body, other);
    assert!(b["chapters"].as_u64().unwrap_or(0) > 0, "{b}");
    assert_eq!(b["loaded"], json!(false), "{b}");
    assert_eq!(
        b["last_open_ms"],
        json!(null),
        "found, never opened — and a scan must not claim otherwise: {b}"
    );
    // No epub answers to that key, so there is no position to find and the row
    // says so rather than borrowing somebody else's.
    assert_eq!(b["position"], json!(null), "{b}");
}

/// A book the scan has never reached is still described by whatever the `book`
/// table knows, with its counts honestly at zero and no scan stamp to stand
/// behind them.
#[tokio::test]
async fn an_unscanned_book_reports_zero_rather_than_a_guess() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    // `/api/load` registers the book; no scan has run.
    let (code, body) = h.get_json("/api/library").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let b = book(&body, &key);
    assert!(b["chapters"].as_u64().unwrap_or(0) > 0, "{b}");
    assert_eq!(b["total_chunks"], json!(0), "{b}");
    assert_eq!(b["est_min"], json!(null), "nothing measured: {b}");
    assert_eq!(body["scanned_ms"], json!(null), "{body}");
}

/// One book, rescanned on its own — the call the render worker should make when
/// a chapter finishes, and the reason it has to be cheap.
#[tokio::test]
async fn one_book_can_be_rescanned_by_itself() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    let n0 = load["chapters"][0]["n"].as_u64().unwrap_or(0) as usize;

    library::scan_all(&h.state);
    render_chapter(&h, &key, 0, n0);
    library::rescan_book(&h.state, &key);

    let (_, body) = h.get_json("/api/library").await;
    assert_eq!(book(&body, &key)["rendered_chapters"], json!(1), "{body}");
}

/// What a scan actually costs, against a real cache rather than a fixture.
///
/// Opt-in for the same reason `parity_chunking`'s python-plan check is: it needs
/// a work directory somebody has really been rendering into, which only exists
/// on Fernando's machines. It is **read-only** — `plan.json` is read and the
/// chapter directories are `stat`ed, nothing is written and no store is touched
/// — so pointing it at the python server's live work directory is safe.
///
///   NARRATOR_REF_WORK=~/git/narrator/work cargo test --test library -- --ignored --nocapture
#[test]
#[ignore]
fn what_a_scan_of_a_real_cache_costs() {
    let Some(work) = std::env::var_os("NARRATOR_REF_WORK").map(std::path::PathBuf::from) else {
        eprintln!("NARRATOR_REF_WORK unset; nothing to measure");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = narrator::config::Config::for_test(tmp.path());
    cfg.work = work.clone();
    let mut roots: Vec<_> = std::fs::read_dir(cache::audio_root(&work))
        .expect("audio root")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("plan.json").is_file())
        .collect();
    roots.sort();
    for dir in roots {
        let key = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let Ok(raw) = std::fs::read(dir.join("plan.json")) else {
            continue;
        };
        let Ok(plan) = serde_json::from_slice::<Vec<narrator::book::Chapter>>(&raw) else {
            continue;
        };
        let t0 = std::time::Instant::now();
        let rows = library::scan_book(&cfg, &key, &plan);
        let took = t0.elapsed().as_secs_f64();
        let rendered: usize = rows.iter().filter(|r| r.n > 0 && r.rendered >= r.n).count();
        let packed: usize = rows.iter().filter(|r| r.m4a).count();
        eprintln!(
            "{key}: {} chapters, {rendered} rendered, {packed} packed, {took:.3}s",
            rows.len()
        );
    }
}
