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
//! **Two records, and each answers a question the other cannot.**
//!
//! `work/audio/<key>/queue.json` is still the record of the book in front of
//! you, unchanged and still written on every mutation. [`crate::store`]'s
//! `intent` table in `work/state.db` now holds the same items for **every** book
//! — one row per (book, chapter), ordered by a globally monotonic `seq` that is
//! *when it was asked for*.
//!
//! The table exists because of the one question a file per book cannot answer:
//! **what does this box owe, across the whole library, in the order it was
//! promised?** A standing order on a book the session is not holding lives in a
//! file nothing is currently reading, in a directory nothing is currently
//! listing; it may as well not be there. The scheduler's entire job is to see
//! all of them at once, and [`all_outstanding`] is that question.
//!
//! The file stays the one that is **read first for the loaded book**, and the
//! reason is not sentiment: it is the only one of the two that says which book
//! it is about. A row in `intent` is keyed by cache key and nothing else, so it
//! believes whatever directory name it finds itself under; the file carries the
//! book path and the key *inside* it and is checked against both on the way in,
//! which is the one guard there is against a work directory restored from
//! another box. Reading the table first would walk straight past it. Three more
//! reasons the file is not retired, none of them the deciding one but all of
//! them real:
//!
//! - It is the **rollback**. `state.db` is new; the binary that predates it is
//!   one `docker run` away and reads only the file. Deleting the file would make
//!   going back cost an overnight download, which is precisely the thing this
//!   module exists to protect.
//! - It is what an **operator can `cat`**. A table needs a client; a file over
//!   ssh needs nothing, and "what does the box think it owes me" is a question
//!   Fernando asks at the console.
//! - It **travels with the audio**. `work/audio/<key>` copied to another box
//!   carries the order along with the chunks it is about, where one global
//!   database would not.
//!
//! So: **the file if it is there, the table if it is not.** A file that is there
//! and will not parse is a *damaged record*, not a missing one, and it costs the
//! list exactly as it always has — the table is then made to agree with it
//! rather than consulted as a second opinion, because the two were written from
//! the same snapshot and a table row the file no longer backs is a row about
//! nothing. A file that is genuinely **absent** is the case the table rescues,
//! and it is not hypothetical: a `work/` restored without its audio, a tidy-up,
//! a bad merge of a backup.
//!
//! **The two cannot drift, because they are written from one snapshot.** [`save`]
//! builds exactly one [`Saved`] out of the session's queues and hands the same
//! `items` to the file and to the table. There is no second derivation to get
//! wrong, and anything that is true of the file — a cancel landing, an item
//! leaving the moment the worker is done with it — is true of the table for the
//! same reason and at the same instant. The file is written *first*, because it
//! is the one that is read first: if only one of the two lands, the next boot
//! reads the file and [`sync`] drags the table back into line with it, which
//! heals. The other order does not — a stale file that wins the read would drag
//! a *correct* table back to stale.
//!
//! **Parked is not a column.** The store keeps `tries`; parked is
//! `tries >= MAX_ATTEMPTS`, derived on the way out. A second field would be a
//! second thing to keep in step with the first, and it would be the field that
//! decides whether the box spends the week on one chapter. `Store::add_intent`
//! resets `tries` for the same reason [`asked`] clears the count — asking again
//! *is* the retry — so the two agree by construction rather than by agreement.
//!
//! **The file the running box already has is adopted, not thrown away.** The A1
//! is holding orders in `queue.json` right now and has never had a row in
//! `intent`. The first time this process reads a book's file ([`resume`] at boot,
//! [`adopt`] on a load) the table is made to match it — order, pack flags and
//! attempt counts — and the file is then **left exactly where it is**. Not
//! deleted: see the rollback above.
//!
//! **Crash safety is the rename, and the transaction.** Every file write is a
//! `.part`, fsynced, renamed over the real name, with the directory fsynced after
//! — the pattern the chunk cache and the note queue already use. A `kill -9` at
//! any instant leaves the old file or the new one, never a half of either, and
//! that matters more here than it looks: a truncated file is a JSON error, and a
//! JSON error is handled by throwing the wishlist away, which is exactly the
//! hours of work this module exists to keep. The table gets the same property
//! from sqlite for free. A write that fails outright, either of them, is a
//! `warn!` and nothing more — the in-memory queue is untouched, the worker
//! renders on, and the only thing lost is the ability to survive the *next*
//! restart.
//!
//! **And the store is optional.** [`AppState::store`](crate::state::AppState)
//! is a `None` on a work directory gone read-only or a `state.db` that is not a
//! database, and every path here falls back to precisely the behaviour that
//! shipped before the table existed: the file is the whole record, and
//! [`all_outstanding`] can only speak for the book that is loaded, which is all
//! a file-per-book world was ever able to know.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::{AppState, Session};
use crate::store::{IntentRow, Store};

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
    // The file first, then the table. Both are written from this one `doc`, so
    // they cannot disagree about what was asked for; the order decides only
    // which is right if the process dies between them, and it has to be the one
    // that gets read first — a file that landed alone is healed by the next
    // boot's `sync`, a table that landed alone is dragged back to the stale file.
    write(&path(&st.cfg.work, &doc.key), &doc);
    store_sync(st, &doc.key, &doc.items);
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

// --------------------------------------------------------------- the store

/// The `device` column of an `intent` row, as written from here.
///
/// Empty, which is the store's own word for "a caller that did not say". It is
/// not a gap to be filled in later by this module: the session's queues are one
/// list per book with no room for who asked, so anything put here would be the
/// device that happened to touch it last rather than the one that wanted it.
/// `/api/chapters` is where a request knows the device, and that is where a name
/// would have to come from.
const NO_DEVICE: &str = "";

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// An `intent` row as this module's [`Item`].
///
/// `parked` is derived rather than stored — see the module doc. `tries` can
/// exceed [`MAX_ATTEMPTS`] only if something outside this module wrote it, and
/// `>=` reads that as parked, which is the safe direction: not tried is
/// recoverable by asking again, tried forever is the failure being bounded.
fn item_of(r: IntentRow) -> Item {
    Item {
        chapter: r.chapter,
        pack: r.pack,
        attempts: r.tries,
        parked: r.tries >= MAX_ATTEMPTS,
    }
}

/// Make the table say what `items` says, for this book and no other.
///
/// Scoped to one key on purpose: a save is about the loaded book, and a standing
/// order on some other book is not this snapshot's to have an opinion about.
/// That is what makes a `/api/load` of a second book harmless to the first one's
/// overnight download.
fn store_sync(st: &AppState, key: &str, items: &[Item]) {
    let Some(db) = st.store() else {
        // No database: the file is the record, exactly as it was before there
        // was a table. Nothing to log — this is a state the server is allowed to
        // run in, and it already said so once at boot.
        return;
    };
    sync(db, key, items);
}

/// The diff itself, against a store the caller has already got hold of.
///
/// Add and drop rather than rewrite, because `seq` **is** the order asked and
/// re-inserting a row would move a chapter somebody asked for last night to the
/// back of tonight's queue. For the same reason a chapter that is already on the
/// list is left completely alone: its place, its `created_ms` and its poison
/// count are all facts about the original ask.
fn sync(db: &Store, key: &str, items: &[Item]) {
    let existing = match db.intents(key) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("wishlist: could not read the standing order for {key:?}: {e}");
            return;
        }
    };
    if items.is_empty() {
        // Cancel-all, or a queue that has been worked through. One statement
        // rather than a delete per row.
        if !existing.is_empty() {
            if let Err(e) = db.drop_intents_for_book(key) {
                tracing::warn!("wishlist: could not clear the standing order for {key:?}: {e}");
            }
        }
        return;
    }
    for row in &existing {
        if !items.iter().any(|i| i.chapter == row.chapter) {
            if let Err(e) = db.drop_intent(key, row.chapter) {
                tracing::warn!(
                    "wishlist: could not drop chapter {} of {key:?}: {e}",
                    row.chapter
                );
            }
        }
    }
    let now = now_ms();
    // New chapters go in in snapshot order, and a run of them sharing a pack
    // flag goes in one call — which is one transaction and one pass of `seq` for
    // the ordinary case, "download these seventy-four".
    let mut run: Vec<usize> = Vec::new();
    let mut run_pack = false;
    let mut fresh: Vec<&Item> = Vec::new();
    for it in items {
        match existing.iter().find(|r| r.chapter == it.chapter) {
            Some(row) => {
                flush(db, key, &mut run, run_pack, now);
                if it.pack && !row.pack {
                    // Somebody asked for the download of a chapter that was only
                    // queued to render. That re-ask resets `tries`, which is
                    // correct and is the same rule [`asked`] follows: a person
                    // asking again is the retry.
                    if let Err(e) = db.add_intent(key, &[it.chapter], NO_DEVICE, true, now) {
                        tracing::warn!(
                            "wishlist: could not mark chapter {} of {key:?} for packing: {e}",
                            it.chapter
                        );
                    }
                }
            }
            None => {
                if !run.is_empty() && run_pack != it.pack {
                    flush(db, key, &mut run, run_pack, now);
                }
                run_pack = it.pack;
                run.push(it.chapter);
                fresh.push(it);
            }
        }
    }
    flush(db, key, &mut run, run_pack, now);
    // A row is born with `tries` at zero, so a chapter that arrives already
    // carrying a count — an adopted `queue.json`, a parked item coming back —
    // has to be walked up to it. Bounded by [`MAX_ATTEMPTS`], and only ever on a
    // row that did not exist a moment ago.
    for it in fresh {
        let want = if it.parked {
            it.attempts.max(MAX_ATTEMPTS)
        } else {
            it.attempts
        };
        for _ in 0..want {
            if let Err(e) = db.bump_tries(key, it.chapter) {
                tracing::warn!(
                    "wishlist: could not restore the attempt count of chapter {} of {key:?}: {e}",
                    it.chapter
                );
                break;
            }
        }
    }
}

fn flush(db: &Store, key: &str, run: &mut Vec<usize>, pack: bool, now: i64) {
    if run.is_empty() {
        return;
    }
    if let Err(e) = db.add_intent(key, run, NO_DEVICE, pack, now) {
        tracing::warn!(
            "wishlist: could not record {} chapter(s) of {key:?}: {e}",
            run.len()
        );
    }
    run.clear();
}

/// The standing order for one book: the file if it is there, the table if it is
/// not.
///
/// The asymmetry is deliberate and it is the whole of the module doc's "two
/// records" section in one function.
///
/// **The file is present.** It is this book's record, whatever state it is in.
/// It is read with every check it has always been read with — version, and the
/// book and key written inside it, which is the only thing that catches a work
/// directory restored from another box — and a file that fails any of them is a
/// *damaged* record and yields nothing, exactly as it did before there was a
/// table. Either way the table is then made to agree with what the file said:
/// that one call is the adoption of a file this binary has never seen, the
/// steady-state no-op when the two already match, and the tidying away of rows
/// whose file has just been declared damaged. Rows the file no longer backs are
/// rows about nothing, and leaving them would have the scheduler chasing an
/// order that this boot has already decided it does not have.
///
/// **The file is absent.** That is the case the table is here for — a `work/`
/// restored without its audio, a tidy-up, a bad merge of a backup — and the
/// rows are read back as they stand. Absent is a different claim from damaged,
/// and this is the one place in the module where the difference is worth a
/// branch.
fn load_items(st: &AppState, key: &str) -> Vec<Item> {
    let p = path(&st.cfg.work, key);
    if p.exists() {
        let items = read(&st.cfg.work, key).map(|d| d.items).unwrap_or_default();
        if let Some(db) = st.store() {
            let had = db.intents(key).map(|r| r.len()).unwrap_or(0);
            sync(db, key, &items);
            if had == 0 && !items.is_empty() {
                tracing::info!(
                    "wishlist: adopted {} chapter(s) for {key:?} out of {}, which stays where it \
                     is",
                    items.len(),
                    p.display()
                );
            }
        }
        return items;
    }
    let Some(db) = st.store() else {
        return Vec::new();
    };
    match db.intents(key) {
        Ok(rows) => {
            if !rows.is_empty() {
                tracing::info!(
                    "wishlist: {} has no {}; taking its {} outstanding chapter(s) from state.db",
                    key,
                    p.display(),
                    rows.len()
                );
            }
            rows.into_iter().map(item_of).collect()
        }
        Err(e) => {
            tracing::warn!("wishlist: could not read the standing order for {key:?}: {e}");
            Vec::new()
        }
    }
}

/// Every outstanding order in the library, oldest ask first.
///
/// One entry per book that owes something, the books in the order their oldest
/// outstanding chapter was asked for and each book's chapters in the order
/// *they* were asked for — so the last item of the last book is the newest thing
/// anybody wanted. Parked chapters are included and say so: they are still
/// owed, and whether to give one another go is the scheduler's call rather than
/// this module's.
///
/// **Without a store it can only speak for the loaded book**, and it says so by
/// returning just that one entry (or nothing at all). That is not a degraded
/// answer to the cross-library question, it is the only answer a file-per-book
/// arrangement can give: the other books' files exist, but nothing has read
/// them and nothing knows which books to go looking for. A caller that needs the
/// whole picture needs the store.
pub fn all_outstanding(st: &AppState) -> Vec<(String, Vec<Item>)> {
    if let Some(db) = st.store() {
        match db.all_intents() {
            Ok(rows) => {
                let mut out: Vec<(String, Vec<Item>)> = Vec::new();
                for r in rows {
                    let book = r.book.clone();
                    match out.iter_mut().find(|(k, _)| *k == book) {
                        Some((_, items)) => items.push(item_of(r)),
                        None => out.push((book, vec![item_of(r)])),
                    }
                }
                return out;
            }
            Err(e) => {
                tracing::warn!("wishlist: could not read the library's standing orders: {e}");
            }
        }
    }
    let w = st.wishlist();
    let s = st.session();
    match snapshot(&s, &w) {
        Some(doc) if !doc.items.is_empty() => vec![(doc.key, doc.items)],
        _ => Vec::new(),
    }
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
    // The same clearing, in the table. `Store::add_intent` would do it for a
    // chapter that is new to the list, but most of these are not new — a
    // reconciler re-places the standing order every two minutes — and a count
    // left behind in a row [`sync`] correctly declines to touch is a chapter
    // that comes back parked after the person asking has already un-parked it.
    reset_tries(st, chapters);
    save(st);
}

/// Put these chapters' poison counts back to zero in the store.
///
/// A chapter with no row is a no-op, which is right at every call site: nothing
/// was asked for, so nothing has failed.
fn reset_tries(st: &AppState, chapters: &[usize]) {
    let Some(db) = st.store() else { return };
    let Some(key) = st.session().key() else {
        return;
    };
    for c in chapters {
        if let Err(e) = db.reset_tries(&key, *c) {
            tracing::warn!("wishlist: could not clear chapter {c}'s attempt count: {e}");
        }
    }
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

/// A chunk of chapter `chapter` of book `key` landed on disk.
///
/// Which is the only evidence that matters for the poison counter: an item that
/// is producing audio is not the item the parking rule is about, however many
/// restarts it has lived through. Costs a map lookup in the common case and only
/// writes when there was actually a count to clear.
///
/// **Keyed by book, because the counts are.** [`Wishlist`]'s map is the loaded
/// book's and holds bare chapter numbers, and the worker renders other books'
/// chapters too — a standing order, the speculative branch. Chapter 4 of some
/// other novel landing used to clear chapter 4 of this one's count, which is a
/// wedged chapter that can never be parked. A foreign book's count is not in
/// that map at all (only a boot's [`resume`] counts, and only for the loaded
/// book), so there is nothing to clear for one and nothing is touched.
pub fn progress(st: &AppState, key: &str, chapter: usize) {
    if st.session().key().as_deref() != Some(key) {
        return;
    }
    {
        let mut w = st.wishlist();
        if w.attempts.remove(&chapter).is_none() {
            return;
        }
    }
    // Only when there was a count to clear, which is why the store is touched
    // here rather than on every chunk: the two records are kept in step, so an
    // in-memory count of zero is a table row of zero and the early return above
    // is not skipping a write, it is skipping a write that would change nothing.
    reset_tries(st, &[chapter]);
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
    let items = load_items(st, &key);
    if items.is_empty() {
        // No wishlist for this book, or one that cannot be trusted. Either way
        // what is remembered about the last book must not follow it here.
        st.wishlist().forget_all();
        return Vec::new();
    }
    remember(st, &items);
    apply(st, &items)
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
    let mut items = load_items(st, &key);
    if items.is_empty() {
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
    for it in &mut items {
        if it.parked {
            out.parked.push(it.chapter);
            continue;
        }
        if !unfinished(it.chapter) {
            out.queued.push(it.chapter);
            continue;
        }
        it.attempts = it.attempts.saturating_add(1);
        // The table counts the same restart, here rather than in [`save`],
        // because `sync` deliberately never touches an existing row's count —
        // it cannot tell a bump from the count it already holds. This is the one
        // place a restart is being counted, so it is the one place that says so
        // to both records.
        if let Some(db) = st.store() {
            if let Err(e) = db.bump_tries(&key, it.chapter) {
                tracing::warn!(
                    "wishlist: could not count the restart against chapter {}: {e}",
                    it.chapter
                );
            }
        }
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
    remember(st, &items);
    apply(st, &items);
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
        let key = st.session().key_or_x();
        progress(&st, &key, 6);
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
