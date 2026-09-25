//! One global session, exactly like the python server's process-wide `S`.
//!
//! Playback state is one book/chapter/playhead across all clients: two devices
//! reading different books fight each other, which is why anything cacheable is
//! scoped by `?book=` and positions can be written by name. Keeping that shape
//! is a parity requirement, not an accident — the Obsidian plugin and the reader
//! both assume it.
//!
//! The lock discipline: `Mutex<Session>` is only ever held for field reads and
//! writes, never across a render, an ffmpeg run or an `.await`. Anything that
//! takes time copies what it needs out first.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::book::Chapter;
use crate::cache;
use crate::config::Config;
use crate::events::Bus;
use crate::stt::Whisper;
use crate::tts::Engine;
use crate::vault::Positions;

#[derive(Debug, Default)]
pub struct Session {
    pub status: String,
    /// Full path of the loaded epub.
    pub book: Option<String>,
    pub title: Option<String>,
    pub plan: Arc<Vec<Chapter>>,
    pub est_s: Vec<f64>,
    pub chapter: usize,
    pub render_idx: usize,
    pub playhead: usize,
    pub error: Option<String>,
    pub rendered_s: f64,
    pub render_time: f64,
    pub model_ready: bool,
    pub prerender: Option<usize>,
    pub prerender_hours: Option<f64>,
    /// Chapters the UI asked for by name, in the order asked.
    pub queue: Vec<usize>,
    /// Chapters that should be packed once rendered.
    pub build_want: BTreeSet<usize>,
    /// Chapters waiting for the packer, and the one it is on.
    pub pack_queue: Vec<usize>,
    /// The same, for chapters of a book this session is **not** holding.
    ///
    /// A separate list rather than a key on `pack_queue`, for one reason:
    /// `pack_queue` is in `/api/status` as a list of chapter numbers and that is
    /// a frozen shape. This is additive beside it.
    ///
    /// It exists because a standing order outlives the book being loaded. Order
    /// seventy-four chapters, then open something else: the renderer follows
    /// them (see `next_elsewhere`), and without this the packer could not, so a
    /// night of rendering would produce no files at all until that book was
    /// opened again — which is the exact bug `pack: true` was introduced to fix,
    /// one level up.
    pub pack_elsewhere: Vec<(String, usize)>,
    /// The chapter of the **loaded** book the packer is on, and nothing else.
    ///
    /// It is in `/api/status` and `/api/chapters` as a bare chapter number, and
    /// every reader of it — the drawer's `packing` flag, the cancel that leaves an
    /// encode in flight alone — reads it as a chapter of the book in front of it.
    /// A foreign job used to be written here too, so chapter 7 of a book nobody
    /// had open showed as packing on chapter 7 of the one somebody did. That job
    /// is in [`Session::packing`] instead.
    pub building: Option<usize>,
    /// The job the packer is on, whichever book it belongs to.
    ///
    /// The book-qualified twin of `building`, and the one the gc reads: the chunk
    /// wavs of a chapter being encoded are the encode's *input*, and protecting
    /// "chapter 7" of the wrong book is protecting nothing.
    pub packing: Option<(String, usize)>,
    pub build_error: Option<String>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            status: "idle".into(),
            ..Default::default()
        }
    }

    pub fn key(&self) -> Option<String> {
        self.book.as_deref().map(cache::book_key)
    }

    /// `book_key()` — "x" when nothing is loaded, which is what the python
    /// server's `chapter_dir` falls back to.
    pub fn key_or_x(&self) -> String {
        self.key().unwrap_or_else(|| "x".into())
    }

    pub fn book_name(&self) -> Option<String> {
        self.book.as_deref().map(|b| {
            std::path::Path::new(b)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| b.to_string())
        })
    }

    /// How many chapters past `ci` the worker should build. An hours target set
    /// from the UI wins over the static `PRERENDER_CHAPTERS`; the span is however
    /// many chapters the estimator says those hours are.
    pub fn prerender_span(&self, ci: usize, cfg: &Config) -> usize {
        let Some(hrs) = self.prerender_hours.filter(|h| *h > 0.0) else {
            return cfg.prerender_chapters;
        };
        let mut left = hrs * 3600.0;
        let mut n = 0usize;
        for cj in (ci + 1)..self.est_s.len() {
            left -= self.est_s[cj];
            n += 1;
            if left <= 0.0 {
                break;
            }
        }
        n
    }

    /// The chapter directories `gc_audio` must not touch: the chapter being read
    /// and its prerender span, plus everything the chapter manager is working on —
    /// on this book and, for the packer, on any other.
    ///
    /// What this cannot see is a standing order on a book the session is not
    /// holding; that lives in the store, and [`AppState::gc_keep`] adds it.
    pub fn gc_keep(&self, cfg: &Config) -> HashSet<PathBuf> {
        let mut keep = HashSet::new();
        // Not behind the loaded-book check below: a foreign pack is the one job
        // that does not need a book loaded at all, and its chunks are its input.
        for (k, c) in self.pack_elsewhere.iter().chain(self.packing.iter()) {
            keep.insert(cache::chapter_dir(&cfg.work, k, *c));
        }
        let Some(key) = self.key() else {
            return keep;
        };
        let n = self.plan.len();
        let span = self.prerender_span(self.chapter, cfg);
        for c in self.chapter..n.min(self.chapter + span + 1) {
            keep.insert(cache::chapter_dir(&cfg.work, &key, c));
        }
        let mut pending: BTreeSet<usize> = self.queue.iter().copied().collect();
        pending.extend(self.build_want.iter().copied());
        pending.extend(self.pack_queue.iter().copied());
        if let Some(b) = self.building {
            pending.insert(b);
        }
        for c in pending {
            if c < n {
                keep.insert(cache::chapter_dir(&cfg.work, &key, c));
            }
        }
        keep
    }
}

/// A flag the render thread waits on, with none of `threading.Event`'s cost.
#[derive(Default)]
pub struct Gate {
    set: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    pub fn set(&self) {
        if let Ok(mut g) = self.set.lock() {
            *g = true;
        }
        self.cv.notify_all();
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.set.lock() {
            *g = false;
        }
    }

    pub fn is_set(&self) -> bool {
        self.set.lock().map(|g| *g).unwrap_or(false)
    }

    /// Block until set or the timeout expires. Returns the flag.
    pub fn wait(&self, timeout: std::time::Duration) -> bool {
        let Ok(g) = self.set.lock() else {
            return false;
        };
        match self.cv.wait_timeout_while(g, timeout, |s| !*s) {
            Ok((g, _)) => *g,
            Err(_) => false,
        }
    }
}

pub struct AppState {
    pub cfg: Config,
    pub session: Mutex<Session>,
    pub bus: Bus,
    pub engine: Engine,
    pub whisper: Whisper,

    /// `RUN` — set means the worker should render.
    pub run: Gate,
    /// `BUILD_EV` — set means there is something in the pack queue.
    pub build_ev: Gate,
    pub stop: AtomicBool,

    /// Positions, cached in memory exactly like python's `POS`.
    pub positions: Mutex<Positions>,
    pub pos_written: Mutex<Option<Instant>>,

    /// Whether the render / build threads have been started.
    pub render_started: AtomicBool,
    pub build_started: AtomicBool,

    /// Every attempt to put a chunk on disk, successful or not — a cache hit is
    /// not one. It is not in the API and nothing reads it at runtime; it exists
    /// so that "the worker is retrying a chunk it can never render" is a number
    /// rather than a guess, and it is what the backoff test asserts against.
    pub render_attempts: AtomicU64,

    /// When a chunk last landed on disk — `/healthz` turns a renderer that has
    /// silently wedged into a 503 a systemd timer can act on.
    pub progress_at: Mutex<Instant>,
    pub started_at: Instant,

    /// `/api/chapters`' 1.5 s memo. A 1433-chapter scan is a few thousand stats
    /// and the drawer polls it.
    pub chstat: Mutex<Option<(Instant, String, Vec<crate::api::chapters::ChapterRow>)>>,
    pub autopack_at: Mutex<Option<Instant>>,

    /// What the three queues above do not say about themselves: how many
    /// restarts each chapter has been picked back up by, and which have run out.
    /// The queues stay where they are; see [`crate::wishlist`].
    pub wishlist: Mutex<crate::wishlist::Wishlist>,

    /// Since when the render worker has been stalled on the chunk under the
    /// playhead — rule 1, the branch where somebody is waiting *right now*.
    /// `None` whenever it is doing anything else, the lookahead included. It is
    /// the packer's cue to hold an encode back; see [`crate::render`].
    pub render_stalled: Mutex<Option<Instant>>,

    /// The durable intent-and-identity store, or `None` if it would not open.
    ///
    /// An `Option` rather than a hard requirement, and that is the whole policy
    /// in one type: a work directory gone read-only, a `state.db` that is not a
    /// database, a disk with nothing left on it — none of those is a reason to
    /// refuse to start a reader. What is lost without it is the *extra* answers
    /// (which device is where, what the library has ready, a standing order that
    /// outlives the process); what keeps working is everything that was working
    /// before the store existed, because all of it still reads the filesystem
    /// and the vault. Every caller therefore reaches it through
    /// [`AppState::store`] and does nothing at all when it is absent.
    pub store: Option<Arc<crate::store::Store>>,

    /// Who is connected right now. Ephemeral by design — see
    /// [`crate::api::device`]'s presence section for why this is the one piece
    /// of device state that is deliberately not in the database.
    pub roster: Arc<crate::api::device::Roster>,

    /// The chunk cache's size in bytes, as last measured by the gc.
    ///
    /// A measurement rather than a running total, because the renderer is not
    /// the only thing that changes it — the gc deletes, `narrator migrate`
    /// deletes, and an operator with `rm` deletes. It is read by the speculative
    /// render branch, which must stand down well below the gc's threshold or the
    /// two of them spend the box's spare core taking turns; see `IDLE_CEILING`
    /// in [`crate::render`]. Stale by up to 25 chunks, which is kilobytes
    /// against a band measured in gigabytes.
    pub audio_bytes: AtomicU64,

    /// A counter that only ever goes up, stamped on every position write.
    ///
    /// The tie-break under `updated_ms`, and it earns its place on a box this
    /// slow for a reason that is not hypothetical: two devices reporting inside
    /// the same millisecond is unlikely, but a *clock* that steps — an ntp
    /// correction on a box that has been up for weeks, a container whose
    /// `/etc/localtime` changed under it — makes two writes compare equal or
    /// backwards, and "which of these is the later one" then has no answer at
    /// all. A monotonic integer always has one.
    ///
    /// Seeded from the store at boot so it never repeats across a restart; the
    /// atomic is what the hot path touches, because a position write happens
    /// under the session lock and must not also want a database.
    pub seq: AtomicU64,
}

impl AppState {
    pub fn new(cfg: Config) -> Arc<Self> {
        let bus = Bus::new(cfg.sse_queue, cfg.sse_render_min_s);
        let engine = Engine::new(&cfg);
        let whisper = Whisper::new(&cfg);
        let positions = crate::vault::load_positions(&cfg.positions_dir);
        // Logged once, loudly, and then never mentioned again: a server running
        // without its store is a degraded one and the operator should be able to
        // find out why from the boot log rather than from a missing feature.
        let store = match crate::store::Store::open(&cfg.work.join("state.db")) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::error!("state.db unavailable ({e}); continuing without it");
                None
            }
        };
        Arc::new(Self {
            store,
            cfg,
            session: Mutex::new(Session::new()),
            bus,
            engine,
            whisper,
            run: Gate::default(),
            build_ev: Gate::default(),
            stop: AtomicBool::new(false),
            positions: Mutex::new(positions),
            pos_written: Mutex::new(None),
            render_started: AtomicBool::new(false),
            build_started: AtomicBool::new(false),
            render_attempts: AtomicU64::new(0),
            progress_at: Mutex::new(Instant::now()),
            started_at: Instant::now(),
            chstat: Mutex::new(None),
            autopack_at: Mutex::new(None),
            wishlist: Mutex::new(crate::wishlist::Wishlist::default()),
            render_stalled: Mutex::new(None),
            audio_bytes: AtomicU64::new(0),
            roster: Arc::new(crate::api::device::Roster::default()),
            seq: AtomicU64::new(1),
        })
    }

    /// The store, if there is one. Sugar, so a caller reads as
    /// `if let Some(db) = st.store()` rather than repeating the field's shape.
    pub fn store(&self) -> Option<&Arc<crate::store::Store>> {
        self.store.as_ref()
    }

    /// The next position stamp. Monotonic, and never zero — zero is what a
    /// reader sees when the field is absent, which is a different claim.
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed).max(1)
    }

    /// Lift the counter past everything already on record, at boot.
    ///
    /// Idempotent and only ever upward: two restores in one process (the test
    /// harness restarts a server in place) must not walk it backwards, because a
    /// repeated stamp is exactly the ambiguity the counter exists to remove.
    pub fn seed_seq(&self, at_least: u64) {
        self.seq
            .fetch_max(at_least.saturating_add(1), Ordering::Relaxed);
    }

    /// Take the session lock, logging rather than panicking if it was poisoned by
    /// a thread that died mid-update. A poisoned lock is recoverable here: every
    /// field is independently meaningful and the renderer re-derives from disk.
    pub fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        match self.session.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("session lock was poisoned; continuing with its contents");
                p.into_inner()
            }
        }
    }

    /// The wishlist's bookkeeping. Always taken *before* the session lock, never
    /// after — see [`crate::wishlist::save`] for what the order is protecting.
    pub fn wishlist(&self) -> std::sync::MutexGuard<'_, crate::wishlist::Wishlist> {
        match self.wishlist.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("wishlist lock was poisoned; continuing with its contents");
                p.into_inner()
            }
        }
    }

    pub fn positions(&self) -> std::sync::MutexGuard<'_, Positions> {
        match self.positions.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Everything `gc_audio` must leave alone, across the library.
    ///
    /// [`Session::gc_keep`] plus every chapter somebody has a standing order on,
    /// whichever book it is. Those are exactly the chapters the worker is working
    /// through while another book is open (`next_elsewhere`), and without them the
    /// gc — oldest first — deletes an order's early chunks while its later ones
    /// are still rendering: a chapter that is never complete, so never packed, so
    /// never retired, rendered again from the top for as long as the process
    /// lives. Parked chapters are included: still owed, and asking again resumes
    /// them from what is on disk.
    ///
    /// The session lock is released before the store is asked, so this never
    /// holds the two at once.
    pub fn gc_keep(&self) -> HashSet<PathBuf> {
        let mut keep = self.session().gc_keep(&self.cfg);
        for (key, items) in crate::wishlist::all_outstanding(self) {
            for it in items {
                keep.insert(cache::chapter_dir(&self.cfg.work, &key, it.chapter));
            }
        }
        keep
    }

    /// How long the renderer has been stuck under the playhead, in seconds.
    /// Zero means it is not — which is the answer nearly always, and is what
    /// lets the packer run at all.
    pub fn stalled_for(&self) -> f64 {
        self.render_stalled
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }

    pub fn set_stalled(&self, yes: bool) {
        if let Ok(mut g) = self.render_stalled.lock() {
            if yes {
                g.get_or_insert_with(Instant::now);
            } else {
                *g = None;
            }
        }
    }

    pub fn touch_progress(&self) {
        if let Ok(mut g) = self.progress_at.lock() {
            *g = Instant::now();
        }
    }

    pub fn since_progress(&self) -> f64 {
        self.progress_at
            .lock()
            .map(|g| g.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_gate_wakes_a_waiter() {
        let g = Arc::new(Gate::default());
        let g2 = g.clone();
        let t = std::thread::spawn(move || g2.wait(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        g.set();
        assert!(t.join().unwrap_or(false));
    }

    #[test]
    fn a_gate_times_out_without_blocking_forever() {
        let g = Gate::default();
        assert!(!g.wait(Duration::from_millis(10)));
    }

    #[test]
    fn prerender_span_falls_back_to_the_static_count() {
        let d = tempfile::tempdir().expect("tempdir");
        let cfg = Config::for_test(d.path());
        let mut s = Session::new();
        s.est_s = vec![600.0; 10];
        assert_eq!(s.prerender_span(0, &cfg), cfg.prerender_chapters);
        s.prerender_hours = Some(1.0);
        // Six ten-minute chapters fill an hour.
        assert_eq!(s.prerender_span(0, &cfg), 6);
        s.prerender_hours = Some(100.0);
        assert_eq!(
            s.prerender_span(0, &cfg),
            9,
            "never past the end of the book"
        );
    }
}
