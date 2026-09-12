//! The wishlist: the chapters someone asked for, kept across restarts.
//!
//! **The gap this fills.** `/api/chapters/render` and `/api/chapters/build` put
//! chapter numbers into three lists in memory — [`Session::queue`],
//! [`Session::build_want`], [`Session::pack_queue`] — and nowhere else. One tap
//! in the chapter drawer can ask for 74 chapters, which on the A1 at a quarter
//! of realtime is an overnight job, and this box restarts on a deploy, on the
//! watchdog's third failed probe, on a `docker pull`. Every one of those threw
//! the whole list away in silence: the worker came back with nothing to do and
//! the drawer came back with every row exactly where it had been, because a
//! queue that was abandoned looks precisely like a queue nobody ever asked for.
//!
//! **What is persisted, and what deliberately is not.** The *intent* — which
//! chapters, in the order asked, and whether each is wanted packed as well as
//! rendered. Never the progress. This server derives progress from the
//! filesystem on purpose (a chapter is rendered iff its chunk wavs are there,
//! packed iff its `chNNN.m4a` is; see the disk-truth invariant in
//! [`crate::render`]), and a second record of it would be a second truth, free
//! to go stale the first time `gc_audio` deletes a chunk behind the frontier.
//! So a resumed wishlist is nothing more than the same chapter numbers handed
//! back to the same worker, which then works out what is left the way it always
//! does — including that a chapter finished while the process was down simply
//! leaves the queue again the moment the worker looks at it.
//!
//! **One file per book, beside its plan**: `work/audio/<key>/queue.json`. A
//! wishlist is chapter *indices into one book's plan*, so it belongs to that
//! book and to nothing else; filing it under that book's own cache directory
//! makes applying it to the wrong book impossible by construction rather than by
//! remembering to check, and it keeps an overnight download alive across "let me
//! look at something else for ten minutes" — which a single global file could
//! only survive by throwing one of the two lists away. The book and the key are
//! written *inside* the file as well, and checked on the way in, because there
//! is one way the path can still lie: a work directory copied or restored from
//! another box.
//!
//! **Crash safety is the rename.** Every write is a `.part`, fsynced, renamed
//! over the real name, with the directory fsynced after — the pattern the chunk
//! cache and the note queue already use. A `kill -9` at any instant leaves the
//! old file or the new one, never a half of either, and that matters more here
//! than it looks: a truncated file is a JSON error, and a JSON error is handled
//! by throwing the wishlist away, which is exactly the hours of work this module
//! exists to keep. A write that fails outright is a `warn!` and nothing more —
//! the in-memory queue is untouched, the worker renders on, and the only thing
//! lost is the ability to survive the *next* restart.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::{AppState, Session};

/// Bumped when the meaning of the file changes rather than its contents. A file
/// from a future version is ignored, not guessed at: the wrong chapters queued
/// for a night is worse than none.
const VERSION: u32 = 1;

/// How many times a chapter may be picked back up by a restart without a single
/// chunk of it landing before it is parked.
///
/// The failure this bounds is specific and this box can actually produce it: a
/// durable queue plus a watchdog that restarts the container after three failed
/// health probes is a machine for retrying a job forever. A chapter whose text
/// wedges espeak-ng renders nothing, the renderer looks stalled, the watchdog
/// restarts it, the queue comes back, and the box spends the rest of the week on
/// one chapter. Five is chosen the way [`crate::api::notes`]'s `MAX_RESUMES` is:
/// high enough that ordinary restarts — a deploy in the middle of a download —
/// never reach it, low enough to stop a loop inside an evening.
///
/// It is not giving up. The item stays in the file, says so in `/api/chapters`,
/// is named in the log at every boot, and asking for the chapter again clears
/// the count and starts it over.
pub const MAX_ATTEMPTS: u32 = 5;

/// One chapter on the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub chapter: usize,
    /// Pack it once it is rendered — what "download" asks for and "render" does
    /// not.
    #[serde(default)]
    pub pack: bool,
    /// Restarts this chapter has been picked back up by without rendering
    /// anything. Reset by the first chunk that lands, so a long download that
    /// survives six deploys carries no count at all.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    /// Out of attempts: still wanted, no longer tried.
    #[serde(default, skip_serializing_if = "is_false")]
    pub parked: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The file. Pretty-printed, with the book spelled out: Fernando reads this one
/// over ssh when he wants to know what the box thinks it owes him.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Saved {
    version: u32,
    book: String,
    key: String,
    updated: String,
    items: Vec<Item>,
}

/// What a resume or an adoption did, for the caller to log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resumed {
    /// Chapters put back into the queue, in the order they were asked for.
    pub queued: Vec<usize>,
    /// Chapters that have run out of attempts and were left alone.
    pub parked: Vec<usize>,
}

/// The half of the wishlist that is not already in [`Session`].
///
/// The queues themselves stay exactly where they were — this is bookkeeping
/// *about* them, and keeping it separate is what stops the poison counter
/// becoming another thing every handler has to remember to update.
#[derive(Debug, Default)]
pub struct Wishlist {
    /// Non-zero only for chapters a restart picked back up that have rendered
    /// nothing since.
    attempts: BTreeMap<usize, u32>,
    /// Parked chapters, and whether each wanted packing — kept so that asking
    /// for one again restores what was originally asked for.
    parked: BTreeMap<usize, bool>,
}

impl Wishlist {
    pub fn is_parked(&self, ci: usize) -> bool {
        self.parked.contains_key(&ci)
    }

    pub fn parked(&self) -> Vec<usize> {
        self.parked.keys().copied().collect()
    }

    pub fn attempts(&self, ci: usize) -> u32 {
        self.attempts.get(&ci).copied().unwrap_or(0)
    }

    /// Forget everything about a book — what an `/api/load` of another one does.
    fn forget_all(&mut self) {
        self.attempts.clear();
        self.parked.clear();
    }

    /// Forget these chapters: they were cancelled, or asked for again, and
    /// either way their history is no longer about anything.
    fn forget(&mut self, chapters: &[usize]) {
        for c in chapters {
            self.attempts.remove(c);
            self.parked.remove(c);
        }
    }
}

pub fn path(work: &Path, key: &str) -> PathBuf {
    crate::cache::book_dir(work, key).join("queue.json")
}

// ------------------------------------------------------------------- writing

/// Write the wishlist as it now stands.
///
/// Called after every change to the queues, including the worker's own: an item
/// the worker has finished with must leave the file promptly, or a restart two
/// seconds later re-renders a chapter that is already on disk (harmless, but it
/// would also un-cancel a cancelled one, which is not).
///
/// Locks are always taken wishlist-then-session, here and everywhere else in
/// this module. Two saves racing could otherwise take their snapshots in one
/// order and reach the file in the other, leaving the older of the two on disk.
pub fn save(st: &AppState) {
    let w = st.wishlist();
    let doc = {
        let s = st.session();
        snapshot(&s, &w)
    };
    let Some(doc) = doc else {
        // Nothing is loaded, so there is no book whose wishlist this could be.
        return;
    };
    write(&path(&st.cfg.work, &doc.key), &doc);
}

fn snapshot(s: &Session, w: &Wishlist) -> Option<Saved> {
    let book = s.book.clone()?;
    let key = s.key()?;
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    // Queue order first, because it is the order Fernando asked in and the order
    // the worker will honour. The other two lists only add chapters the queue
    // has already let go of: one that is complete and waiting for the packer,
    // one the packer is on right now.
    for c in &s.queue {
        push(
            &mut items,
            &mut seen,
            w,
            *c,
            s.build_want.contains(c),
            false,
        );
    }
    for c in &s.pack_queue {
        push(&mut items, &mut seen, w, *c, true, false);
    }
    for c in &s.build_want {
        push(&mut items, &mut seen, w, *c, true, false);
    }
    for (c, pack) in &w.parked {
        push(&mut items, &mut seen, w, *c, *pack, true);
    }
    Some(Saved {
        version: VERSION,
        book,
        key,
        updated: chrono::Local::now().to_rfc3339(),
        items,
    })
}

fn push(
    items: &mut Vec<Item>,
    seen: &mut HashSet<usize>,
    w: &Wishlist,
    chapter: usize,
    pack: bool,
    parked: bool,
) {
    if !seen.insert(chapter) {
        return;
    }
    items.push(Item {
        chapter,
        pack,
        attempts: w.attempts(chapter),
        parked,
    });
}

fn write(path: &Path, doc: &Saved) {
    let body = match serde_json::to_vec_pretty(doc) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("wishlist: could not encode the queue: {e}");
            return;
        }
    };
    if let Err(e) = write_durable(path, &body) {
        // Never the caller's problem: a queue that cannot be written down is a
        // queue that still renders, and failing the request instead would turn a
        // full disk into a reader that cannot ask for chapters at all.
        tracing::warn!("wishlist: could not write {}: {e}", path.display());
    }
}

/// `.part`, fsync, rename, fsync the directory.
///
/// The same shape as the chunk cache's write and the note queue's, and for the
/// same reason: the rename is the atom, so a kill at any instant leaves one
/// whole file or the other. Deliberately a local copy — this module is below the
/// API layer and has no business reaching up into it for eighteen lines.
fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let part = parent.join("queue.json.part");
    {
        let mut f = std::fs::File::create(&part)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&part, path)?;
    // Best effort: a filesystem that will not open a directory for this is not a
    // reason to report a write that has already landed as failed.
    if let Ok(d) = std::fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

// ------------------------------------------------------------------- reading

fn read(work: &Path, key: &str) -> Option<Saved> {
    let p = path(work, key);
    let raw = std::fs::read(&p).ok()?;
    let doc: Saved = match serde_json::from_slice(&raw) {
        Ok(d) => d,
        Err(e) => {
            // Truncated by a disk that filled, hand-edited into something that
            // is not JSON, written by a version that meant something else: a
            // wishlist that will not parse is treated as no wishlist. The cost
            // is a list of chapter numbers; the alternative — a boot that fails
            // on it — costs the book.
            tracing::warn!("wishlist: unreadable {}: {e}", p.display());
            return None;
        }
    };
    if doc.version != VERSION {
        tracing::warn!(
            "wishlist: {} is version {}, not {VERSION}; ignoring it",
            p.display(),
            doc.version
        );
        return None;
    }
    if doc.key != key {
        // The path already names the book. This catches the one case where it
        // can still be wrong: a work directory copied from somewhere else, where
        // the directory name survived and the contents are another book's.
        tracing::warn!(
            "wishlist: {} says it belongs to {:?}, not {:?}; ignoring it",
            p.display(),
            doc.key,
            key
        );
        return None;
    }
    Some(doc)
}

/// Put the file's chapters into the session's queues, in the order given.
///
/// Everything goes into `queue`, including a chapter that only wants packing:
/// whether it still needs rendering is a question about the filesystem, and the
/// worker's `next_queued` already asks it — a complete chapter leaves the queue
/// and goes to the packer the first time the worker looks at it. Duplicating
/// that decision here would be a second implementation of the disk-truth rule,
/// which is the one thing this crate exists not to have.
fn apply(st: &AppState, items: &[Item]) -> Vec<usize> {
    let mut s = st.session();
    let n = s.plan.len();
    let mut taken = Vec::new();
    for it in items {
        if it.parked {
            continue;
        }
        if it.chapter >= n {
            // The book has fewer chapters than it did. Nothing sensible can be
            // done with the index, and it is dropped on the next save.
            tracing::warn!(
                "wishlist: chapter {} is past the end of this book; dropping it",
                it.chapter
            );
            continue;
        }
        if !s.queue.contains(&it.chapter) {
            s.queue.push(it.chapter);
        }
        if it.pack {
            s.build_want.insert(it.chapter);
        }
        taken.push(it.chapter);
    }
    taken
}

fn remember(st: &AppState, items: &[Item]) {
    let mut w = st.wishlist();
    w.forget_all();
    for it in items {
        if it.parked {
            w.parked.insert(it.chapter, it.pack);
        } else if it.attempts > 0 {
            w.attempts.insert(it.chapter, it.attempts);
        }
    }
}

// ------------------------------------------------------------------ the verbs

/// A client asked for these chapters: clear whatever this module remembers about
/// them and write the list down.
///
/// Asking again is the retry, which is why it also un-parks: the button in the
/// drawer is the only thing that should be able to overrule five failed
/// attempts, and it does not need a new endpoint to do it.
pub fn asked(st: &AppState, chapters: &[usize]) {
    st.wishlist().forget(chapters);
    save(st);
}

/// These chapters were cancelled — all of them, if `chapters` is None.
///
/// The file has to be rewritten here rather than merely left alone: it is what
/// the next boot believes, and a cancel that only happened in memory would be
/// undone by the next restart, which is the same bug in the other direction.
pub fn cancelled(st: &AppState, chapters: Option<&[usize]>) {
    {
        let mut w = st.wishlist();
        match chapters {
            None => w.forget_all(),
            Some(cs) => w.forget(cs),
        }
    }
    save(st);
}

/// A chunk of this chapter landed on disk.
///
/// Which is the only evidence that matters for the poison counter: an item that
/// is producing audio is not the item the parking rule is about, however many
/// restarts it has lived through. Costs a map lookup in the common case and only
/// writes when there was actually a count to clear.
pub fn progress(st: &AppState, chapter: usize) {
    {
        let mut w = st.wishlist();
        if w.attempts.remove(&chapter).is_none() {
            return;
        }
    }
    save(st);
}

/// Pick the wishlist back up for the book the session has just loaded.
///
/// Called by `/api/load`, and the reason it exists is that loading a book
/// *clears* the queues: the reader re-opens the book it was on when the app
/// starts, and without this, a download resumed by a restart would be wiped
/// thirty seconds later by a client doing something entirely routine. It counts
/// no attempt — a load is a person asking, not an unattended retry — and it does
/// not un-park anything.
pub fn adopt(st: &Arc<AppState>) -> Vec<usize> {
    let Some(key) = st.session().key() else {
        return Vec::new();
    };
    let Some(doc) = read(&st.cfg.work, &key) else {
        // No wishlist for this book, or one that cannot be trusted. Either way
        // what is remembered about the last book must not follow it here.
        st.wishlist().forget_all();
        return Vec::new();
    };
    remember(st, &doc.items);
    apply(st, &doc.items)
}

/// Pick the wishlist back up at startup, and count the attempt.
///
/// Called from [`crate::boot`], after the session has been restored — with no
/// book there is nothing a wishlist could safely be applied to, which is
/// requirement enough to make the order load-bearing.
///
/// The incremented counts are written down **before** any work starts, exactly
/// as the note queue's `resuming` does. A crash that happens during the work
/// would otherwise never be counted, and a crash during the work is precisely
/// the case the counter exists for.
pub fn resume(st: &Arc<AppState>) -> Option<Resumed> {
    let (plan, key) = {
        let s = st.session();
        (s.plan.clone(), s.key()?)
    };
    let mut doc = read(&st.cfg.work, &key)?;
    if doc.items.is_empty() {
        return None;
    }
    // Whether a chapter still has rendering left in it, asked of the filesystem
    // rather than of a number this module kept — one `read_dir` per item, which
    // is what `/api/chapters` does a thousand of in 25 ms on the A1. It decides
    // only whether the restart counts *against* the item: a chapter whose chunks
    // are all there has nothing left that can fail (what it may still be owed is
    // a pack, and a pack that fails leaves the queues rather than looping), so
    // counting a restart against it would park work that is finished.
    let unfinished = |ci: usize| -> bool {
        let Some(n) = plan.get(ci).map(|c| c.chunks.len()) else {
            return true;
        };
        let dir = crate::cache::chapter_dir(&st.cfg.work, &key, ci);
        n > 0 && crate::cache::rendered_count(&dir, n) < n
    };
    let mut out = Resumed::default();
    for it in &mut doc.items {
        if it.parked {
            out.parked.push(it.chapter);
            continue;
        }
        if !unfinished(it.chapter) {
            out.queued.push(it.chapter);
            continue;
        }
        it.attempts = it.attempts.saturating_add(1);
        if it.attempts >= MAX_ATTEMPTS {
            it.parked = true;
            out.parked.push(it.chapter);
            tracing::error!(
                "wishlist: chapter {} has been resumed {} times without rendering a chunk — \
                 parking it. It stays in {} and asking for it again starts it over.",
                it.chapter,
                it.attempts,
                path(&st.cfg.work, &key).display()
            );
        } else {
            out.queued.push(it.chapter);
        }
    }
    remember(st, &doc.items);
    apply(st, &doc.items);
    save(st);
    Some(out)
}

/// How long after startup the worker is allowed to act on a resumed wishlist.
///
/// Not zero, and the reason is the box rather than the code. The A1 has two
/// cores; a voice memo is the one thing on it that cannot be made again, and
/// `/api/note`'s startup sweep may be about to claim one of those cores for a
/// memo the last process was in the middle of. Coming up rendering in the same
/// instant would have them fight for the machine on exactly the boot where
/// something already went wrong. The gap costs a few seconds of an overnight job
/// and buys a startup that is never the busiest moment of the day.
///
/// It is a delay, not a throttle: once the worker is awake, resumed chapters are
/// ordinary queue entries, rendered one chunk at a time on one thread, parking
/// for whisper between chunks like everything else.
pub const RESUME_DELAY_S: f64 = 10.0;

/// Wake the render worker on resumed work, after [`RESUME_DELAY_S`].
///
/// Gated on the worker not already being alive, which is the same rule
/// `/api/playhead` uses and for the same reason: if something has started it in
/// the meantime, that something knows more than this does, and if someone has
/// explicitly stopped it with `/api/renderer {"on": false}`, a resume from ten
/// seconds ago must not quietly turn it back on.
pub fn start_soon(st: &Arc<AppState>, chapters: usize) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // `narrator --openapi` and `narrator export`: no runtime, nothing to
        // resume onto, and nothing lost by not trying.
        tracing::debug!("wishlist: no runtime; the queue waits for a client to say something");
        return;
    };
    let delay = st.cfg.queue_resume_delay_s.max(0.0);
    let st = st.clone();
    handle.spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        if st.stop.load(Ordering::SeqCst) || crate::render::render_alive(&st) {
            return;
        }
        tracing::info!("wishlist: taking up {chapters} chapter(s) left over from the last process");
        st.run.set();
        crate::render::ensure_render_thread(&st);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::{Chapter, Chunk};
    use crate::config::Config;

    fn state(root: &Path) -> Arc<AppState> {
        let st = AppState::new(Config::for_test(root));
        {
            let mut s = st.session();
            s.book = Some("/books/A Book (2016).epub".into());
            s.plan = Arc::new(
                (0..10)
                    .map(|i| Chapter {
                        index: i,
                        id: format!("c{i}.xhtml"),
                        title: format!("Chapter {i}"),
                        chunks: vec![Chunk {
                            text: "hello".into(),
                            para: 0,
                            silent: false,
                        }],
                    })
                    .collect(),
            );
        }
        st
    }

    fn items(st: &AppState) -> Vec<Item> {
        let key = st.session().key_or_x();
        let raw = std::fs::read(path(&st.cfg.work, &key)).expect("queue.json");
        serde_json::from_slice::<Saved>(&raw).expect("parse").items
    }

    #[test]
    fn the_file_carries_the_order_asked_and_the_pack_flag() {
        let d = tempfile::tempdir().expect("tempdir");
        let st = state(d.path());
        {
            let mut s = st.session();
            s.queue = vec![7, 3, 4];
            s.build_want.insert(3);
        }
        save(&st);
        assert_eq!(
            items(&st)
                .iter()
                .map(|i| (i.chapter, i.pack))
                .collect::<Vec<_>>(),
            vec![(7, false), (3, true), (4, false)]
        );
    }

    #[test]
    fn a_chapter_waiting_for_the_packer_is_on_the_list_too() {
        let d = tempfile::tempdir().expect("tempdir");
        let st = state(d.path());
        // What `/api/chapters/build` does with a chapter that is already
        // rendered: straight to the packer, never through the queue.
        st.session().pack_queue.push(2);
        save(&st);
        assert_eq!(
            items(&st),
            vec![Item {
                chapter: 2,
                pack: true,
                attempts: 0,
                parked: false,
            }]
        );
    }

    #[test]
    fn nothing_is_written_for_a_book_that_is_not_loaded() {
        let d = tempfile::tempdir().expect("tempdir");
        let st = AppState::new(Config::for_test(d.path()));
        st.session().queue.push(1);
        save(&st);
        assert!(!path(&st.cfg.work, "x").exists());
    }

    #[test]
    fn a_file_that_will_not_parse_is_no_file_at_all() {
        let d = tempfile::tempdir().expect("tempdir");
        let st = state(d.path());
        let key = st.session().key_or_x();
        let p = path(&st.cfg.work, &key);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(&p, b"{\"version\": 1, \"items\": [{\"chapter\"").expect("truncate");
        assert!(resume(&st).is_none());
        assert!(st.session().queue.is_empty());
    }

    #[test]
    fn a_file_that_names_another_book_is_ignored() {
        let d = tempfile::tempdir().expect("tempdir");
        let st = state(d.path());
        let key = st.session().key_or_x();
        let doc = Saved {
            version: VERSION,
            book: "/books/Something Else (2011).epub".into(),
            key: "Something Else (2011)".into(),
            updated: "2026-09-11T22:00:00+02:00".into(),
            items: vec![Item {
                chapter: 5,
                pack: true,
                attempts: 0,
                parked: false,
            }],
        };
        write(&path(&st.cfg.work, &key), &doc);
        assert!(resume(&st).is_none());
        assert!(st.session().queue.is_empty(), "no work on the wrong book");
    }

    #[test]
    fn an_attempt_is_counted_per_resume_and_parks_at_the_limit() {
        let d = tempfile::tempdir().expect("tempdir");
        // What the process that took the request left behind.
        {
            let st = state(d.path());
            st.session().queue.push(4);
            save(&st);
            assert_eq!(items(&st)[0].attempts, 0);
        }
        for n in 1..MAX_ATTEMPTS {
            let st = state(d.path());
            let r = resume(&st).unwrap_or_default();
            assert_eq!(r.queued, vec![4], "still tried on restart {n}");
            assert_eq!(items(&st)[0].attempts, n);
        }
        let st = state(d.path());
        assert_eq!(resume(&st).unwrap_or_default().parked, vec![4]);
        assert!(st.session().queue.is_empty(), "parked work is not queued");
        assert!(st.wishlist().is_parked(4));
        // And it stays parked rather than costing a core on every boot after.
        let st = state(d.path());
        let r = resume(&st).unwrap_or_default();
        assert_eq!((r.queued, r.parked), (vec![], vec![4]));
    }

    #[test]
    fn a_rendered_chunk_clears_the_count_and_asking_again_unparks() {
        let d = tempfile::tempdir().expect("tempdir");
        let st = state(d.path());
        st.session().queue.push(6);
        save(&st);
        for _ in 0..(MAX_ATTEMPTS - 2) {
            resume(&st);
        }
        assert!(st.wishlist().attempts(6) > 0);
        progress(&st, 6);
        assert_eq!(st.wishlist().attempts(6), 0);
        assert_eq!(items(&st)[0].attempts, 0);

        // And the parked case: only being asked for again brings it back.
        st.wishlist().parked.insert(8, true);
        save(&st);
        assert!(items(&st).iter().any(|i| i.chapter == 8 && i.parked));
        asked(&st, &[8]);
        assert!(!st.wishlist().is_parked(8));
    }

    #[test]
    fn a_cancel_is_written_down_so_a_restart_cannot_undo_it() {
        let d = tempfile::tempdir().expect("tempdir");
        let st = state(d.path());
        {
            let mut s = st.session();
            s.queue = vec![1, 2, 3];
            s.build_want.insert(2);
        }
        save(&st);
        {
            let mut s = st.session();
            s.queue.retain(|c| *c != 2);
            s.build_want.remove(&2);
        }
        cancelled(&st, Some(&[2]));
        assert_eq!(
            items(&st).iter().map(|i| i.chapter).collect::<Vec<_>>(),
            vec![1, 3]
        );
        {
            let mut s = st.session();
            s.queue.clear();
            s.build_want.clear();
        }
        cancelled(&st, None);
        assert!(items(&st).is_empty());
    }
}
