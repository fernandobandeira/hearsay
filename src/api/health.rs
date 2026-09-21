//! A check that can actually fail.
//!
//! A liveness probe that only proves the HTTP server is answering is worth very
//! little — it answers fine while the render thread is dead, and that is the
//! failure Fernando would notice hours later, as silence. This exercises the
//! parts that can wedge: the worker thread, forward progress, the packer, and a
//! writable work dir. The VPS watchdog restarts the container after three
//! consecutive failures.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::round1;
use crate::render;
use crate::state::AppState;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Health {
    pub ok: bool,
    pub problems: Vec<String>,
    pub status: String,
    pub uptime_s: f64,
    pub model_ready: bool,
    #[schema(required = true)]
    pub book: Option<String>,
    pub threads: Vec<String>,
    pub since_progress_s: f64,
    pub stall_limit_s: f64,
    #[schema(required = true)]
    pub build_error: Option<String>,
    #[schema(required = true)]
    pub packing: Option<usize>,
    pub queue: usize,
    /// Open SSE streams.
    pub live: usize,
    /// Where positions and notes are being written. `null` means the vault is
    /// not mounted and they are falling back under the work dir — which is a
    /// thing to *say*, not to hide: with NARRATOR_VAULT pointing at a path that
    /// does not exist, the python server writes nothing, reads back `{}`, and
    /// every book quietly opens at chapter one.
    pub vault: Option<String>,
    pub positions_dir: String,
    /// Whether `work/state.db` opened.
    ///
    /// **Not** a problem when it is false, deliberately — everything that
    /// mattered before the store existed still works without it, and refusing to
    /// be healthy over a degraded extra would have the watchdog restart the
    /// container in a loop for a fault a restart cannot fix. It is reported
    /// because the alternative is a box that quietly stops remembering standing
    /// orders and answering for the library, with nothing anywhere saying so.
    pub store: bool,
    /// How many devices are holding an event stream right now.
    ///
    /// `live` counts subscribers to the bus; this counts the ones that said who
    /// they are. The two disagreeing means something is connected that is not
    /// this reader — the Obsidian plugin, a stray `curl` — which is worth being
    /// able to see when a position seems to be moving on its own.
    pub devices: usize,
}

#[utoipa::path(
    get, path = "/healthz", tag = "health",
    responses(
        (status = 200, body = Health),
        (status = 503, body = Health, description = "something that matters is wedged"),
    )
)]
pub async fn healthz(State(st): State<Arc<AppState>>) -> Response {
    let since = st.since_progress();
    let (status, error, berr, ready, book, want, queue, packing) = {
        let s = st.session();
        let want = !s.plan.is_empty()
            && s.plan
                .get(s.chapter)
                .is_some_and(|c| s.render_idx < c.chunks.len());
        (
            s.status.clone(),
            s.error.clone(),
            s.build_error.clone(),
            s.model_ready,
            s.book_name(),
            want,
            s.queue.len(),
            s.building,
        )
    };
    let mut threads = Vec::new();
    if render::render_alive(&st) {
        threads.push("render".to_string());
    }
    if st.build_started.load(std::sync::atomic::Ordering::SeqCst) {
        threads.push("build".to_string());
    }
    threads.sort();

    let mut problems = Vec::new();
    if status == "error" {
        problems.push(format!(
            "render worker died: {}",
            error.unwrap_or_else(|| "unknown".into())
        ));
    }
    if st.run.is_set() && want && !threads.iter().any(|t| t == "render") {
        problems.push("render worker not running while work is queued".into());
    }
    // "rendering" that has not produced a chunk in minutes is a wedge, not work:
    // a single chunk is a second or two even on the slowest box.
    //
    // Unless a voice memo is being transcribed, in which case the renderer is
    // parked on purpose and *should* not be producing chunks. Whisper runs at
    // 30–50× realtime on the A1, so a one-minute memo is half an hour of not
    // rendering — comfortably past `HEALTH_STALL_S`, and the watchdog restarting
    // the container over it would kill the transcription every time it was
    // retried. A stall that is explained is not a stall.
    if matches!(status.as_str(), "rendering" | "prerendering" | "queued")
        && since > st.cfg.health_stall_s
        && !st.whisper.gate().held()
    {
        problems.push(format!(
            "no chunk finished in {}s while status={status}",
            since as i64
        ));
    }
    if let Err(e) = std::fs::create_dir_all(&st.cfg.work)
        .and_then(|()| std::fs::write(st.cfg.work.join(".healthz"), b"1"))
    {
        problems.push(format!("work dir not writable: {e}"));
    }
    // Positions that cannot be written are positions that do not exist, and the
    // symptom - every book opening at chapter one - looks nothing like the
    // cause. Probe the directory that is actually being used.
    if let Err(e) = std::fs::create_dir_all(&st.cfg.positions_dir)
        .and_then(|()| std::fs::write(st.cfg.positions_dir.join(".healthz"), b"1"))
    {
        problems.push(format!(
            "positions dir {} not writable: {e}",
            st.cfg.positions_dir.display()
        ));
    }

    let body = Health {
        ok: problems.is_empty(),
        status,
        uptime_s: round1(st.started_at.elapsed().as_secs_f64()),
        model_ready: ready,
        book,
        threads,
        since_progress_s: round1(since),
        stall_limit_s: st.cfg.health_stall_s,
        build_error: berr,
        packing,
        queue,
        live: st.bus.subscribers(),
        vault: st.cfg.vault.as_ref().map(|p| p.display().to_string()),
        positions_dir: st.cfg.positions_dir.display().to_string(),
        store: st.store().is_some(),
        devices: st.roster.len(),
        problems,
    };
    let code = if body.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body)).into_response()
}
