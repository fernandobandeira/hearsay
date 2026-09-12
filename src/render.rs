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
use crate::state::AppState;

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
    let (key, playhead) = {
        let s = st.session();
        (s.key(), s.playhead)
    };
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
        crate::wishlist::progress(st, ci);
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
    let keep = st.session().gc_keep(&st.cfg);
    cache::gc_audio(&st.cfg.work, st.cfg.max_audio_gb, &keep);
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
            s.building.is_some() || !s.pack_queue.is_empty() || !s.queue.is_empty(),
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

/// Ask the packer for chapter `ci`'s m4a. Idempotent; starts the thread.
pub fn enqueue_build(st: &Arc<AppState>, ci: usize) {
    {
        let mut s = st.session();
        if !s.pack_queue.contains(&ci) {
            s.pack_queue.push(ci);
        }
    }
    st.build_ev.set();
    ensure_build_thread(st);
}

// ----------------------------------------------------------------- the worker

fn worker(st: Arc<AppState>) {
    st.engine.load();
    st.session().model_ready = st.engine.ready();
    let mut pre = 0usize;
    let mut bo = Backoff::default();
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

        let (ci, hint, ph, plan, key) = {
            let s = st.session();
            (s.chapter, s.render_idx, s.playhead, s.plan.clone(), s.key())
        };
        let Some(key) = key else {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        };
        // Renders have been failing: wait before trying again. Nothing is
        // skipped and nothing is given up on — the loop comes straight back
        // round to rule 1 — it just does not do it thousands of times a second.
        if !bo.ready() {
            std::thread::sleep(bo.nap());
            continue;
        }
        let chunks: &[Chunk] = plan.get(ci).map(|c| c.chunks.as_slice()).unwrap_or(&[]);
        let n = chunks.len();

        // 1. Disk truth: the chunk under the playhead outranks everything. If it
        //    is missing the reader is stalled on it right now.
        if ph < n && !exists(&st, &key, ci, ph) {
            st.session().status = "rendering".into();
            attempt(&st, &key, ci, ph, chunks, &mut bo);
            render_event(&st, "progress", ci, ph + 1, n, "rendering");
            continue;
        }

        // 2. The lookahead window, scanned for a real hole rather than trusted.
        let limit = n.min(ph.saturating_add(st.cfg.lookahead).saturating_add(1));
        let target = first_missing(&st, &key, ci, hint.min(limit), limit)
            .or_else(|| first_missing(&st, &key, ci, ph, limit));
        if let Some(i) = target {
            {
                let mut s = st.session();
                s.status = "rendering".into();
                s.prerender = None;
            }
            attempt(&st, &key, ci, i, chunks, &mut bo);
            let next = {
                let mut s = st.session();
                // Only advance if nothing moved the hint while we were rendering:
                // a forward jump from /api/playhead must not be clobbered by the
                // stale i+1 computed several seconds ago.
                if s.render_idx <= i {
                    s.render_idx = i + 1;
                }
                s.render_idx
            };
            if next >= n {
                render_event(&st, "complete", ci, next, n, "rendering");
                // The chapter just became packable and the renderer is about to
                // go do speculative work; pack it now, while it matters.
                autopack(&st, true);
            } else {
                render_event(&st, "progress", ci, next, n, "rendering");
            }
            if next % 25 == 0 {
                gc(&st);
            }
            continue;
        }

        // 3. The playhead has all the buffer it asked for. Chapters the reader
        //    named in the chapter manager come next — an explicit offline request
        //    outranks the speculative span.
        if let Some((cj, j)) = next_queued(&st, &key) {
            {
                let mut s = st.session();
                s.status = "queued".into();
                s.prerender = Some(cj);
            }
            let qn = plan.get(cj).map(|c| c.chunks.len()).unwrap_or(0);
            attempt(
                &st,
                &key,
                cj,
                j,
                plan.get(cj).map(|c| c.chunks.as_slice()).unwrap_or(&[]),
                &mut bo,
            );
            render_event(&st, "progress", cj, j + 1, qn, "queued");
            pre += 1;
            if pre % 25 == 0 {
                gc(&st);
            }
            continue;
        }

        // 4. Buffered ahead within this chapter, nothing asked for.
        if n > 0 && first_missing(&st, &key, ci, 0, n).is_some() {
            {
                let mut s = st.session();
                s.status = "ready".into();
                s.prerender = None;
            }
            autopack(&st, false);
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }

        // 5. This chapter is fully rendered. Rather than idle, build the buffer
        //    into the chapters ahead — that head start is what keeps playback
        //    continuous across a chapter boundary.
        let span = st.session().prerender_span(ci, &st.cfg);
        match next_ahead(&st, &key, ci, span) {
            None => {
                {
                    let mut s = st.session();
                    s.status = "ready".into();
                    s.prerender = None;
                }
                autopack(&st, false);
                std::thread::sleep(Duration::from_secs(1));
            }
            Some((cj, j)) => {
                {
                    let mut s = st.session();
                    s.status = "prerendering".into();
                    s.prerender = Some(cj);
                }
                let an = plan.get(cj).map(|c| c.chunks.len()).unwrap_or(0);
                attempt(
                    &st,
                    &key,
                    cj,
                    j,
                    plan.get(cj).map(|c| c.chunks.as_slice()).unwrap_or(&[]),
                    &mut bo,
                );
                render_event(&st, "progress", cj, j + 1, an, "prerendering");
                pre += 1;
                if pre % 25 == 0 {
                    gc(&st);
                }
            }
        }
    }
    st.render_started.store(false, Ordering::SeqCst);
}

// ----------------------------------------------------------------- the packer

fn builder(st: Arc<AppState>) {
    let mut parked: Option<Instant> = None;
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
        let Some((ci, plan, key, title)) = ({
            let mut s = st.session();
            match s.pack_queue.first().copied() {
                None => {
                    st.build_ev.clear();
                    None
                }
                Some(ci) => {
                    s.building = Some(ci);
                    Some((
                        ci,
                        s.plan.clone(),
                        s.key_or_x(),
                        s.title.clone().unwrap_or_default(),
                    ))
                }
            }
        }) else {
            continue;
        };

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
        let cur = {
            let mut s = st.session();
            s.building = None;
            s.pack_queue.retain(|c| *c != ci);
            s.build_want.remove(&ci);
            s.chapter
        };
        // Whether it packed or not. A failed encode already leaves the queues
        // here rather than being retried forever in this process, and the file
        // has to say the same thing — a pack that fails on every restart is the
        // one loop a durable queue could otherwise run until someone noticed.
        crate::wishlist::save(&st);
        let keep: HashSet<String> = [format!("{key}/ch{cur:03}")].into_iter().collect();
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
