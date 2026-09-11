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
    pub book: Option<String>,
    pub threads: Vec<String>,
    pub since_progress_s: f64,
    pub stall_limit_s: f64,
    pub build_error: Option<String>,
    pub packing: Option<usize>,
    pub queue: usize,
    /// Open SSE streams.
    pub live: usize,
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
    if matches!(status.as_str(), "rendering" | "prerendering" | "queued")
        && since > st.cfg.health_stall_s
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
        problems,
    };
    let code = if body.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body)).into_response()
}
