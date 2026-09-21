//! The library scan: what every book on this box has ready, for a reader that
//! has loaded none of them.
//!
//! **The question this answers, and why nothing else could.**
//! [`crate::api::chapters::chapter_rows`] describes one book — the one the
//! session is holding — because that is where the plan lives, and it describes
//! it by walking the cache: a `read_dir` per chapter and a `stat` per packed
//! file, a few thousand of each on the 1433-chapter *Lord of Mysteries*,
//! memoised for 1.5 s because the drawer polls it. That is the right shape for
//! the book in front of someone and the wrong shape for the question Fernando
//! actually asks when he picks up a phone: *of everything on this box, what is
//! rendered, what is packed, and what can I take with me?* Answering that
//! per-request would be the same scan once per book in the library, on a machine
//! with two ARM cores one of which is always inside Kokoro. So it is answered
//! from a table, and this module is what fills the table.
//!
//! # The rule, restated because it is the whole rewrite
//!
//! `chapter_index` is **a cache of a filesystem scan and never truth**. See
//! [`crate::store`]'s module doc and the disk-truth invariant in
//! [`crate::render`]: the bug this server exists to fix is a renderer that
//! believed its own bookkeeping over the filesystem, and a `rendered` column
//! sitting in a database is that bug with a schema. So the direction is strictly
//! one way — this module *writes* rows, [`crate::api::library`] *reads* them,
//! and **nothing in the renderer or the packer may consult them to decide
//! whether to render or pack anything**. A row is a photograph of a moment; the
//! gc can make it wrong the instant after it is taken, which is exactly why
//! every row carries `scanned_ms` and the endpoint reports the oldest one it
//! served.
//!
//! # Three decisions worth the words
//!
//! **The plan is read raw from `plan.json`**, the way [`crate::export`] reads
//! it, not through [`crate::plancache`]. The stamp beside a plan answers "may a
//! *parse* be reused", which is a question about the epub; an index of audio
//! already on disk has no business going blank because somebody's git sync
//! touched an mtime. The audio is there either way, and the chunk count that
//! names it is the plan that rendered it.
//!
//! **Both roots are consulted: the `book` table and `work/audio/*/plan.json`.**
//! Neither alone is the library. A book is registered in the table by
//! `/api/load`, so a cache adopted from the python server — or from a work
//! directory that predates the table — has audio, a plan, packed chapters and no
//! row at all. Finding those and registering them is how a Rust deploy adopts an
//! existing cache in place rather than pretending it is empty.
//!
//! **The scanner stands down for whisper and for a stalled renderer**, in that
//! order and for the reasons the render worker and the packer already do (see
//! [the STT priority gate][crate::stt] and the packer's hold-back in
//! [`crate::render`]). A full scan is thousands of `stat` calls; a voice memo
//! exists in exactly one place until `/api/note` answers, and a reader waiting
//! on the chunk under their own playhead is waiting *now*. A stale library view
//! costs somebody a refresh. It is the cheapest thing on this box to postpone,
//! so it is the first thing to yield.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::book::{est_chapter_s, Chapter};
use crate::cache;
use crate::chapters as pack;
use crate::config::Config;
use crate::state::AppState;
use crate::store::{BookRow, ChapterIndexRow};

/// How often the background scanner re-walks the whole library.
///
/// Five minutes is chosen against what makes a row wrong: a chapter finishing,
/// a pack landing, the gc evicting chunks. None of those is news the reader
/// learns from here — it learns them from `/api/events` and from
/// `/api/chapters` for the book it has open — so this interval only has to be
/// short enough that a library view opened cold is not embarrassing, and long
/// enough that a few thousand `stat`s are not a background load on two cores.
///
/// This is the **default** for `LIBRARY_SCAN_EVERY_S`
/// ([`Config::library_scan_every_s`](crate::config::Config::library_scan_every_s)),
/// which is what the scanner actually reads.
pub const SCAN_EVERY_S: f64 = 300.0;

/// The longest single sleep between checks, so `stop` is noticed promptly at
/// shutdown rather than up to [`SCAN_EVERY_S`] later.
const SLICE: Duration = Duration::from_millis(500);

/// The longest the scanner defers to a renderer stalled under the playhead.
///
/// The same bound, and the same argument, as the packer's `PACK_HOLD_MAX_S`:
/// past thirty seconds the renderer is not slow, it is wedged (a wedged
/// espeak-ng, a work directory gone read-only), and a library view that never
/// refreshes again until somebody restarts the container is a worse failure than
/// the one the hold-back prevents.
const HOLD_MAX_S: f64 = 30.0;

/// What one pass did. Returned so a caller — a test, a log line — can say
/// whether a scan found anything, without reading the table back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanReport {
    /// Books whose rows were written.
    pub books: usize,
    /// Chapter rows written across all of them.
    pub chapters: usize,
    /// Books found on disk that the `book` table had never heard of.
    pub registered: usize,
    /// Wall time, which is the only honest measure of a scan: it is `stat`
    /// calls, so it is the disk's answer and not this process's.
    pub took_s: f64,
}

// ------------------------------------------------------------------ one book

/// Scan one book's chapters against the filesystem.
///
/// Pure apart from the `stat`s: it takes the plan it is told to measure against
/// rather than looking one up, so a caller that already holds a plan does not
/// read it twice and a test can hand in three chapters.
///
/// Every field is the one [`crate::api::chapters::ChapterRow`] computes, by the
/// same rule, including the one that is easy to get wrong: **a manifest whose
/// `chunks` disagrees with the plan makes `m4a` read false**. Such a file exists
/// and plays, but its chunk→second table points at words that are no longer at
/// those indices, so a reader seeking into it lands somewhere else. `bytes` is
/// still reported, because the file is still there and still taking up the
/// space the gc cares about — what is false is that it is *usable*.
pub fn scan_book(cfg: &Config, key: &str, plan: &[Chapter]) -> Vec<ChapterIndexRow> {
    let now_ms = chrono::Local::now().timestamp_millis();
    plan.iter()
        .enumerate()
        .map(|(ci, ch)| {
            let n = ch.chunks.len();
            let rendered = cache::rendered_count(&cache::chapter_dir(&cfg.work, key, ci), n);
            let (m4a_path, _) = pack::chapter_files(&cfg.work, key, ci);
            let bytes = m4a_path.metadata().ok().map(|m| m.len());
            let mut m4a = bytes.is_some();
            let mut duration = None;
            if m4a {
                if let Some(man) = pack::read_manifest(&cfg.work, key, ci) {
                    duration = Some(man.duration);
                    if man.chunks != n {
                        m4a = false;
                    }
                }
            }
            ChapterIndexRow {
                chapter: ci,
                n,
                rendered,
                m4a,
                bytes,
                duration,
                title: ch.display_title(),
                est_s: Some(est_chapter_s(
                    &ch.chunks,
                    cfg.chapter_gap_s,
                    cfg.chapter_para_gap_s,
                    cfg.silence_s,
                )),
                // One timestamp for the whole book: a scan is a single
                // observation of a single moment, and rows stamped one by one
                // would let a reader think half a book is fresher than the
                // other half of the same walk.
                scanned_ms: now_ms,
            }
        })
        .collect()
}

/// The plan the server left beside the audio, read raw.
///
/// Deliberately not [`crate::plancache::load`] — see the module doc. `None` for
/// anything unexpected (no file, JSON that will not parse, an empty plan), all
/// of which mean the same thing to every caller here: there is nothing to scan,
/// carry on with the next book.
fn read_plan(work: &Path, key: &str) -> Option<Vec<Chapter>> {
    let raw = std::fs::read(cache::plan_path(work, key)).ok()?;
    let plan: Vec<Chapter> = serde_json::from_slice(&raw).ok()?;
    (!plan.is_empty()).then_some(plan)
}

/// Scan one book and write its rows. Cheap enough to call on a chapter
/// completing — one plan read and a `read_dir` per chapter — which is what it is
/// for: the render worker and the packer own the moments a row actually goes
/// stale, and this is the call they should make when they do.
pub fn rescan_book(st: &Arc<AppState>, key: &str) {
    let Some(db) = st.store() else { return };
    let Some(plan) = read_plan(&st.cfg.work, key) else {
        return;
    };
    let rows = scan_book(&st.cfg, key, &plan);
    if let Err(e) = db.put_chapter_index(key, &rows) {
        tracing::warn!("library: could not record the scan of {key}: {e}");
    }
}

// ----------------------------------------------------------- the whole shelf

/// Every book with a `plan.json` under `work/audio`.
///
/// The plan is what makes a directory a book rather than a stray: `book_key`
/// truncates at fifty characters, so a directory name alone is a guess, and a
/// half-made cache directory with no plan has nothing to index.
fn cached_keys(work: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(cache::audio_root(work)) else {
        return out;
    };
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if !e.path().join("plan.json").is_file() {
            continue;
        }
        out.push(e.file_name().to_string_lossy().to_string());
    }
    out.sort();
    out
}

/// Cache key → the epub it was made from, across both book roots.
///
/// Built only when there is at least one unregistered key, because it is the one
/// part of a scan that touches the vault: the point is to recover a book's
/// **name** (which is what `.narrator-positions.json` and therefore every
/// position is keyed by) and its path, neither of which survives into the cache
/// directory. `book_key` is lossy in the other direction — it truncates — so
/// this goes forwards from the file and matches, rather than guessing backwards
/// from the key.
fn epubs_by_key(cfg: &Config) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let mut found = Vec::new();
    for d in &cfg.books {
        walk_epubs(d, &mut found);
    }
    for p in found {
        let key = cache::book_key(&p.to_string_lossy());
        out.entry(key).or_insert(p);
    }
    out
}

fn walk_epubs(d: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_epubs(&p, out),
            Ok(_)
                if p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("epub")) =>
            {
                out.push(p)
            }
            _ => {}
        }
    }
}

/// The row to register for a book found on disk that the table never knew.
///
/// A key with no epub behind it still gets a row: the audio is real, the reader
/// can still be told what is packed, and refusing to name a book because its
/// source file has moved would hide exactly the chapters somebody most wants
/// off the box. What it loses is the position — that is keyed by file name — so
/// the name falls back to the key and the lookup simply misses, which reads as
/// "never opened" rather than as somebody else's position.
fn adopt(key: &str, chapters: usize, epub: Option<&PathBuf>, now_ms: i64) -> BookRow {
    match epub {
        Some(p) => BookRow {
            key: key.to_string(),
            name: p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| key.to_string()),
            path: p.to_string_lossy().to_string(),
            title: p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| key.to_string()),
            chapters,
            // Never opened *in this server's memory*, which is a different claim
            // from "never opened" and the only one this can honestly make. Null
            // leaves whatever the table holds; see `Store::put_book`.
            last_open_ms: None,
            scanned_ms: Some(now_ms),
        },
        None => BookRow {
            key: key.to_string(),
            name: key.to_string(),
            path: String::new(),
            title: key.to_string(),
            chapters,
            last_open_ms: None,
            scanned_ms: Some(now_ms),
        },
    }
}

/// Walk the whole library and write what it finds.
///
/// Nothing here can fail the caller: a store that will not answer, a plan that
/// will not parse, a book directory that vanished between the `read_dir` and the
/// `stat` — each is logged and skipped, and the pass carries on with the next
/// book. A scan that indexes nine books out of ten is worth strictly more than
/// one that gives up on the first bad row.
pub fn scan_all(st: &Arc<AppState>) -> ScanReport {
    let t0 = Instant::now();
    let mut rep = ScanReport::default();
    let Some(db) = st.store() else {
        return rep;
    };
    let known: Vec<BookRow> = match db.books() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("library: could not list the books: {e}");
            return rep;
        }
    };
    let mut by_key: HashMap<String, BookRow> =
        known.into_iter().map(|b| (b.key.clone(), b)).collect();
    let on_disk = cached_keys(&st.cfg.work);
    let strangers: Vec<String> = on_disk
        .iter()
        .filter(|k| !by_key.contains_key(*k))
        .cloned()
        .collect();
    let epubs = if strangers.is_empty() {
        HashMap::new()
    } else {
        epubs_by_key(&st.cfg)
    };

    let now_ms = chrono::Local::now().timestamp_millis();
    // The table's books first, then whatever else has audio: a key in both is
    // scanned once, and the order means a book somebody has actually opened is
    // indexed before one that merely has a directory.
    let mut keys: Vec<String> = by_key.keys().cloned().collect();
    keys.sort();
    keys.extend(strangers.iter().cloned());

    for key in keys {
        if st.stop.load(Ordering::SeqCst) {
            break;
        }
        let Some(plan) = read_plan(&st.cfg.work, &key) else {
            // A registered book with no plan on disk is not an error: it was
            // loaded once, its cache was cleared, and it has nothing to index.
            // Its existing rows are left alone rather than blanked — deleting
            // them would turn "I cannot see it right now" into "it is gone".
            continue;
        };
        let rows = scan_book(&st.cfg, &key, &plan);
        if let Err(e) = db.put_chapter_index(&key, &rows) {
            tracing::warn!("library: could not record the scan of {key}: {e}");
            continue;
        }
        rep.chapters += rows.len();
        rep.books += 1;
        let row = match by_key.remove(&key) {
            Some(b) => BookRow {
                chapters: plan.len(),
                scanned_ms: Some(now_ms),
                // Merged, not assigned: this pass has no idea when anybody last
                // opened anything, and a null here leaves what the table holds.
                last_open_ms: None,
                ..b
            },
            None => {
                rep.registered += 1;
                adopt(&key, plan.len(), epubs.get(&key), now_ms)
            }
        };
        if let Err(e) = db.put_book(&row) {
            tracing::warn!("library: could not register {key}: {e}");
        }
    }
    rep.took_s = t0.elapsed().as_secs_f64();
    rep
}

// --------------------------------------------------------------- the thread

/// Start the background scanner: one OS thread, a full scan now and another
/// every [`SCAN_EVERY_S`].
///
/// A plain thread rather than a tokio task for the same reason the render worker
/// is one: a pass is thousands of blocking `stat` calls and has no business on
/// the runtime that is serving audio ranges. Called **once** at startup — this
/// does not guard against being called twice, because the one caller is the boot
/// sequence and a flag pretending otherwise would be a flag nobody tests.
pub fn start_scanner(st: &Arc<AppState>) {
    let st = st.clone();
    std::thread::Builder::new()
        .name("library".into())
        .spawn(move || scanner(st))
        .map_err(|e| tracing::error!("could not start the library scanner: {e}"))
        .ok();
}

fn scanner(st: Arc<AppState>) {
    let mut parked: Option<Instant> = None;
    let mut held: Option<Instant> = None;
    // Now, not `now + SCAN_EVERY_S`: a process that has just come up is exactly
    // the one whose table is emptiest.
    let mut due = Instant::now();
    while !st.stop.load(Ordering::SeqCst) {
        let wait = due.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait.min(SLICE));
            continue;
        }
        // Whisper outranks this, exactly as it outranks the renderer and the
        // packer. A bounded wait on the gate's condvar, so the scan starts the
        // instant the last transcription ends, and both the park and the resume
        // are logged: a background job that has quietly stopped is the shape of
        // bug this whole server is about.
        if !st.whisper.gate().wait_clear(Duration::from_millis(250)) {
            if parked.is_none() {
                parked = Some(Instant::now());
                tracing::info!("library scan parked: a voice memo is being transcribed");
            }
            continue;
        }
        if let Some(t) = parked.take() {
            tracing::info!(
                "library scan resumed after {:.1}s parked for transcription",
                t.elapsed().as_secs_f64()
            );
        }
        // And one rank below that, the renderer stalled under the playhead —
        // rule 1, the branch where somebody is waiting on this exact chunk now.
        // Bounded, because past half a minute the renderer is stuck rather than
        // busy and a library that never refreshes again is the worse answer.
        let stalled = st.stalled_for();
        if stalled > 0.0 && stalled < HOLD_MAX_S {
            if held.is_none() {
                held = Some(Instant::now());
                tracing::info!("library scan holding back: the renderer is stalled");
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if let Some(t) = held.take() {
            tracing::info!(
                "library scan resumed after {:.1}s held back for the renderer",
                t.elapsed().as_secs_f64()
            );
        }

        let rep = scan_all(&st);
        if rep.books > 0 || rep.registered > 0 {
            tracing::info!(
                "library scan: {} book(s), {} chapter(s), {} adopted, {:.2}s",
                rep.books,
                rep.chapters,
                rep.registered,
                rep.took_s
            );
        }
        due = Instant::now()
            .checked_add(Duration::from_secs_f64(
                st.cfg.library_scan_every_s.max(1.0),
            ))
            .unwrap_or_else(Instant::now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Chunk;

    fn plan(chapters: &[(&str, usize)]) -> Vec<Chapter> {
        chapters
            .iter()
            .enumerate()
            .map(|(i, (title, n))| Chapter {
                index: i,
                id: format!("c{i}.xhtml"),
                title: (*title).to_string(),
                chunks: (0..*n)
                    .map(|k| Chunk {
                        text: format!("Sentence {k}."),
                        para: 0,
                        silent: false,
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn a_scan_counts_the_wavs_that_are_actually_there() {
        let d = tempfile::tempdir().expect("tempdir");
        let cfg = Config::for_test(d.path());
        let p = plan(&[("One", 3), ("Two", 2)]);
        // Chapter 0 half rendered, chapter 1 untouched.
        let ch0 = cache::chapter_dir(&cfg.work, "K", 0);
        std::fs::create_dir_all(&ch0).expect("dir");
        for i in 0..2 {
            cache::write_silence_wav(&ch0.join(format!("{i:05}.wav")), 0.1, 1, 24_000, 2)
                .expect("wav");
        }
        let rows = scan_book(&cfg, "K", &p);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].n, rows[0].rendered), (3, 2));
        assert_eq!((rows[1].n, rows[1].rendered), (2, 0));
        assert!(!rows[0].m4a && rows[0].bytes.is_none());
        assert_eq!(rows[0].title, "One");
        assert!(rows[0].est_s.unwrap_or(0.0) > 0.0);
        // One observation, one moment.
        assert_eq!(rows[0].scanned_ms, rows[1].scanned_ms);
    }

    #[test]
    fn a_chapter_with_no_untitled_entry_is_named_the_way_the_ui_names_it() {
        let d = tempfile::tempdir().expect("tempdir");
        let cfg = Config::for_test(d.path());
        let rows = scan_book(&cfg, "K", &plan(&[("", 1)]));
        assert_eq!(rows[0].title, "Section 1");
    }

    #[test]
    fn only_a_directory_with_a_plan_counts_as_a_book() {
        let d = tempfile::tempdir().expect("tempdir");
        let work = d.path().join("work");
        let root = cache::audio_root(&work);
        std::fs::create_dir_all(root.join("Has")).expect("dir");
        std::fs::create_dir_all(root.join("HasNot")).expect("dir");
        std::fs::write(root.join("Has/plan.json"), b"[]").expect("plan");
        std::fs::write(root.join("loose.txt"), b"x").expect("file");
        assert_eq!(cached_keys(&work), vec!["Has".to_string()]);
    }

    #[test]
    fn a_plan_that_will_not_parse_is_simply_no_plan() {
        let d = tempfile::tempdir().expect("tempdir");
        let work = d.path().join("work");
        std::fs::create_dir_all(cache::book_dir(&work, "K")).expect("dir");
        assert!(read_plan(&work, "K").is_none(), "no file");
        std::fs::write(cache::plan_path(&work, "K"), b"{ not json").expect("write");
        assert!(read_plan(&work, "K").is_none(), "not json");
        std::fs::write(cache::plan_path(&work, "K"), b"[]").expect("write");
        assert!(read_plan(&work, "K").is_none(), "an empty plan is no plan");
    }

    #[test]
    fn an_adopted_book_with_no_epub_still_gets_a_row() {
        let orphan = adopt("Key", 4, None, 7);
        assert_eq!(orphan.name, "Key", "so the position lookup misses, quietly");
        assert_eq!(orphan.path, "");
        assert_eq!((orphan.chapters, orphan.scanned_ms), (4, Some(7)));
        assert_eq!(
            orphan.last_open_ms, None,
            "this pass knows nothing about it"
        );

        let p = PathBuf::from("/books/Some Title (2016).epub");
        let found = adopt("Key", 4, Some(&p), 7);
        assert_eq!(found.name, "Some Title (2016).epub");
        assert_eq!(found.title, "Some Title (2016)");
        assert_eq!(found.path, "/books/Some Title (2016).epub");
    }
}
