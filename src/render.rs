//! The render worker and the packer, one OS thread each.
//!
//! Synthesis is CPU-bound and takes a second or more per chunk, so it does not
//! belong on the async runtime at all: the worker is a plain thread that the HTTP
//! handlers wake through a [`Gate`], exactly as the python server's
//! `threading.Event` does. The packer is separate on purpose — an ffmpeg encode
//! of a long chapter takes seconds, and stalling the renderer for it would stall
//! the playhead.
//!
//! ## The disk-truth invariant
//!
//! Field bug, 2026-09-11: the python worker advances `render_idx` and treats it
//! as the record of what has been rendered. It is not. `gc_audio` can delete a
//! chunk behind the frontier, a `/api/playhead` jump drags `render_idx` past
//! holes, and a container restart resets it — and in each case the chunk the
//! reader is *waiting on* never gets rendered, because `render_idx` has already
//! moved past it. The reader then sits on a 404 forever with a renderer that
//! believes it is up to date.
//!
//! So here `render_idx` is a **hint** and the filesystem is the truth:
//!
//! 1. If the chunk under the playhead is missing, it is rendered first. Always.
//! 2. Otherwise the worker scans forward from the hint for the first chunk that
//!    is genuinely absent, and renders that.
//! 3. `/api/open` sets the playhead, so rule 1 guarantees the opened chunk
//!    renders — whatever the cache looks like.
//!
//! ## ... and what it cost
//!
//! Rule 1 has no exit: a chunk under the playhead that is missing is rendered,
//! every time round the loop, for as long as the playhead sits on it. That is
//! exactly right while the render can succeed and exactly wrong when it cannot —
//! an espeak-ng that will not answer, a work directory gone read-only, a model
//! that failed to load — because then it is a full-speed retry loop: a subprocess
//! spawn and a `warn!` line per iteration, burning a core for nothing and burying
//! the log. [`Backoff`] bounds it without weakening the invariant: the chunk is
//! still retried, and still first, just not faster than a doubling interval.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::book::Chunk;
use crate::cache;
use crate::chapters;
use crate::state::{AppState, ChapterRef};

/// Start the render thread if it is not already running.
pub fn ensure_render_thread(st: &Arc<AppState>) {
    if st
        .render_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let st = st.clone();
        std::thread::Builder::new()
            .name("render".into())
            .spawn(move || worker(st))
            .map_err(|e| tracing::error!("could not start render thread: {e}"))
            .ok();
    }
}

pub fn ensure_build_thread(st: &Arc<AppState>) {
    if st
        .build_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let st = st.clone();
        std::thread::Builder::new()
            .name("build".into())
            .spawn(move || builder(st))
            .map_err(|e| tracing::error!("could not start build thread: {e}"))
            .ok();
    }
}

pub fn render_alive(st: &AppState) -> bool {
    st.render_started.load(Ordering::SeqCst)
}

fn render_event(st: &AppState, kind: &str, ci: usize, idx: usize, total: usize, status: &str) {
    let key = st.session().key();
    render_event_for(st, key.as_deref(), kind, ci, idx, total, status);
}

/// The same event, for work on a book that is **not** the loaded one.
///
/// The speculative branch renders across the library, and an event that took its
/// `key` from the session would name the wrong book — which the reader acts on:
/// it invalidates that book's chapter rows and its download reconciler reads
/// them. A render event has always carried a key; until there was work on other
/// books, reading it off the session happened to be the same thing.
#[allow(clippy::too_many_arguments)]
fn render_event_for(
    st: &AppState,
    key: Option<&str>,
    kind: &str,
    ci: usize,
    idx: usize,
    total: usize,
    status: &str,
) {
    let playhead = st.session().playhead;
    st.bus.emit_render(
        kind,
        json!({"key": key, "chapter": ci, "render_idx": idx,
               "playhead": playhead, "n": total, "status": status}),
    );
}

/// How long the worker waits after a run of failed renders.
///
/// Consecutive failures only — any success puts it straight back to zero, so a
/// single unlucky chunk in a healthy book costs a quarter of a second and
/// nothing else. It is deliberately *not* per-chunk: a render that fails is
/// almost always a systemic problem (the engine did not load, the disk is full,
/// espeak-ng is wedged), and in that state every chunk fails, so counting
/// per-chunk would just ping-pong between two hot targets. The cap is generous
/// because the thing being avoided is a spin, not a delay: when the cause clears,
/// the next tick renders and the counter resets.
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// The longest single sleep, so `stop` is still noticed promptly at shutdown.
const BACKOFF_SLICE: Duration = Duration::from_millis(400);

#[derive(Default)]
struct Backoff {
    fails: u32,
    until: Option<Instant>,
}

impl Backoff {
    /// May the worker try to render right now?
    fn ready(&self) -> bool {
        match self.until {
            None => true,
            Some(t) => Instant::now() >= t,
        }
    }

    /// How long is left, in a slice short enough to stay responsive to `stop`.
    fn nap(&self) -> Duration {
        match self.until {
            None => Duration::ZERO,
            Some(t) => t
                .saturating_duration_since(Instant::now())
                .min(BACKOFF_SLICE),
        }
    }

    fn record(&mut self, ok: bool) {
        if ok {
            self.fails = 0;
            self.until = None;
            return;
        }
        self.fails = self.fails.saturating_add(1);
        let d = BACKOFF_BASE
            .saturating_mul(1u32 << (self.fails - 1).min(16))
            .min(BACKOFF_MAX);
        self.until = Instant::now().checked_add(d);
        if self.fails == 1 || self.fails.is_power_of_two() {
            tracing::warn!(
                "{} render(s) failed in a row; retrying in {:.1}s",
                self.fails,
                d.as_secs_f64()
            );
        }
    }
}

/// Render chunk `i` of chapter `ci` to disk, unless it is already there.
///
/// Never propagates a failure upward: a chunk that will not synthesize is logged
/// and skipped, because the alternative — a render thread that dies on one bad
/// sentence — takes the whole book with it.
fn render_one(st: &AppState, key: &str, ci: usize, i: usize, chunks: &[Chunk]) -> bool {
    let p = cache::chunk_path(&st.cfg.work, key, ci, i);
    if p.exists() {
        return true;
    }
    let Some(chunk) = chunks.get(i) else {
        return false;
    };
    st.render_attempts.fetch_add(1, Ordering::Relaxed);
    let t0 = Instant::now();
    let wav = if chunk.silent {
        // Nothing to pronounce — render the beat, do not ask the model to invent
        // one. See `book::is_speakable`.
        crate::tts::silence(st.cfg.silence_s as f32)
    } else {
        match st.engine.generate(&chunk.text) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("chapter {ci} chunk {i}: {e}");
                return false;
            }
        }
    };
    if let Err(e) = cache::write_wav(&p, &wav) {
        tracing::warn!("chapter {ci} chunk {i}: {e}");
        return false;
    }
    let mut s = st.session();
    s.rendered_s += wav.len() as f64 / crate::tts::SR as f64;
    s.render_time += t0.elapsed().as_secs_f64();
    drop(s);
    st.touch_progress();
    true
}

/// Render one chunk and record what it means — to the failure backoff, and to
/// the wishlist's poison counter.
///
/// A chapter that is putting audio on disk is not the chapter the parking rule
/// is about, however many restarts it has lived through, so any chunk that lands
/// clears its count. Cheap: a map lookup that almost always finds nothing.
fn attempt(st: &Arc<AppState>, key: &str, ci: usize, i: usize, chunks: &[Chunk], bo: &mut Backoff) {
    let ok = render_one(st, key, ci, i, chunks);
    bo.record(ok);
    if ok {
        crate::wishlist::progress(st, key, ci);
    }
}

fn exists(st: &AppState, key: &str, ci: usize, i: usize) -> bool {
    cache::chunk_path(&st.cfg.work, key, ci, i).exists()
}

/// The first chunk at or after `from` that is not on disk, bounded by `limit`.
fn first_missing(st: &AppState, key: &str, ci: usize, from: usize, limit: usize) -> Option<usize> {
    (from..limit).find(|j| !exists(st, key, ci, *j))
}

/// First unrendered chunk in the chapters following `ci`, or None.
fn next_ahead(st: &AppState, key: &str, ci: usize, span: usize) -> Option<(usize, usize)> {
    let plan = st.session().plan.clone();
    for cj in (ci + 1)..plan.len().min(ci + 1 + span) {
        let n = plan[cj].chunks.len();
        if let Some(j) = first_missing(st, key, cj, 0, n) {
            return Some((cj, j));
        }
    }
    None
}

// ------------------------------------------------------ rendering while idle
//
// The box has two ARM cores and renders at about a quarter of realtime, so it can
// never keep up with a listener — which means every second it spends idle is a
// second of audio somebody will wait for later. Before this, it spent a lot of
// them: once the playhead's lookahead and the prerender span were full, the
// worker slept in a 1-second loop with a whole novel unrendered behind it.
//
// So there is a branch below all the others that says: render the next thing
// anybody is plausibly going to want. The rest of the current book first, then
// the other books in the library, most recently opened first, each from the
// position it was last left at. It outranks nothing — a playhead move, a
// lookahead hole and a chapter somebody actually asked for all still come first,
// and the loop re-evaluates every iteration, so this work is abandoned the
// instant there is real work.
//
// # The ceiling, which is the part that matters
//
// `gc_audio` trims the chunk cache to 90 % of `MAX_AUDIO_GB` once it is over
// 100 %, oldest first. A speculative renderer that runs until the cache is full
// therefore does not settle: it renders to the cap, the gc deletes the oldest
// chunks — which are exactly the ones nobody has listened to yet — and it renders
// them again, forever, burning the one core the box had spare on work that is
// thrown away before anyone hears it. Worse, it is invisible: the log looks
// busy, the RTF looks healthy, and nothing ever finishes.
//
// So speculation stops at `IDLE_CEILING` (`Config::idle_ceiling`) and the band
// between there and the gc
// belongs to *demanded* work only. The renderer and the collector then never
// touch: one stops below the floor the other starts at.
/// How often the speculative branch re-measures the cache.
///
/// Measured in *time*, not in chunks rendered, and that is a correction rather
/// than a preference: a count only advances when something is rendered, so the
/// branch that stands down — the one that has decided the cache is full — would
/// never advance it and would re-measure on every pass of a loop that ticks once
/// a second. The measurement is a walk of every wav in the cache, which on the
/// box is 50 GB of them. Once a minute costs nothing and is far fresher than it
/// needs to be: the A1 renders perhaps fifteen chunks in that time, a megabyte
/// or so, against a band between the ceiling and the collector measured in
/// gigabytes.
const IDLE_MEASURE_EVERY: Duration = Duration::from_secs(60);

/// A chunk worth rendering on a book found by searching — a standing order or
/// speculation — carrying the plan it was found in.
struct Target {
    key: String,
    chapter: usize,
    chunk: usize,
    plan: Arc<Vec<crate::book::Chapter>>,
}

impl Target {
    fn choice(self, rule: Rule) -> Choice {
        Choice::Render {
            rule,
            at: ChapterRef::new(self.key, self.chapter),
            idx: self.chunk,
            plan: self.plan,
        }
    }
}

/// Where the reader would resume a book that is not the loaded one.
///
/// The stored position, because that is where a person coming back to it would
/// start — rendering a book from chapter one when they are eighty chapters in is
/// the most expensive possible way to be useless. Falls back to the beginning
/// for a book nobody has opened.
fn resume_chapter(st: &AppState, name: &str) -> usize {
    if let Some(db) = st.store() {
        if let Ok(Some((_, row))) = db.newest_position(name) {
            return row.chapter.max(0) as usize;
        }
    }
    st.positions()
        .get(name)
        .and_then(|v| v.get("chapter"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
        .max(0) as usize
}

/// The first chapter of `plan` at or after `from` with a chunk missing on disk.
fn first_hole(
    st: &AppState,
    key: &str,
    plan: &[crate::book::Chapter],
    from: usize,
) -> Option<(usize, usize)> {
    for (cj, ch) in plan.iter().enumerate().skip(from) {
        let n = ch.chunks.len();
        if n == 0 {
            continue;
        }
        if let Some(j) = first_missing(st, key, cj, 0, n) {
            return Some((cj, j));
        }
    }
    None
}

/// Pick something worth rendering, or None if the whole library is done.
///
/// Deliberately re-derived rather than remembered: the cheapest way to be wrong
/// here is to hold a target across a gc that deleted it, or across a book being
/// loaded, and every branch above this one already re-reads the filesystem for
/// the same reason. The cost is bounded — one `read_dir` per chapter until a
/// hole is found, and a hole is usually found immediately.
fn next_idle(
    st: &Arc<AppState>,
    cur_key: &str,
    cur_plan: &Arc<Vec<crate::book::Chapter>>,
    from: usize,
    plans: &mut Plans,
) -> Option<Target> {
    // 1. The rest of the book in front of the reader. It is the one they are
    //    most likely to want next, and its plan is already in memory.
    if let Some((cj, j)) = first_hole(st, cur_key, cur_plan, from) {
        return Some(Target {
            key: cur_key.to_string(),
            chapter: cj,
            chunk: j,
            plan: cur_plan.clone(),
        });
    }
    // 2. Everything else, most recently opened first — which is what Fernando
    //    asked for in so many words, and is also the only ordering the server
    //    can defend: it is the last thing he chose, rather than a guess about
    //    what he might choose next.
    let db = st.store()?;
    let books = db
        .recent_books(IDLE_BOOKS)
        .map_err(|e| tracing::warn!("could not read the library: {e}"))
        .ok()?;
    for b in books {
        if b.key == cur_key {
            continue;
        }
        // Raw, never through the parse cache: most of these books are not
        // loaded, their epubs may have moved, and the chunks on disk correspond
        // to this plan whatever has happened to the file they came from.
        let Some(plan) = plans.get(st, &b.key) else {
            continue;
        };
        let from = resume_chapter(st, &b.name).min(plan.len().saturating_sub(1));
        if let Some((cj, j)) = first_hole(st, &b.key, &plan, from) {
            return Some(Target {
                key: b.key,
                chapter: cj,
                chunk: j,
                plan,
            });
        }
    }
    None
}

/// How many books deep the speculative branch looks. Twenty is far more than
/// the library has ever held, and the bound exists so that a pathological
/// library cannot turn one loop iteration into a thousand plan reads.
const IDLE_BOOKS: usize = 20;

/// Plans of books the session is not holding, read off `plan.json` and kept.
///
/// Reading one is 0.30 s on the 1433-chapter book, and both branches that work
/// on other books ask on every pass of the loop — `next_elsewhere` once per
/// chunk rendered, `next_idle` once a second when there is nothing else to do.
/// Each entry remembers the file's mtime and length and is re-read the moment
/// either moves, so `narrator migrate` rewriting a plan is picked up on the next
/// pass rather than never; a `stat` per book per pass is the whole cost of
/// asking.
///
/// Bounded, because a plan is the whole text of a book: a handful of recent
/// books is what the branches actually walk, and anything past that is read
/// again rather than held.
#[derive(Default)]
struct Plans {
    held: std::collections::HashMap<String, (Option<std::time::SystemTime>, u64, Plan)>,
}

type Plan = Arc<Vec<crate::book::Chapter>>;

const PLANS_HELD: usize = 8;

impl Plans {
    fn get(&mut self, st: &AppState, key: &str) -> Option<Plan> {
        let md = std::fs::metadata(cache::plan_path(&st.cfg.work, key)).ok()?;
        let stamp = (md.modified().ok(), md.len());
        if let Some((m, l, p)) = self.held.get(key) {
            if (*m, *l) == stamp {
                return Some(p.clone());
            }
        }
        let plan = crate::plancache::read_raw(&st.cfg.work, key)?;
        st.plan_reads.fetch_add(1, Ordering::Relaxed);
        if self.held.len() >= PLANS_HELD && !self.held.contains_key(key) {
            // No recency to speak of is worth tracking at this size: dropping
            // the lot costs one re-read per book on the next pass.
            self.held.clear();
        }
        self.held
            .insert(key.to_string(), (stamp.0, stamp.1, plan.clone()));
        Some(plan)
    }
}

/// A standing order on a book the session is **not** holding.
///
/// `next_queued` above covers the loaded book, because that is where the
/// session's `queue` lives. Everything else somebody asked for was invisible to
/// the worker until that book was loaded again — so "download these 74 chapters"
/// placed on the phone, followed by opening something else, left 74 chapters
/// waiting for an `/api/load` that might not come for days. The order was
/// durable the whole time; nothing was reading it.
///
/// Ranked **below** the loaded book's own queue and above the speculative
/// branch, which is the honest order: an explicit ask beats a guess, and the
/// book in front of the reader beats one that is not.
///
/// Completion is still disk truth — an order whose chunks all exist simply
/// leaves the list the first time this looks at it, exactly as `next_queued`
/// does for the loaded book.
fn next_elsewhere(st: &Arc<AppState>, cur_key: &str, plans: &mut Plans) -> Option<Target> {
    for (key, items) in crate::wishlist::all_outstanding(st) {
        if key == cur_key {
            continue; // `next_queued` owns this one.
        }
        // Reading `plan.json` is 0.30 s on the 1433-chapter book, and this runs
        // once per chunk rendered. See `Plans`.
        let Some(plan) = plans.get(st, &key) else {
            continue;
        };
        for it in items {
            if it.parked {
                continue; // out of attempts: still wanted, no longer tried.
            }
            let Some(ch) = plan.get(it.chapter) else {
                // Not a chapter of this book any more — a re-chunk, a different
                // epub under the same name. Nothing to render and nothing to
                // wait for.
                retire(st, &key, it.chapter, 0, false);
                continue;
            };
            let n = ch.chunks.len();
            if n == 0 {
                retire(st, &key, it.chapter, 0, false);
                continue;
            }
            if let Some(j) = first_missing(st, &key, it.chapter, 0, n) {
                return Some(Target {
                    key,
                    chapter: it.chapter,
                    chunk: j,
                    plan,
                });
            }
            // Every chunk is on disk. Completion is disk truth, so this is where
            // the order ends: hand it to the packer if it asked to be packed,
            // and take it off the list either way.
            //
            // Retiring it here is not tidiness. An order that is never dropped
            // is walked again on every pass of the worker loop, for the life of
            // the process — a `read_dir` per chapter and, without the cache
            // above, a plan read too. A finished order that stays on the list is
            // a slow leak with a healthy-looking log, which is the shape of
            // failure this whole round is about.
            retire(st, &key, it.chapter, n, it.pack);
        }
    }
    None
}

/// Take a finished (or impossible) order off the list, packing it first if that
/// is what was asked for.
fn retire(st: &Arc<AppState>, key: &str, ci: usize, n: usize, pack: bool) {
    // `n` matters: a manifest whose chunk count disagrees with the plan is a
    // manifest for a chapter that has since been re-rendered, and packing again
    // is exactly right. Passing a wrong `n` here would either re-pack a finished
    // chapter on every pass or accept a stale file as done.
    if pack && !chapters::chapter_packed(&st.cfg.work, key, ci, n) {
        // The packer drops the intent when the encode lands, so the order
        // survives a restart that happens mid-pack.
        enqueue_build_elsewhere(st, key, ci);
        return;
    }
    if let Some(db) = st.store() {
        if let Err(e) = db.drop_intent(key, ci) {
            tracing::warn!("could not clear the order for {key} ch{ci}: {e}");
        }
    }
    // The table is the record; the book's `queue.json` is its copy, and it is
    // not the session's to rewrite, so it is brought up to date here.
    crate::wishlist::project(st, key);
}

/// First unrendered chunk of the chapter queue the UI filled, or None.
///
/// The queue is the chapter manager's "Render" button: explicit chapters, in the
/// order asked for, worked on after the playhead's own lookahead is satisfied but
/// before the automatic prerender span. A chapter that turns out to be complete
/// leaves the queue here, and is handed to the packer if it was queued for
/// offline.
fn next_queued(st: &Arc<AppState>, key: &str) -> Option<(usize, usize)> {
    let (q, plan) = {
        let s = st.session();
        (s.queue.clone(), s.plan.clone())
    };
    // An item that leaves the queue has to leave the file too, before the worker
    // moves on to the next one: a restart in between would put a chapter that is
    // finished back on the list. Harmless for a render — the chunks are there and
    // it leaves again immediately — but it also un-does a cancel that was
    // processed in the same pass, and that is work on a book nobody asked for.
    let mut changed = false;
    let done = |st: &Arc<AppState>, changed: bool| {
        if changed {
            crate::wishlist::save(st);
        }
    };
    for cj in q {
        if cj >= plan.len() {
            st.session().queue.retain(|c| *c != cj);
            changed = true;
            continue;
        }
        let n = plan[cj].chunks.len();
        if let Some(j) = first_missing(st, key, cj, 0, n) {
            done(st, changed);
            return Some((cj, j));
        }
        let want = {
            let mut s = st.session();
            s.queue.retain(|c| *c != cj);
            s.build_want.contains(&cj)
        };
        changed = true;
        render_event(st, "complete", cj, n, n, "queued");
        if want {
            enqueue_build(st, cj);
        }
    }
    done(st, changed);
    None
}

fn gc(st: &AppState) {
    let keep = st.gc_keep();
    let total = cache::gc_audio(&st.cfg.work, st.cfg.max_audio_gb, &keep);
    // The walk has already been done, so the number is free. It is what the
    // speculative branch stands down on.
    st.audio_bytes.store(total, Ordering::Relaxed);
}

/// Is there room to render something nobody has asked for yet?
///
/// See `Config::idle_ceiling`. `max_audio_gb` at or below zero means the cap
/// is disabled, and so is this check; `IDLE_RENDER=0` turns the branch off
/// entirely.
fn room_to_speculate(st: &AppState) -> bool {
    if !st.cfg.idle_render {
        return false;
    }
    if st.cfg.max_audio_gb <= 0.0 {
        return true;
    }
    let cap = st.cfg.max_audio_gb * 1024.0_f64.powi(3);
    (st.audio_bytes.load(Ordering::Relaxed) as f64) < cap * st.cfg.idle_ceiling
}

/// Is every chunk of this chapter on disk?
fn chapter_complete(st: &AppState, key: &str, ci: usize, n: usize) -> bool {
    first_missing(st, key, ci, 0, n).is_none()
}

/// Queue the current or next chapter for packing if it is ready and unpacked.
///
/// Field bug, 2026-09-11: on the VPS `work/chapters/` did not exist at all and
/// ffmpeg had never run, because packing was reachable only from the chapter
/// manager's explicit "download". Organic listening — the thing Fernando actually
/// does — never produced an m4a, so the player never had an HLS or chapter source
/// to prefer and every session ran on the per-chunk fallback, which is the slow
/// path by design.
///
/// Three rules keep it honest: only the chapter being read and the one after it;
/// only from the worker's idle branches, never while it is racing the playhead;
/// one chapter in flight, and never on top of a queued user request.
pub fn autopack(st: &Arc<AppState>, force: bool) -> Option<usize> {
    if !st.cfg.autopack {
        return None;
    }
    {
        let mut at = st.autopack_at.lock().ok()?;
        let now = Instant::now();
        if !force {
            if let Some(t) = *at {
                if now.duration_since(t).as_secs_f64() < st.cfg.autopack_every_s {
                    return None;
                }
            }
        }
        *at = Some(now);
    }
    let (ci, plan, key, busy) = {
        let s = st.session();
        (
            s.chapter,
            s.plan.clone(),
            s.key(),
            s.packing.is_some() || !s.loaded_pack_queue().is_empty() || !s.queue.is_empty(),
        )
    };
    let key = key?;
    if busy || plan.is_empty() {
        return None;
    }
    for c in [ci, ci + 1] {
        let Some(ch) = plan.get(c) else { continue };
        let n = ch.chunks.len();
        if n == 0 || chapters::chapter_packed(&st.cfg.work, &key, c, n) {
            continue;
        }
        if !chapter_complete(st, &key, c, n) {
            continue;
        }
        enqueue_build(st, c);
        return Some(c);
    }
    None
}

/// Ask the packer for chapter `ci` of the loaded book. Idempotent; starts the
/// thread. With nothing loaded there is no book the chapter could be of, and
/// nothing is queued.
pub fn enqueue_build(st: &Arc<AppState>, ci: usize) {
    let Some(key) = st.session().key() else {
        return;
    };
    enqueue_pack(st, ChapterRef::new(key, ci));
}

/// The same, for a chapter of a book the session is not holding.
///
/// The builder takes the loaded book's jobs first, which is the right way round:
/// the book in front of the reader is the one whose file somebody may be waiting
/// for.
pub fn enqueue_build_elsewhere(st: &Arc<AppState>, key: &str, ci: usize) {
    enqueue_pack(st, ChapterRef::new(key, ci));
}

fn enqueue_pack(st: &Arc<AppState>, job: ChapterRef) {
    {
        let mut s = st.session();
        if !s.pack_queue.contains(&job) {
            s.pack_queue.push(job);
        }
    }
    st.build_ev.set();
    ensure_build_thread(st);
}

// ----------------------------------------------------------------- the worker

/// Which rule picked a chunk, highest rank first.
///
/// Every rule renders the same way; they differ only in what they report while
/// they do it and what follows. See [`execute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// The chunk under the playhead is missing: somebody is waiting on it now.
    UnderPlayhead,
    /// A hole in the lookahead window of the chapter being read.
    Lookahead,
    /// A chapter somebody named in the chapter manager, on the loaded book.
    Queued,
    /// A standing order on a book the session is not holding.
    Elsewhere,
    /// The chapters after this one, up to the prerender span.
    Prerender,
    /// The rest of this book, then the library, when nothing else is owed.
    Speculative,
}

impl Rule {
    /// What `Session::status` says while this rule renders.
    fn status(self) -> &'static str {
        match self {
            Rule::UnderPlayhead | Rule::Lookahead => "rendering",
            Rule::Queued | Rule::Elsewhere => "queued",
            Rule::Prerender | Rule::Speculative => "prerendering",
        }
    }

    /// What `Session::prerender` becomes, or `None` to leave it alone.
    ///
    /// It names a chapter of the loaded book, so a standing order elsewhere
    /// does not touch it.
    fn prerender(self, ci: usize) -> Option<Option<usize>> {
        match self {
            Rule::UnderPlayhead | Rule::Elsewhere => None,
            Rule::Lookahead => Some(None),
            Rule::Queued | Rule::Prerender | Rule::Speculative => Some(Some(ci)),
        }
    }

    /// Does a chunk this rule renders count towards the gc every 25?
    ///
    /// The lookahead keys its gc off the render hint instead, and speculation
    /// re-measures on a clock (see `IDLE_MEASURE_EVERY`).
    fn counted(self) -> bool {
        matches!(self, Rule::Queued | Rule::Elsewhere | Rule::Prerender)
    }
}

/// What the worker does next.
#[derive(Debug)]
enum Choice {
    /// Render chunk `idx` of `at`, out of `plan`.
    Render {
        rule: Rule,
        at: ChapterRef,
        idx: usize,
        plan: Plan,
    },
    /// Nothing to render: say "ready", offer the packer a chapter, then sleep.
    Idle(Duration),
    /// Not yet — no book is loaded, or renders have been failing. Sleep and
    /// report nothing.
    Wait(Duration),
}

/// What the worker keeps between passes. None of it is a record of what is
/// rendered; that is always asked of the filesystem.
#[derive(Default)]
struct WorkerCtx {
    /// Chunks rendered by the counted rules, for the gc every 25.
    counted: usize,
    /// When the chunk cache was last measured for the ceiling.
    measured: Option<Instant>,
    /// Whether "rendering ahead paused" has been logged — once per spell, not
    /// once a second.
    said_full: bool,
    /// Foreign plans already read, so a standing order on another book does
    /// not cost a `plan.json` parse per chunk. See `Plans`.
    plans: Plans,
    /// When speculation last looked across the library and found nothing, and
    /// where the reader was when it did. See [`speculate`].
    dry: Option<(Instant, String, usize, usize)>,
    bo: Backoff,
}

fn worker(st: Arc<AppState>) {
    st.engine.load();
    st.session().model_ready = st.engine.ready();
    let mut ctx = WorkerCtx::default();
    let mut parked: Option<Instant> = None;
    while !st.stop.load(Ordering::SeqCst) {
        if !st.run.wait(Duration::from_millis(500)) {
            continue;
        }
        if st.stop.load(Ordering::SeqCst) {
            break;
        }
        // Whisper outranks this. A voice memo exists only in the phone that
        // recorded it until `/api/note` answers, and on the two-core A1 a memo
        // sharing the box with Kokoro took seven minutes instead of one. So the
        // worker stands down for the duration — a bounded wait on the gate's
        // condvar, so it starts again the instant the last transcription ends —
        // and this is **not** a render failure, so the backoff is untouched.
        //
        // Said out loud both ways: a renderer that has quietly stopped is
        // exactly the shape of the bug this round is about, and "it is parked
        // for a memo" is only reassuring if it is written down somewhere.
        if !st.whisper.gate().wait_clear(Duration::from_millis(250)) {
            if parked.is_none() {
                parked = Some(Instant::now());
                tracing::info!("renderer parked: a voice memo is being transcribed");
            }
            continue;
        }
        if let Some(t) = parked.take() {
            tracing::info!(
                "renderer resumed after {:.1}s parked for transcription",
                t.elapsed().as_secs_f64()
            );
        }
        let choice = choose(&mut ctx, &st);
        execute(&mut ctx, &st, choice);
    }
    st.render_started.store(false, Ordering::SeqCst);
}

/// The next thing to render, by rank. The first rule with something to do wins,
/// and every pass starts again from the top, so real work abandons speculation
/// the moment it appears.
///
/// Every rule asks the filesystem rather than trusting `render_idx`; see the
/// disk-truth invariant at the top of this module.
fn choose(ctx: &mut WorkerCtx, st: &Arc<AppState>) -> Choice {
    let (ci, hint, ph, plan, key) = {
        let s = st.session();
        (s.chapter, s.render_idx, s.playhead, s.plan.clone(), s.key())
    };
    let Some(key) = key else {
        return Choice::Wait(Duration::from_millis(300));
    };
    // Renders have been failing: wait before trying again. Nothing is skipped
    // and nothing is given up on — the next pass comes straight back to the
    // chunk under the playhead — it just does not do it thousands of times a
    // second.
    if !ctx.bo.ready() {
        return Choice::Wait(ctx.bo.nap());
    }
    let n = plan.get(ci).map(|c| c.chunks.len()).unwrap_or(0);
    let here = |rule: Rule, cj: usize, j: usize| Choice::Render {
        rule,
        at: ChapterRef::new(key.clone(), cj),
        idx: j,
        plan: plan.clone(),
    };

    // Disk truth: the chunk under the playhead outranks everything. If it is
    // missing the reader is stalled on it right now.
    //
    // And this is the one condition under which a pack would genuinely starve
    // the renderer: two ARM cores, Kokoro at a quarter of realtime, and a
    // listener waiting on this exact chunk. Flagged here rather than read off
    // `status`, which also says "rendering" while the lookahead fills, and
    // cleared here too, before any rule below can hand the packer a job. See
    // the packer's hold-back.
    let stalled = ph < n && !exists(st, &key, ci, ph);
    st.set_stalled(stalled);
    if stalled {
        return here(Rule::UnderPlayhead, ci, ph);
    }
    if let Some(i) = lookahead_hole(st, &key, ci, n, hint, ph) {
        return here(Rule::Lookahead, ci, i);
    }
    // The playhead has all the buffer it asked for. Chapters the reader named
    // in the chapter manager come next — an explicit offline request outranks
    // the speculative span.
    if let Some((cj, j)) = next_queued(st, &key) {
        return here(Rule::Queued, cj, j);
    }
    // ...and the same for every *other* book somebody has a standing order on:
    // an explicit ask beats a guess, and the book in front of the reader beats
    // one that is not.
    if let Some(t) = next_elsewhere(st, &key, &mut ctx.plans) {
        return t.choice(Rule::Elsewhere);
    }
    // Buffered ahead within this chapter, and nothing asked for.
    if n > 0 && first_missing(st, &key, ci, 0, n).is_some() {
        return Choice::Idle(Duration::from_millis(300));
    }
    // This chapter is fully rendered. Rather than idle, build the buffer into
    // the chapters ahead — that head start is what keeps playback continuous
    // across a chapter boundary.
    let span = st.session().prerender_span(ci, &st.cfg);
    if let Some((cj, j)) = next_ahead(st, &key, ci, span) {
        return here(Rule::Prerender, cj, j);
    }
    match speculate(ctx, st, &key, &plan, ci, ph, span) {
        Some(t) => t.choice(Rule::Speculative),
        None => Choice::Idle(Duration::from_secs(1)),
    }
}

/// The lookahead window, scanned for a real hole rather than trusted: from the
/// hint first, then from the playhead.
fn lookahead_hole(
    st: &AppState,
    key: &str,
    ci: usize,
    n: usize,
    hint: usize,
    ph: usize,
) -> Option<usize> {
    let limit = n.min(ph.saturating_add(st.cfg.lookahead).saturating_add(1));
    first_missing(st, key, ci, hint.min(limit), limit)
        .or_else(|| first_missing(st, key, ci, ph, limit))
}

/// Everything anybody has asked for is done and the buffer is full: something
/// worth rendering anyway, or None.
///
/// The box renders at a quarter of realtime and can never catch up with a
/// listener, so an idle second here is a second of waiting later: keep going
/// through the rest of this book and then through the library, most recently
/// opened first. Below every other rule, and stopped well short of the gc's
/// threshold, which is the part that keeps it from becoming a treadmill. See
/// `IDLE_CEILING`.
fn speculate(
    ctx: &mut WorkerCtx,
    st: &Arc<AppState>,
    key: &str,
    plan: &Plan,
    ci: usize,
    ph: usize,
    span: usize,
) -> Option<Target> {
    if ctx
        .measured
        .is_none_or(|t: Instant| t.elapsed() >= IDLE_MEASURE_EVERY)
    {
        gc(st);
        ctx.measured = Some(Instant::now());
    }
    // A pass that found nothing is not repeated every second. With the whole
    // library rendered it was: a `stat` per chapter of this book and a plan read
    // and a `stat` per chapter of every recent one, once a second for as long as
    // the box stayed finished — on the A1, a core's steady background hum to
    // learn nothing new. What can make the answer change is the reader moving
    // (a load, an open, a playhead — each of which moves `key`, `ci` or `ph` and
    // is looked at again at once), or the library growing under it, which can
    // wait for the next re-measure. Orders are not affected: the queue and the
    // standing orders are ranked above this and asked on every pass.
    let looked = ctx.dry.as_ref().is_some_and(|(t, k, c, p)| {
        t.elapsed() < IDLE_MEASURE_EVERY && k == key && *c == ci && *p == ph
    });
    if looked {
        return None;
    }
    if !room_to_speculate(st) {
        if !ctx.said_full {
            ctx.said_full = true;
            tracing::info!(
                "rendering ahead paused: the chunk cache is within {:.0}% of {} GB",
                st.cfg.idle_ceiling * 100.0,
                st.cfg.max_audio_gb
            );
        }
        return None;
    }
    let t = next_idle(st, key, plan, ci + span + 1, &mut ctx.plans);
    ctx.dry = match t {
        Some(_) => None,
        None => Some((Instant::now(), key.to_string(), ci, ph)),
    };
    if t.is_some() {
        ctx.said_full = false;
    }
    t
}

/// Do what [`choose`] picked.
fn execute(ctx: &mut WorkerCtx, st: &Arc<AppState>, choice: Choice) {
    match choice {
        Choice::Wait(d) => std::thread::sleep(d),
        Choice::Idle(d) => {
            {
                let mut s = st.session();
                s.status = "ready".into();
                s.prerender = None;
            }
            autopack(st, false);
            std::thread::sleep(d);
        }
        Choice::Render {
            rule,
            at,
            idx,
            plan,
        } => {
            {
                let mut s = st.session();
                s.status = rule.status().into();
                if let Some(p) = rule.prerender(at.ci) {
                    s.prerender = p;
                }
            }
            let chunks: &[Chunk] = plan.get(at.ci).map(|c| c.chunks.as_slice()).unwrap_or(&[]);
            let n = chunks.len();
            attempt(st, &at.key, at.ci, idx, chunks, &mut ctx.bo);
            if rule == Rule::Lookahead {
                advance(st, &at, idx, n);
                return;
            }
            render_event_for(
                st,
                Some(&at.key),
                "progress",
                at.ci,
                idx + 1,
                n,
                rule.status(),
            );
            if rule.counted() {
                ctx.counted += 1;
                if ctx.counted % 25 == 0 {
                    gc(st);
                }
            }
        }
    }
}

/// After a lookahead chunk: move the hint on, and when that finishes the
/// chapter, say so.
fn advance(st: &Arc<AppState>, at: &ChapterRef, i: usize, n: usize) {
    let next = {
        let mut s = st.session();
        // Only advance if nothing moved the hint while we were rendering: a
        // forward jump from /api/playhead must not be clobbered by the stale
        // i+1 computed several seconds ago.
        if s.render_idx <= i {
            s.render_idx = i + 1;
        }
        s.render_idx
    };
    if next >= n {
        render_event_for(st, Some(&at.key), "complete", at.ci, next, n, "rendering");
        // The chapter just became packable and the renderer is about to go do
        // speculative work; pack it now, while it matters.
        autopack(st, true);
        // ...and the library index has just gone stale in the one way that
        // matters — a chapter went from partly to fully rendered. Cheap (one
        // plan read, one `read_dir` per chapter) and worth doing here, because
        // the alternative is that nothing notices until the scanner's next
        // five-minute tick. Note the direction: the worker *writes* this index
        // and never reads it. See the disk-truth invariant.
        crate::library::rescan_book(st, &at.key);
    } else {
        render_event_for(st, Some(&at.key), "progress", at.ci, next, n, "rendering");
    }
    if next % 25 == 0 {
        gc(st);
    }
}

// ----------------------------------------------------------------- the packer

/// The longest the packer defers to a stalled renderer before packing anyway.
///
/// A courtesy, not a lock — see the hold-back in [`builder`]. Thirty seconds is
/// many chunks on the desktop and a couple on the A1; past it the renderer is
/// not slow, it is stuck (a wedged espeak-ng, a read-only work dir), and a
/// download that waits on a stuck renderer forever would be a worse bug than the
/// one the hold-back prevents.
const PACK_HOLD_MAX_S: f64 = 30.0;

fn builder(st: Arc<AppState>) {
    let mut parked: Option<Instant> = None;
    let mut held: Option<Instant> = None;
    while !st.stop.load(Ordering::SeqCst) {
        st.build_ev.wait(Duration::from_secs(1));
        // The same rule as the renderer, for the same reason: an ffmpeg encode
        // of a long chapter is the other thing on this box that will hold a core
        // for a minute, and a memo waiting on it is a memo that exists nowhere
        // but a phone.
        //
        // Only a *new* pack is held back. One already running is left to finish:
        // killing an encode mid-chapter throws away every second it has spent
        // and the chapter has to be packed again from nothing, which costs the
        // box more than the transcription gains — and the packer is one chapter
        // at a time, so the wait is bounded by one encode either way.
        if !st.whisper.gate().wait_clear(Duration::from_millis(250)) {
            if parked.is_none() {
                parked = Some(Instant::now());
                tracing::info!("packer parked: a voice memo is being transcribed");
            }
            continue;
        }
        if let Some(t) = parked.take() {
            tracing::info!(
                "packer resumed after {:.1}s parked for transcription",
                t.elapsed().as_secs_f64()
            );
        }
        // And the same shape one rank down: the renderer outranks the packer
        // while somebody is actually waiting on a chunk.
        //
        // This is worth having now that packs arrive without a client asking for
        // them. A wishlist resumed at boot can put twenty chapters in the pack
        // queue seconds after the port opens, and on two ARM cores an ffmpeg
        // encode running against a renderer that is stalled *under the playhead*
        // is a reader waiting longer for the chapter in their hands so that a
        // chapter for tonight can be filed. The policy, in one line: pack when
        // the renderer is ahead or idle, hold while it is behind.
        //
        // Only rule 1 counts as behind — the chunk the playhead is on. The
        // lookahead is 80 chunks and also reports "rendering", and a packer that
        // waited for *that* would never run on this box at all.
        let stalled = st.stalled_for();
        // Any book's job: a foreign encode costs the core exactly what a local
        // one does, and the listener stalled under the playhead is just as
        // stalled.
        let pending = !st.session().pack_queue.is_empty();
        if stalled > 0.0 && stalled < PACK_HOLD_MAX_S && pending {
            if held.is_none() {
                held = Some(Instant::now());
                tracing::info!("packer holding back: the renderer is stalled under the playhead");
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if let Some(t) = held.take() {
            tracing::info!(
                "packer resumed after {:.1}s held back for the renderer",
                t.elapsed().as_secs_f64()
            );
        }
        let Some((job, plan, title)) = ({
            let mut s = st.session();
            // The loaded book's first: its file is the one somebody may be
            // waiting for. Then a chapter of some *other* book that somebody
            // ordered, whose plan is read raw, like everything else that touches
            // a book this process is not holding.
            let loaded = s.pack_queue.iter().find(|j| s.is_loaded(j)).cloned();
            match loaded.or_else(|| s.pack_queue.first().cloned()) {
                None => {
                    st.build_ev.clear();
                    None
                }
                Some(job) if s.is_loaded(&job) => {
                    s.packing = Some(job.clone());
                    Some((job, s.plan.clone(), s.title.clone().unwrap_or_default()))
                }
                Some(job) => {
                    drop(s);
                    match crate::plancache::read_raw(&st.cfg.work, &job.key) {
                        Some(plan) => {
                            st.session().packing = Some(job.clone());
                            Some((job, plan, String::new()))
                        }
                        None => {
                            // No plan, so nothing that could be packed. Drop it
                            // rather than spin on it.
                            tracing::warn!("cannot pack {job}: no plan.json");
                            st.session().pack_queue.retain(|j| *j != job);
                            None
                        }
                    }
                }
            }
        }) else {
            continue;
        };
        let (key, ci) = (job.key.clone(), job.ci);

        let outcome = match plan.get(ci) {
            None => Err("chapter out of range".to_string()),
            Some(ch) => chapters::build(
                &st.cfg,
                &key,
                ci,
                &ch.chunks,
                &cache::chapter_dir(&st.cfg.work, &key, ci),
                &ch.display_title(),
                &title,
            )
            .map(|_| ch.chunks.len())
            .map_err(|e| format!("chapter {}: {e}", ci + 1)),
        };
        match outcome {
            Ok(n) => {
                st.session().build_error = None;
                // The m4a is what "download this chapter" is waiting for, on this
                // device and on every other one with the drawer open.
                st.bus.emit_render(
                    "packed",
                    json!({"key": key, "chapter": ci, "ok": true, "n": n, "status": "packed"}),
                );
                // The other moment a library row genuinely goes stale: an m4a
                // landed, so this chapter is now downloadable from any device.
                // Same direction as the renderer's — written here, never read.
                crate::library::rescan_book(&st, &key);
            }
            Err(msg) => {
                tracing::warn!("pack failed - {msg}");
                st.session().build_error = Some(msg.clone());
                st.bus.emit_render(
                    "packed",
                    json!({"key": key, "chapter": ci, "ok": false,
                           "error": msg, "status": "packed"}),
                );
            }
        }
        let (cur, cur_key, loaded) = {
            let mut s = st.session();
            s.packing = None;
            s.pack_queue.retain(|j| *j != job);
            // `build_want` holds bare chapter numbers of the loaded book, so it
            // is only this job's to clear if this job is the loaded book's.
            let loaded = s.is_loaded(&job);
            if loaded {
                s.build_want.remove(&ci);
            }
            (s.chapter, s.key(), loaded)
        };
        // A foreign chapter that packed has nothing left to want: the order was
        // "render it and pack it", and both have happened. Dropping the intent
        // here rather than leaving it for the renderer to notice is what keeps
        // `next_elsewhere` from walking a list that never shrinks.
        if !loaded {
            if let Some(db) = st.store() {
                if let Err(e) = db.drop_intent(&key, ci) {
                    tracing::warn!("could not clear the order for {key} ch{ci}: {e}");
                }
            }
            crate::wishlist::project(&st, &key);
        }
        // Whether it packed or not. A failed encode already leaves the queues
        // here rather than being retried forever in this process, and the file has
        // to say the same thing — a pack that fails on every restart is the one
        // loop a durable queue could otherwise run until someone noticed.
        crate::wishlist::save(&st);
        // The chapter being *read* is the one the packed-chapter gc must spare,
        // and it belongs to the loaded book — not to whichever book this job
        // happened to be for. Nothing loaded, nothing to spare.
        let keep: HashSet<String> = cur_key
            .map(|k| ChapterRef::new(k, cur).to_string())
            .into_iter()
            .collect();
        chapters::gc(&st.cfg, &keep);
    }
    st.build_started.store(false, Ordering::SeqCst);
}

/// Estimated seconds of the loaded book already in the streaming cache.
pub fn done_seconds(st: &AppState) -> f64 {
    let (plan, key) = {
        let s = st.session();
        (s.plan.clone(), s.key())
    };
    let Some(key) = key else { return 0.0 };
    let mut t = 0.0;
    for (ci, ch) in plan.iter().enumerate() {
        let d = cache::chapter_dir(&st.cfg.work, &key, ci);
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "wav") {
                if let Some(i) = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    if let Some(k) = ch.chunks.get(i) {
                        t += crate::book::est_chunk_s(k, st.cfg.silence_s);
                    }
                }
            }
        }
    }
    t
}

#[cfg(test)]
mod tests {
    //! The ranking, asked of [`choose`] directly: no worker thread, no sleep.

    use super::*;
    use crate::book::Chapter;
    use crate::config::Config;

    const CHUNKS: usize = 3;

    fn plan(chapters: usize) -> Vec<Chapter> {
        (0..chapters)
            .map(|i| Chapter {
                index: i,
                id: format!("c{i}"),
                title: format!("C{i}"),
                chunks: (0..CHUNKS)
                    .map(|_| Chunk {
                        text: "hello.".into(),
                        para: 0,
                        silent: false,
                    })
                    .collect(),
            })
            .collect()
    }

    /// Book A loaded, three chapters of three chunks, the playhead at the
    /// start; no prerender span and a lookahead of two.
    fn state() -> (tempfile::TempDir, Arc<AppState>) {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.lookahead = 2;
        cfg.prerender_chapters = 0;
        std::fs::create_dir_all(&cfg.work).expect("work");
        let st = AppState::new(cfg);
        {
            let mut s = st.session();
            s.book = Some("/books/A.epub".into());
            s.plan = Arc::new(plan(3));
        }
        (d, st)
    }

    fn seed(st: &AppState, key: &str, ci: usize) {
        for i in 0..CHUNKS {
            cache::write_wav(&cache::chunk_path(&st.cfg.work, key, ci, i), &[0.1f32; 240])
                .expect("wav");
        }
    }

    /// Book B, on disk only, with a standing order on its chapter 1.
    fn order_elsewhere(st: &AppState) {
        let p = cache::plan_path(&st.cfg.work, "B");
        std::fs::create_dir_all(p.parent().expect("dir")).expect("dir");
        std::fs::write(&p, serde_json::to_vec(&plan(3)).expect("json")).expect("plan");
        st.store()
            .expect("store")
            .add_intent("B", &[1], "", false, 1)
            .expect("intent");
    }

    /// The rule, book, chapter and chunk a choice renders, if it renders.
    fn picked(c: &Choice) -> Option<(Rule, &str, usize, usize)> {
        match c {
            Choice::Render { rule, at, idx, .. } => Some((*rule, at.key.as_str(), at.ci, *idx)),
            _ => None,
        }
    }

    #[test]
    fn a_missing_chunk_under_the_playhead_outranks_everything() {
        let (_d, st) = state();
        seed(&st, "A", 0);
        std::fs::remove_file(cache::chunk_path(&st.cfg.work, "A", 0, 1)).expect("hole");
        {
            let mut s = st.session();
            s.playhead = 1;
            s.queue = vec![2];
        }
        order_elsewhere(&st);
        let mut ctx = WorkerCtx::default();

        let c = choose(&mut ctx, &st);
        assert_eq!(picked(&c), Some((Rule::UnderPlayhead, "A", 0, 1)), "{c:?}");
        assert!(st.stalled_for() > 0.0, "and only this rule flags a stall");

        // Filled, the next rank down is the queue, and the stall is over.
        seed(&st, "A", 0);
        let c = choose(&mut ctx, &st);
        assert_eq!(picked(&c), Some((Rule::Queued, "A", 2, 0)), "{c:?}");
        assert_eq!(st.stalled_for(), 0.0);
    }

    #[test]
    fn a_standing_order_elsewhere_beats_the_rest_of_the_library() {
        let (_d, st) = state();
        seed(&st, "A", 0);
        order_elsewhere(&st);
        let mut ctx = WorkerCtx::default();

        let c = choose(&mut ctx, &st);
        assert_eq!(picked(&c), Some((Rule::Elsewhere, "B", 1, 0)), "{c:?}");

        // With the order gone, what is left is speculation on the loaded book.
        st.store()
            .expect("store")
            .drop_intent("B", 1)
            .expect("drop");
        let c = choose(&mut ctx, &st);
        assert_eq!(picked(&c), Some((Rule::Speculative, "A", 1, 0)), "{c:?}");
    }

    #[test]
    fn above_the_ceiling_there_is_nothing_to_speculate_on() {
        let (_d, st) = state();
        seed(&st, "A", 0);
        let mut ctx = WorkerCtx {
            // Measured a moment ago, so the number below is what it goes on.
            measured: Some(Instant::now()),
            ..WorkerCtx::default()
        };
        st.audio_bytes.store(u64::MAX / 2, Ordering::Relaxed);
        let c = choose(&mut ctx, &st);
        assert!(matches!(c, Choice::Idle(_)), "{c:?}");
        assert!(ctx.said_full);

        st.audio_bytes.store(0, Ordering::Relaxed);
        let c = choose(&mut ctx, &st);
        assert_eq!(picked(&c), Some((Rule::Speculative, "A", 1, 0)), "{c:?}");
        assert!(!ctx.said_full);
    }

    #[test]
    fn with_nothing_loaded_there_is_nothing_to_choose() {
        let (_d, st) = state();
        st.session().book = None;
        let c = choose(&mut WorkerCtx::default(), &st);
        assert!(matches!(c, Choice::Wait(_)), "{c:?}");
    }
}
