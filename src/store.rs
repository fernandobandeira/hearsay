//! The state store: the durable *intent and identity* this server keeps, in one
//! SQLite file at `work/state.db`.
//!
//! **What it is for.** Everything narrator remembers today is remembered per
//! book, in a JSON file beside that book's audio — `queue.json` for the wishlist,
//! `plan.json` and its stamp for the parse cache, `session.json` for the one
//! loaded book. That shape answers every question about *the book in front of
//! you* and none about the library: which device is where, how far anything has
//! ever reached, which chapters somebody asked for on a book that is not loaded,
//! what each phone actually holds. Those are cross-cutting questions, they are
//! asked by the scheduler and by the reader's reconciler, and answering them by
//! walking a few thousand directories per request is how the python server's
//! `/api/chapters` got slow. This is the table those questions are asked of.
//!
//! # Two rules, and the first one is the whole rewrite
//!
//! **This database never holds "is this chunk or chapter rendered".** That is the
//! disk-truth invariant (see `crate::render`, and the section of the same name in
//! `AGENTS.md`), and it is the exact bug this rewrite exists to fix: the python
//! worker kept its progress in a counter, `gc_audio` deleted a chunk behind that
//! counter, and the reader waited forever on a chunk the renderer believed it had
//! already made. A row in [`chapter_index`](Store::chapter_index) that says
//! `rendered = 33` is **a cache of a filesystem scan** and nothing more: it is
//! there so `/api/chapters` can answer for a book the session has not loaded
//! without a thousand `read_dir` calls, and it is stale the instant the gc runs.
//! Nothing in the renderer or the packer may consult it to decide whether to
//! render or pack anything. Ever. This is written out at length because the next
//! person to touch this file will find a `rendered` column sitting right there
//! and be tempted, and the failure that follows is silent, slow to reproduce and
//! indistinguishable from a network problem.
//!
//! The same rule is why there is no `chunk` table at all. Disk is truth.
//!
//! **This database does not replace the vault files.**
//! `.narrator-positions.json` and `Reading Log.md` are a byte-level contract with
//! the Obsidian plugin and with the python reference implementation — two
//! programs that have never heard of this file and never will. So the direction
//! is one-way: the store is the record, the vault files are a **projection** of
//! it, written the way they have always been written.
//! [`newest_position`](Store::newest_position) exists precisely to compute that
//! projection — the vault has one record per book and the store has one per
//! device, so something has to choose, and the choice is "the most recent write,
//! ties broken by the sequence it was written in". Never the other way round: a
//! vault file edited by hand is a person's opinion about one book, not a reason
//! to forget which device said what.
//!
//! # Why SQLite, and why one connection
//!
//! Crash safety, mostly. The JSON files each buy it with a `.part`/fsync/rename
//! dance that every writer has to remember; here it is the transaction, and a
//! wholesale update like [`replace_inventory`](Store::replace_inventory) is
//! atomic without anybody thinking about it. WAL plus `synchronous=NORMAL` is the
//! right trade for a box whose worst case is losing the last few seconds of
//! *intent* — a chapter somebody asked for twice is a duplicate request, not a
//! lost book.
//!
//! One connection behind a `Mutex`, because the A1 has two cores and one of them
//! is always inside Kokoro. Every statement here touches a handful of rows in a
//! database measured in hundreds of kilobytes; a pool would be machinery in
//! service of contention that cannot happen.
//!
//! # Failure is the caller's to shrug at
//!
//! Every method returns a `Result` and nothing in here panics. A store that will
//! not open — a work directory gone read-only, a file that is not a database —
//! must degrade to "narrator without the extra answers", never to a server that
//! will not start. That is the same rule the rest of the crate follows and it is
//! the reason for the typed [`StoreError`]: a caller that logs and carries on
//! needs to be able to tell "no row" (an `Ok(None)`) from "the disk is gone" (an
//! `Err`), and `Option` alone cannot say it.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

/// The schema this build expects. The migration runner walks a database forward
/// to this number and never backwards — a downgrade is a restore from backup,
/// not a code path, because the only honest way to un-apply a migration is to
/// throw away what it added.
pub const SCHEMA_VERSION: u32 = 1;

/// `meta` key holding the applied schema version.
const K_SCHEMA: &str = "schema_version";

/// `meta` key holding the sequence counter. See [`Store::seq`].
const K_SEQ: &str = "seq";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A value the store itself wrote back that it cannot read — a hand-edited
    /// `meta` row, or a file from a version that meant something else. Its own
    /// error rather than a sqlite one, because the sqlite call succeeded.
    #[error("state.db: {0}")]
    Corrupt(String),
    /// The database is newer than this binary. Refusing is the point: a rollback
    /// that quietly wrote version-1 rows into a version-2 table would corrupt the
    /// thing the rollback was meant to rescue.
    #[error("state.db is schema version {0}, newer than this build's {SCHEMA_VERSION}")]
    FromTheFuture(u32),
}

type Result<T> = std::result::Result<T, StoreError>;

// --------------------------------------------------------------- the row types

/// A device that has ever talked to this server.
///
/// `id` is a uuid the reader mints and keeps in its own storage; the empty string
/// is the legacy client — the Obsidian plugin, a curl, any caller that predates
/// the header. It is a real device with a real position, so it gets a real row
/// rather than being dropped on the floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// Where one device is in one book.
///
/// The fields through `chapters_total` are [`crate::vault::Position`]'s, in its
/// order, because this row is what that file is projected from and a rename would
/// make the projection a translation. What is *not* shared is the timestamp:
/// the vault carries a naive local string because python wrote one, and this
/// carries epoch milliseconds because ordering two devices' writes is the whole
/// job and a string with no zone cannot do it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PositionRow {
    pub chapter: i64,
    pub chunk: i64,
    pub chapter_title: String,
    pub chunks_total: i64,
    pub chapters_total: i64,
    /// Server clock, epoch ms. The ordering between devices.
    pub updated_ms: i64,
    /// The tie-break *inside* one millisecond, stamped by
    /// [`Store::put_position`] — whatever is in this field on the way in is
    /// ignored. Two phones saving in the same millisecond is not a hypothetical
    /// on a tunnel that delivers a backlog all at once, and "whichever row the
    /// query planner felt like" is not an answer anybody can debug.
    pub seq: u64,
}

/// How far a device has *ever* reached in a book.
///
/// A separate table from `position` because they answer different questions and
/// conflating them is a bug with a name: the reader's auto-trim anchors on the
/// furthest point, and anchoring it on the current one means re-reading a scene
/// shrinks the anchor and deletes chapters that were downloaded on purpose. So
/// this only ever moves forward. See [`Store::bump_furthest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FurthestRow {
    pub chapter: i64,
    pub chunk: i64,
    pub updated_ms: i64,
}

/// One chapter somebody asked to have rendered, and usually packed.
///
/// This is the same kind of fact `work/audio/<key>/queue.json` holds and it is
/// keyed the same way — by **cache key**, not by epub file name — so the two
/// describe the same scope and a future migration between them is a copy rather
/// than a reinterpretation. Positions are keyed by file name instead, because
/// that is how `.narrator-positions.json` keys them and the projection has to be
/// a lookup rather than a rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IntentRow {
    /// Cache key ([`crate::cache::book_key`]).
    pub book: String,
    pub chapter: usize,
    /// Which device asked, or `""` for one that did not say. Kept so a download
    /// can eventually be reported back to the phone that wanted it; nothing
    /// filters on it today and nothing should have to.
    pub device: String,
    /// Pack it once rendered — what "download" asks for and a bare "render" does
    /// not.
    pub pack: bool,
    /// The order asked, globally monotonic. This is the queue order and it is a
    /// number rather than a row position because rows have no order.
    pub seq: u64,
    /// The poison counter: restarts that picked this chapter back up without a
    /// chunk of it landing. Past a threshold the caller parks it. Same bound, and
    /// the same reason, as [`crate::wishlist::MAX_ATTEMPTS`].
    pub tries: u32,
    pub created_ms: i64,
}

/// One chapter one device actually holds in Cache Storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InventoryRow {
    pub device: String,
    pub book: String,
    pub chapter: usize,
    /// What it weighs, when the device bothered to say. Null is "it is here and
    /// I did not measure it", which is a different claim from "it is not here".
    pub bytes: Option<u64>,
    pub stored_ms: i64,
}

/// A cached filesystem scan of one chapter. **Not truth** — see the module doc.
///
/// The field names are [`crate::api::chapters::ChapterRow`]'s on purpose: this
/// table exists to answer that endpoint for a book the session has not loaded,
/// and a row that has to be renamed on the way out is a row that will eventually
/// be renamed wrongly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ChapterIndexRow {
    pub chapter: usize,
    /// Chunks in the plan.
    pub n: usize,
    /// Chunk wavs that were on disk **when the scan ran**.
    pub rendered: usize,
    pub m4a: bool,
    pub bytes: Option<u64>,
    pub duration: Option<f64>,
    pub title: String,
    /// Estimated spoken seconds, from the chunker's character counts.
    pub est_s: Option<f64>,
    pub scanned_ms: i64,
}

/// A book this server knows about.
///
/// Both names are kept because both are load-bearing and neither derives from
/// the other safely: `key` is what the cache directory is called and what
/// `intent` and `chapter_index` are keyed by, `name` is the epub file name and
/// what `.narrator-positions.json` is keyed by. `book_key` truncates at fifty
/// characters, so going from `name` to `key` is easy and going back is a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BookRow {
    pub key: String,
    pub name: String,
    pub path: String,
    pub title: String,
    pub chapters: usize,
    /// What the scheduler means by "the most recent book". Null for one that has
    /// been seen on disk but never opened — which is why
    /// [`Store::recent_books`] has to order nulls last explicitly.
    pub last_open_ms: Option<i64>,
    pub scanned_ms: Option<i64>,
}

// ------------------------------------------------------------- pure helpers

/// Strictly further, lexicographically on (chapter, chunk).
///
/// Strict is the whole point: a device re-saving where it already is must not
/// restamp the furthest row, or "how long since anything moved" stops meaning
/// anything.
fn further(new: (i64, i64), old: (i64, i64)) -> bool {
    new > old
}

/// The inclusive, clamped bounds of a chapter window, or None when the window is
/// empty. `from > to` is a client that has subtracted wrongly, and the honest
/// answer to it is no rows rather than every row.
fn window_bounds(from: usize, to: usize) -> Option<(i64, i64)> {
    if from > to {
        return None;
    }
    Some((as_i64(from as u64), as_i64(to as u64)))
}

/// A counter read back out of `meta`, which is TEXT because `meta` is a
/// key/value table and every other value in it is a string.
fn meta_u64(v: &str) -> Option<u64> {
    v.trim().parse::<u64>().ok()
}

/// SQLite has no unsigned integers. The saturation is unreachable — it would take
/// nine quintillion positions — and it is here so the cast is not a silent wrap.
fn as_i64(v: u64) -> i64 {
    if v > i64::MAX as u64 {
        i64::MAX
    } else {
        v as i64
    }
}

/// And back. A negative value is a hand-edited row, not arithmetic.
fn as_u64(v: i64) -> u64 {
    if v < 0 {
        0
    } else {
        v as u64
    }
}

fn as_usize(v: i64) -> usize {
    as_u64(v) as usize
}

// ------------------------------------------------------------------- the store

/// The handle. One connection, one lock, no pool.
///
/// Wrap it in an `Arc` at the call site rather than cloning a connection: the
/// point of the `Mutex` is that there is exactly one writer, and two handles to
/// the same file would reintroduce the `SQLITE_BUSY` this is designed not to
/// have.
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if need be) the database at `path`, make its parent
    /// directories, and walk the schema forward.
    ///
    /// Every failure here is an `Err` and none of them is fatal to narrator: the
    /// caller logs it and runs without the store. The one thing this refuses to
    /// do is guess — a file that is not a database, or one from a newer build,
    /// comes back as an error rather than being moved aside, because the contents
    /// might be the only copy of an overnight download somebody asked for.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        Self::from_conn(Connection::open(path)?)
    }

    /// An in-memory database, for tests and for a caller that wants the API
    /// without the file. Same schema, same migrations, nothing on disk — and
    /// nothing shared between two calls, so a test gets its own.
    pub fn open_memory() -> Result<Store> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Store> {
        // `execute_batch` rather than `pragma_update`: `journal_mode` answers
        // with a row, and a pragma that returns rows is an error through the
        // update path. WAL is a no-op on an in-memory database and says so
        // rather than failing, which is why both constructors share this.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )?;
        let st = Store {
            conn: Mutex::new(conn),
        };
        st.migrate()?;
        Ok(st)
    }

    /// The lock, with poisoning treated as the non-event it is.
    ///
    /// Nothing in this module panics while holding the connection, so a poisoned
    /// mutex means some *other* thread unwound through a borrow — and rusqlite
    /// rolls a dropped transaction back, so what is behind the lock is a
    /// consistent database either way. Refusing to open it after that would turn
    /// one panic elsewhere into a store that is dead for the life of the process,
    /// which is a worse answer than carrying on.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------- migrations

    /// Forward-only, one transaction per step, version written last.
    ///
    /// Idempotent by construction: the version is read first and every step is
    /// skipped once it is at or below it, so opening the same file twice runs
    /// nothing the second time. Each step commits with its own version bump, so
    /// an interrupted run of several migrations resumes at the one that did not
    /// finish rather than starting over.
    fn migrate(&self) -> Result<()> {
        let mut c = self.lock();
        c.execute_batch("CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);")?;
        let have = match meta_get(&c, K_SCHEMA)? {
            None => 0,
            Some(v) => meta_u64(&v)
                .ok_or_else(|| StoreError::Corrupt(format!("schema_version is {v:?}")))?
                as u32,
        };
        if have > SCHEMA_VERSION {
            return Err(StoreError::FromTheFuture(have));
        }
        if have < 1 {
            let tx = c.transaction()?;
            tx.execute_batch(SCHEMA_1)?;
            meta_set(&tx, K_SCHEMA, "1")?;
            tx.commit()?;
        }
        Ok(())
    }

    /// The schema version on disk. Cheap, and worth logging at boot.
    pub fn schema_version(&self) -> Result<u32> {
        let c = self.lock();
        Ok(meta_get(&c, K_SCHEMA)?
            .and_then(|v| meta_u64(&v))
            .unwrap_or(0) as u32)
    }

    // ----------------------------------------------------------------- the seq

    /// The next value of the monotonic counter, persisted before it is returned.
    ///
    /// It lives in `meta` rather than in an `AtomicU64` seeded at startup for one
    /// reason: a restart. An in-memory counter reseeded from `MAX(seq)` is fine
    /// until the row holding that maximum is deleted — a cancelled download, a
    /// position for a book that went away — and then the next process hands out
    /// numbers it has already used, and the tie-break that decides which of two
    /// positions is newer starts lying. Persisting the high-water mark itself
    /// costs one small write per bump and can never do that.
    pub fn seq(&self) -> Result<u64> {
        let c = self.lock();
        next_seq(&c)
    }

    // ------------------------------------------------------------- devices

    /// Record that a device is here, minting its row the first time.
    ///
    /// `first_seen` is written once and never touched again; `name` is only
    /// overwritten when the caller actually supplies one, so a reader that stops
    /// sending a label does not silently erase the one a person typed.
    pub fn touch_device(&self, id: &str, name: &str, now_ms: i64) -> Result<()> {
        let c = self.lock();
        c.execute(
            "INSERT INTO device (id, name, first_seen, last_seen) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET
               last_seen = excluded.last_seen,
               name = CASE WHEN excluded.name = '' THEN device.name ELSE excluded.name END",
            params![id, name, now_ms],
        )?;
        Ok(())
    }

    /// Every device ever seen, most recently seen first.
    pub fn devices(&self) -> Result<Vec<DeviceRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT id, name, first_seen, last_seen FROM device ORDER BY last_seen DESC, id",
        )?;
        let rows = q.query_map([], device_row)?;
        collect(rows)
    }

    /// The devices seen at or after `since_ms` — "who is actually using this
    /// server", which is a different and much smaller set than "who ever has".
    pub fn active_devices(&self, since_ms: i64) -> Result<Vec<DeviceRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT id, name, first_seen, last_seen FROM device
             WHERE last_seen >= ?1 ORDER BY last_seen DESC, id",
        )?;
        let rows = q.query_map([since_ms], device_row)?;
        collect(rows)
    }

    // ------------------------------------------------------------ positions

    /// Write where a device is, and return the sequence number it was stamped
    /// with.
    ///
    /// The `seq` field of `row` is ignored — the store stamps it, because a
    /// number the caller chose is a number two callers can choose twice. The
    /// returned value is the same one, handed back so a caller that is about to
    /// broadcast the write can say which write it was.
    ///
    /// `book` is the **epub file name**, exactly as `.narrator-positions.json`
    /// keys it.
    pub fn put_position(&self, book: &str, device: &str, row: PositionRow) -> Result<u64> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        let seq = next_seq(&tx)?;
        tx.execute(
            "INSERT INTO position
               (book, device, chapter, chunk, chapter_title, chunks_total, chapters_total,
                updated_ms, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(book, device) DO UPDATE SET
               chapter = excluded.chapter, chunk = excluded.chunk,
               chapter_title = excluded.chapter_title,
               chunks_total = excluded.chunks_total,
               chapters_total = excluded.chapters_total,
               updated_ms = excluded.updated_ms, seq = excluded.seq",
            params![
                book,
                device,
                row.chapter,
                row.chunk,
                row.chapter_title,
                row.chunks_total,
                row.chapters_total,
                row.updated_ms,
                as_i64(seq),
            ],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// One device's position in one book.
    pub fn position(&self, book: &str, device: &str) -> Result<Option<PositionRow>> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT chapter, chunk, chapter_title, chunks_total, chapters_total, updated_ms, seq
             FROM position WHERE book = ?1 AND device = ?2",
            params![book, device],
            position_row,
        )
        .optional()?)
    }

    /// Every device's position in one book, newest first.
    pub fn positions_for_book(&self, book: &str) -> Result<Vec<(String, PositionRow)>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT device, chapter, chunk, chapter_title, chunks_total, chapters_total,
                    updated_ms, seq
             FROM position WHERE book = ?1 ORDER BY updated_ms DESC, seq DESC",
        )?;
        let rows = q.query_map([book], |r| {
            Ok((r.get::<_, String>(0)?, position_row_at(r, 1)?))
        })?;
        collect(rows)
    }

    /// The position the vault should show for this book: the most recent write,
    /// ties broken by sequence.
    ///
    /// This *is* the projection rule. The vault has room for one record per book
    /// and several devices can have an opinion, so last-write-wins is the answer,
    /// and `seq` is what makes it deterministic when two writes share a
    /// millisecond instead of leaving it to whichever row came back first.
    pub fn newest_position(&self, book: &str) -> Result<Option<(String, PositionRow)>> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT device, chapter, chunk, chapter_title, chunks_total, chapters_total,
                    updated_ms, seq
             FROM position WHERE book = ?1 ORDER BY updated_ms DESC, seq DESC LIMIT 1",
            [book],
            |r| Ok((r.get::<_, String>(0)?, position_row_at(r, 1)?)),
        )
        .optional()?)
    }

    /// The same rule, for every book at once: what a full rewrite of
    /// `.narrator-positions.json` is made of.
    pub fn all_newest_positions(&self) -> Result<Vec<(String, String, PositionRow)>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT p.book, p.device, p.chapter, p.chunk, p.chapter_title, p.chunks_total,
                    p.chapters_total, p.updated_ms, p.seq
             FROM position p
             WHERE p.seq = (SELECT q.seq FROM position q WHERE q.book = p.book
                            ORDER BY q.updated_ms DESC, q.seq DESC LIMIT 1)
             ORDER BY p.book",
        )?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                position_row_at(r, 2)?,
            ))
        })?;
        collect(rows)
    }

    // ------------------------------------------------------------- furthest

    /// Move the furthest-reached mark, if and only if this is further.
    ///
    /// Returns whether it moved. The comparison is lexicographic on
    /// (chapter, chunk) and it is **strict**, so re-saving the same spot is not a
    /// move — the reader's trim reads the timestamp as "how long since this
    /// advanced", and a heartbeat that restamped it would quietly disable the
    /// only thing keeping a 1433-chapter novel off a phone.
    ///
    /// Read and write in one transaction: two devices bumping at once must not
    /// both read the old value and let the lower of the two win.
    pub fn bump_furthest(
        &self,
        book: &str,
        device: &str,
        chapter: usize,
        chunk: usize,
        now_ms: i64,
    ) -> Result<bool> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        let old: Option<(i64, i64)> = tx
            .query_row(
                "SELECT chapter, chunk FROM furthest WHERE book = ?1 AND device = ?2",
                params![book, device],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let new = (as_i64(chapter as u64), as_i64(chunk as u64));
        let moved = match old {
            Some(o) => further(new, o),
            None => true,
        };
        if moved {
            tx.execute(
                "INSERT INTO furthest (book, device, chapter, chunk, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(book, device) DO UPDATE SET
                   chapter = excluded.chapter, chunk = excluded.chunk,
                   updated_ms = excluded.updated_ms",
                params![book, device, new.0, new.1, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(moved)
    }

    /// How far this device has ever got in this book.
    pub fn furthest(&self, book: &str, device: &str) -> Result<Option<FurthestRow>> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT chapter, chunk, updated_ms FROM furthest WHERE book = ?1 AND device = ?2",
            params![book, device],
            |r| {
                Ok(FurthestRow {
                    chapter: r.get(0)?,
                    chunk: r.get(1)?,
                    updated_ms: r.get(2)?,
                })
            },
        )
        .optional()?)
    }

    // --------------------------------------------------------------- intent

    /// Place a standing order for these chapters, in the order given.
    ///
    /// One transaction and one `seq` per chapter, so the order asked survives as
    /// a total order across books and restarts. A chapter already on the list
    /// keeps its original place and its original `created_ms` — asking twice is
    /// what a reconciler does every two minutes, and re-stamping would let a
    /// chapter somebody asked for last night drift to the back of tonight's
    /// queue. What asking again *does* clear is `tries`: the button is the retry,
    /// exactly as it is for the wishlist's parked chapters.
    ///
    /// `book` is the **cache key**.
    pub fn add_intent(
        &self,
        book: &str,
        chapters: &[usize],
        device: &str,
        pack: bool,
        now_ms: i64,
    ) -> Result<()> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        for ch in chapters {
            let seq = next_seq(&tx)?;
            tx.execute(
                "INSERT INTO intent (book, chapter, device, pack, seq, tries, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                 ON CONFLICT(book, chapter) DO UPDATE SET
                   device = excluded.device,
                   pack = MAX(intent.pack, excluded.pack),
                   tries = 0",
                params![
                    book,
                    as_i64(*ch as u64),
                    device,
                    i64::from(pack),
                    as_i64(seq),
                    now_ms
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// One book's standing order, in the order asked.
    pub fn intents(&self, book: &str) -> Result<Vec<IntentRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT book, chapter, device, pack, seq, tries, created_ms
             FROM intent WHERE book = ?1 ORDER BY seq",
        )?;
        let rows = q.query_map([book], intent_row)?;
        collect(rows)
    }

    /// Every book's, in one global order — which is the order the box should work
    /// through them, because `seq` is when each was asked for.
    pub fn all_intents(&self) -> Result<Vec<IntentRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT book, chapter, device, pack, seq, tries, created_ms FROM intent ORDER BY seq",
        )?;
        let rows = q.query_map([], intent_row)?;
        collect(rows)
    }

    /// Forget one chapter's order. Returns whether there was one — a cancel of
    /// something already done is not an error, and the caller usually wants to
    /// know whether to say anything about it.
    pub fn drop_intent(&self, book: &str, chapter: usize) -> Result<bool> {
        let c = self.lock();
        let n = c.execute(
            "DELETE FROM intent WHERE book = ?1 AND chapter = ?2",
            params![book, as_i64(chapter as u64)],
        )?;
        Ok(n > 0)
    }

    /// Forget a whole book's order — "cancel all", and what a book leaving the
    /// library should do. Returns how many rows went.
    pub fn drop_intents_for_book(&self, book: &str) -> Result<usize> {
        let c = self.lock();
        Ok(c.execute("DELETE FROM intent WHERE book = ?1", [book])?)
    }

    /// Count one more attempt against a chapter and return the new total.
    ///
    /// Returns 0 for a chapter with no order, which reads correctly at every call
    /// site: nothing was asked for, so nothing has failed.
    pub fn bump_tries(&self, book: &str, chapter: usize) -> Result<u32> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE intent SET tries = tries + 1 WHERE book = ?1 AND chapter = ?2",
            params![book, as_i64(chapter as u64)],
        )?;
        let tries: Option<i64> = tx
            .query_row(
                "SELECT tries FROM intent WHERE book = ?1 AND chapter = ?2",
                params![book, as_i64(chapter as u64)],
                |r| r.get(0),
            )
            .optional()?;
        tx.commit()?;
        Ok(tries.map(|t| as_u64(t) as u32).unwrap_or(0))
    }

    /// A chunk landed, or a person asked again: the count is about neither of
    /// those, so it goes back to zero.
    pub fn reset_tries(&self, book: &str, chapter: usize) -> Result<()> {
        let c = self.lock();
        c.execute(
            "UPDATE intent SET tries = 0 WHERE book = ?1 AND chapter = ?2",
            params![book, as_i64(chapter as u64)],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ inventory

    /// A device says it now holds this chapter.
    pub fn put_inventory(
        &self,
        device: &str,
        book: &str,
        chapter: usize,
        bytes: Option<u64>,
        now_ms: i64,
    ) -> Result<()> {
        let c = self.lock();
        c.execute(
            "INSERT INTO inventory (device, book, chapter, bytes, stored_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(device, book, chapter) DO UPDATE SET
               bytes = COALESCE(excluded.bytes, inventory.bytes),
               stored_ms = excluded.stored_ms",
            params![
                device,
                book,
                as_i64(chapter as u64),
                bytes.map(as_i64),
                now_ms
            ],
        )?;
        Ok(())
    }

    /// A device gave a chapter back — the trim, or a quota eviction it noticed.
    pub fn drop_inventory(&self, device: &str, book: &str, chapter: usize) -> Result<bool> {
        let c = self.lock();
        let n = c.execute(
            "DELETE FROM inventory WHERE device = ?1 AND book = ?2 AND chapter = ?3",
            params![device, book, as_i64(chapter as u64)],
        )?;
        Ok(n > 0)
    }

    /// Who holds what, for one book.
    pub fn inventory_for_book(&self, book: &str) -> Result<Vec<InventoryRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT device, book, chapter, bytes, stored_ms FROM inventory
             WHERE book = ?1 ORDER BY device, chapter",
        )?;
        let rows = q.query_map([book], inventory_row)?;
        collect(rows)
    }

    /// Set one device's holdings for one book wholesale, in one transaction.
    ///
    /// This is the shape the reader's reconciler actually reports in: it *asks*
    /// Cache Storage what is there rather than remembering, precisely so a quota
    /// eviction reads as missing, and the answer it gets is the complete list.
    /// Applying it as "these and no others" is therefore the only faithful
    /// translation, and one transaction is what stops a crash halfway through
    /// leaving a device that appears to hold three chapters of eleven.
    ///
    /// Two things are deliberately preserved for a chapter that is in both the
    /// old set and the new one: its `bytes` and its `stored_ms`. A sweep runs
    /// every twenty seconds and refreshing the timestamp on every pass would turn
    /// "when this was stored" into "when this was last seen", which is a
    /// different fact and the wrong input to anything that ages a chapter out.
    pub fn replace_inventory(
        &self,
        device: &str,
        book: &str,
        chapters: &[usize],
        now_ms: i64,
    ) -> Result<()> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        {
            let mut keep = tx.prepare(
                "DELETE FROM inventory WHERE device = ?1 AND book = ?2 AND chapter = ?3",
            )?;
            let held: Vec<i64> = {
                let mut q =
                    tx.prepare("SELECT chapter FROM inventory WHERE device = ?1 AND book = ?2")?;
                let rows = q.query_map(params![device, book], |r| r.get::<_, i64>(0))?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                out
            };
            let wanted: Vec<i64> = chapters.iter().map(|c| as_i64(*c as u64)).collect();
            for ch in held {
                if !wanted.contains(&ch) {
                    keep.execute(params![device, book, ch])?;
                }
            }
            let mut add = tx.prepare(
                "INSERT INTO inventory (device, book, chapter, bytes, stored_ms)
                 VALUES (?1, ?2, ?3, NULL, ?4)
                 ON CONFLICT(device, book, chapter) DO NOTHING",
            )?;
            for ch in wanted {
                add.execute(params![device, book, ch, now_ms])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------- chapter index

    /// Record a filesystem scan of one book's chapters. **A cache, not truth** —
    /// see the module doc, and do not reach for it from the renderer.
    ///
    /// One transaction for the whole book: a scan is a single observation of a
    /// single moment, and half of one is a book that claims twelve chapters are
    /// packed and the other fourteen hundred are empty.
    pub fn put_chapter_index(&self, book: &str, rows: &[ChapterIndexRow]) -> Result<()> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO chapter_index
                   (book, chapter, n, rendered, m4a, bytes, duration, title, est_s, scanned_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(book, chapter) DO UPDATE SET
                   n = excluded.n, rendered = excluded.rendered, m4a = excluded.m4a,
                   bytes = excluded.bytes, duration = excluded.duration,
                   title = excluded.title, est_s = excluded.est_s,
                   scanned_ms = excluded.scanned_ms",
            )?;
            for r in rows {
                ins.execute(params![
                    book,
                    as_i64(r.chapter as u64),
                    as_i64(r.n as u64),
                    as_i64(r.rendered as u64),
                    i64::from(r.m4a),
                    r.bytes.map(as_i64),
                    r.duration,
                    r.title,
                    r.est_s,
                    r.scanned_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// One book's cached scan, in chapter order.
    pub fn chapter_index(&self, book: &str) -> Result<Vec<ChapterIndexRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT chapter, n, rendered, m4a, bytes, duration, title, est_s, scanned_ms
             FROM chapter_index WHERE book = ?1 ORDER BY chapter",
        )?;
        let rows = q.query_map([book], chapter_index_row)?;
        collect(rows)
    }

    /// A window of it, inclusive at both ends — the same shape `/api/chapters`
    /// takes `?from=&to=` in, so a reader showing a screenful pays for a
    /// screenful.
    pub fn chapter_index_window(
        &self,
        book: &str,
        from: usize,
        to: usize,
    ) -> Result<Vec<ChapterIndexRow>> {
        let Some((lo, hi)) = window_bounds(from, to) else {
            return Ok(Vec::new());
        };
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT chapter, n, rendered, m4a, bytes, duration, title, est_s, scanned_ms
             FROM chapter_index WHERE book = ?1 AND chapter BETWEEN ?2 AND ?3 ORDER BY chapter",
        )?;
        let rows = q.query_map(params![book, lo, hi], chapter_index_row)?;
        collect(rows)
    }

    // ---------------------------------------------------------------- books

    /// Remember a book, or update what is known about it.
    ///
    /// `last_open_ms` is merged rather than assigned: a `None` leaves whatever is
    /// there. The watcher rescans the library on every epub that appears and it
    /// has no idea when anybody last opened anything — letting its scan write a
    /// null over the open stamp would reset the scheduler's notion of "the
    /// current book" every time the vault's git sync ran.
    pub fn put_book(&self, row: &BookRow) -> Result<()> {
        let c = self.lock();
        c.execute(
            "INSERT INTO book (key, name, path, title, chapters, last_open_ms, scanned_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(key) DO UPDATE SET
               name = excluded.name, path = excluded.path, title = excluded.title,
               chapters = excluded.chapters,
               last_open_ms = COALESCE(excluded.last_open_ms, book.last_open_ms),
               scanned_ms = COALESCE(excluded.scanned_ms, book.scanned_ms)",
            params![
                row.key,
                row.name,
                row.path,
                row.title,
                as_i64(row.chapters as u64),
                row.last_open_ms,
                row.scanned_ms,
            ],
        )?;
        Ok(())
    }

    /// Every book, by key.
    pub fn books(&self) -> Result<Vec<BookRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT key, name, path, title, chapters, last_open_ms, scanned_ms
             FROM book ORDER BY key",
        )?;
        let rows = q.query_map([], book_row)?;
        collect(rows)
    }

    /// One, by cache key.
    pub fn book(&self, key: &str) -> Result<Option<BookRow>> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT key, name, path, title, chapters, last_open_ms, scanned_ms
             FROM book WHERE key = ?1",
            [key],
            book_row,
        )
        .optional()?)
    }

    /// Somebody opened this book. Does nothing for a book that is not known,
    /// which is the caller's cue to [`put_book`](Store::put_book) first.
    pub fn touch_book_open(&self, key: &str, now_ms: i64) -> Result<()> {
        let c = self.lock();
        c.execute(
            "UPDATE book SET last_open_ms = ?2 WHERE key = ?1",
            params![key, now_ms],
        )?;
        Ok(())
    }

    /// Most recently opened first, never-opened last.
    ///
    /// The nulls-last part is explicit because SQLite sorts NULL *first* under a
    /// plain `DESC`, which would put every book nobody has ever opened at the top
    /// of a list whose whole purpose is "what was I reading".
    pub fn recent_books(&self, limit: usize) -> Result<Vec<BookRow>> {
        let c = self.lock();
        let mut q = c.prepare(
            "SELECT key, name, path, title, chapters, last_open_ms, scanned_ms
             FROM book
             ORDER BY (last_open_ms IS NULL), last_open_ms DESC, key
             LIMIT ?1",
        )?;
        let rows = q.query_map([as_i64(limit as u64)], book_row)?;
        collect(rows)
    }
}

// ------------------------------------------------------------ plumbing

/// Version 1. Written out as one string rather than assembled, because a schema
/// is read far more often than it is changed and the `CREATE TABLE` text is the
/// clearest description of these rows that exists.
const SCHEMA_1: &str = "
CREATE TABLE IF NOT EXISTS device (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL DEFAULT '',
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS position (
  book TEXT NOT NULL, device TEXT NOT NULL,
  chapter INTEGER NOT NULL, chunk INTEGER NOT NULL,
  chapter_title TEXT NOT NULL DEFAULT '',
  chunks_total INTEGER NOT NULL DEFAULT 0,
  chapters_total INTEGER NOT NULL DEFAULT 0,
  updated_ms INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  PRIMARY KEY (book, device)
);
CREATE TABLE IF NOT EXISTS furthest (
  book TEXT NOT NULL, device TEXT NOT NULL,
  chapter INTEGER NOT NULL, chunk INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
  PRIMARY KEY (book, device)
);
CREATE TABLE IF NOT EXISTS intent (
  book TEXT NOT NULL, chapter INTEGER NOT NULL,
  device TEXT NOT NULL DEFAULT '',
  pack INTEGER NOT NULL DEFAULT 1,
  seq INTEGER NOT NULL,
  tries INTEGER NOT NULL DEFAULT 0,
  created_ms INTEGER NOT NULL,
  PRIMARY KEY (book, chapter)
);
CREATE TABLE IF NOT EXISTS inventory (
  device TEXT NOT NULL, book TEXT NOT NULL, chapter INTEGER NOT NULL,
  bytes INTEGER, stored_ms INTEGER NOT NULL,
  PRIMARY KEY (device, book, chapter)
);
CREATE TABLE IF NOT EXISTS chapter_index (
  book TEXT NOT NULL, chapter INTEGER NOT NULL,
  n INTEGER NOT NULL, rendered INTEGER NOT NULL,
  m4a INTEGER NOT NULL, bytes INTEGER, duration REAL,
  title TEXT NOT NULL DEFAULT '', est_s REAL,
  scanned_ms INTEGER NOT NULL,
  PRIMARY KEY (book, chapter)
);
CREATE TABLE IF NOT EXISTS book (
  key TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  path TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  chapters INTEGER NOT NULL DEFAULT 0,
  last_open_ms INTEGER,
  scanned_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_position_updated ON position(book, updated_ms DESC);
CREATE INDEX IF NOT EXISTS idx_intent_seq ON intent(seq);
CREATE INDEX IF NOT EXISTS idx_chapter_index_book ON chapter_index(book);
";

fn meta_get(c: &Connection, k: &str) -> Result<Option<String>> {
    Ok(
        c.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
            .optional()?,
    )
}

fn meta_set(tx: &Transaction<'_>, k: &str, v: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![k, v],
    )?;
    Ok(())
}

/// Bump and return the counter, in whatever transaction the caller is already
/// in.
///
/// Takes a `&Connection` rather than going through [`Store::lock`] because every
/// caller is already holding it — `std::sync::Mutex` is not reentrant, so a
/// version of this that took `&self` would deadlock the first time a write
/// needed a sequence number, which is every write that needs one.
fn next_seq(c: &Connection) -> Result<u64> {
    let v: String = c.query_row(
        "INSERT INTO meta (k, v) VALUES (?1, '1')
         ON CONFLICT(k) DO UPDATE SET v = CAST(CAST(meta.v AS INTEGER) + 1 AS TEXT)
         RETURNING v",
        [K_SEQ],
        |r| r.get(0),
    )?;
    meta_u64(&v).ok_or_else(|| StoreError::Corrupt(format!("seq is {v:?}")))
}

/// `query_map` gives an iterator of results; this is the three lines every
/// caller would otherwise repeat, and it stops at the first bad row rather than
/// returning a short list that looks complete.
fn collect<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        id: r.get(0)?,
        name: r.get(1)?,
        first_seen: r.get(2)?,
        last_seen: r.get(3)?,
    })
}

fn position_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PositionRow> {
    position_row_at(r, 0)
}

/// The seven position columns starting at `i`, so a query that selects a device
/// alongside them does not need a second copy of this.
fn position_row_at(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<PositionRow> {
    Ok(PositionRow {
        chapter: r.get(i)?,
        chunk: r.get(i + 1)?,
        chapter_title: r.get(i + 2)?,
        chunks_total: r.get(i + 3)?,
        chapters_total: r.get(i + 4)?,
        updated_ms: r.get(i + 5)?,
        seq: as_u64(r.get::<_, i64>(i + 6)?),
    })
}

fn intent_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<IntentRow> {
    Ok(IntentRow {
        book: r.get(0)?,
        chapter: as_usize(r.get::<_, i64>(1)?),
        device: r.get(2)?,
        pack: r.get::<_, i64>(3)? != 0,
        seq: as_u64(r.get::<_, i64>(4)?),
        tries: as_u64(r.get::<_, i64>(5)?) as u32,
        created_ms: r.get(6)?,
    })
}

fn inventory_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<InventoryRow> {
    Ok(InventoryRow {
        device: r.get(0)?,
        book: r.get(1)?,
        chapter: as_usize(r.get::<_, i64>(2)?),
        bytes: r.get::<_, Option<i64>>(3)?.map(as_u64),
        stored_ms: r.get(4)?,
    })
}

fn chapter_index_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChapterIndexRow> {
    Ok(ChapterIndexRow {
        chapter: as_usize(r.get::<_, i64>(0)?),
        n: as_usize(r.get::<_, i64>(1)?),
        rendered: as_usize(r.get::<_, i64>(2)?),
        m4a: r.get::<_, i64>(3)? != 0,
        bytes: r.get::<_, Option<i64>>(4)?.map(as_u64),
        duration: r.get(5)?,
        title: r.get(6)?,
        est_s: r.get(7)?,
        scanned_ms: r.get(8)?,
    })
}

fn book_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BookRow> {
    Ok(BookRow {
        key: r.get(0)?,
        name: r.get(1)?,
        path: r.get(2)?,
        title: r.get(3)?,
        chapters: as_usize(r.get::<_, i64>(4)?),
        last_open_ms: r.get(5)?,
        scanned_ms: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn further_is_lexicographic_and_strict() {
        assert!(further((1, 0), (0, 999)), "a chapter forward always counts");
        assert!(further((0, 5), (0, 4)));
        assert!(!further((0, 4), (0, 4)), "standing still is not moving");
        assert!(!further((0, 3), (0, 4)));
        assert!(!further((0, 999), (1, 0)), "a chapter back is still back");
    }

    #[test]
    fn a_window_with_its_ends_the_wrong_way_round_is_empty() {
        assert_eq!(window_bounds(0, 0), Some((0, 0)));
        assert_eq!(window_bounds(3, 7), Some((3, 7)));
        assert_eq!(window_bounds(7, 3), None);
    }

    #[test]
    fn a_meta_counter_that_is_not_a_number_has_no_value() {
        assert_eq!(meta_u64("41"), Some(41));
        assert_eq!(meta_u64(" 41 "), Some(41), "sqlite casts pad nothing, but");
        assert_eq!(meta_u64(""), None);
        assert_eq!(meta_u64("-1"), None);
        assert_eq!(meta_u64("seventeen"), None);
    }

    #[test]
    fn the_unsigned_round_trip_saturates_rather_than_wrapping() {
        assert_eq!(as_i64(7), 7);
        assert_eq!(as_i64(u64::MAX), i64::MAX);
        assert_eq!(as_u64(7), 7);
        assert_eq!(as_u64(-3), 0);
        assert_eq!(as_usize(-3), 0);
    }
}
