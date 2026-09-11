//! The chapter manager's data source and its three verbs.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{err, round1, round3, ApiError};
use crate::cache;
use crate::chapters as pack;
use crate::render;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChapterRow {
    pub i: usize,
    pub title: String,
    pub n: usize,
    /// How many of this chapter's chunks are on disk.
    pub rendered: usize,
    #[schema(required = true)]
    pub est_min: Option<f64>,
    /// Whether a *trustworthy* packed m4a exists. A manifest whose `chunks`
    /// disagrees with the plan reports false: it points at the wrong words.
    pub m4a: bool,
    #[schema(required = true)]
    pub bytes: Option<u64>,
    #[schema(required = true)]
    pub duration: Option<f64>,
    /// Always present: a row only exists for a loaded book, and then the
    /// chapter manager's three flags always have an answer.
    pub queued: bool,
    pub packing: bool,
    pub pack_queued: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ChaptersResult {
    #[schema(required = true)]
    pub book: Option<String>,
    #[schema(required = true)]
    pub key: Option<String>,
    #[schema(required = true)]
    pub title: Option<String>,
    // Omitted in the "no book loaded" shape and a plain number otherwise; the
    // value_type override keeps `null` out of the generated client, which the
    // reader's hand-written type does not admit either.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = usize)]
    pub chapter: Option<usize>,
    pub chapters: Vec<ChapterRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Vec<usize>)]
    pub queue: Option<Vec<usize>>,
    /// Absent only in the "no book loaded" shape; `null` when nothing is being
    /// packed. The python server distinguishes those two and the reader's type
    /// says `building?: number | null`, so the nesting is the contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<usize>)]
    pub building: Option<Option<usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub build_error: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = f64)]
    pub chapters_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = f64)]
    pub chapters_cap_gb: Option<f64>,
    /// The window this response covers, when `?from=`/`?to=` narrowed it.
    /// Additive: without the query these are 0 and the last chapter index, and
    /// `chapters` is the whole book exactly as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = usize)]
    pub from: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = usize)]
    pub to: Option<usize>,
    /// How many chapters the book has, whatever the window is.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = usize)]
    pub total: Option<usize>,
}

/// `?from=&to=` on the chapter list. The drawer polls this every two seconds
/// while it is open, and for 1433 chapters the unwindowed scan is a few thousand
/// `stat` calls; a reader that only shows a screenful never needs the rest.
/// Both bounds are inclusive and clamped to the book.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct RangeQuery {
    pub from: Option<usize>,
    pub to: Option<usize>,
}

/// Per-chapter render/pack state, memoised for 1.5 s: scanning a 1400-chapter
/// cache is a few thousand stat calls and the drawer polls this.
pub fn chapter_rows(st: &Arc<AppState>) -> Vec<ChapterRow> {
    let (plan, est, key) = {
        let s = st.session();
        (s.plan.clone(), s.est_s.clone(), s.key_or_x())
    };
    if let Ok(g) = st.chstat.lock() {
        if let Some((t, k, rows)) = g.as_ref() {
            if *k == key && t.elapsed().as_secs_f64() < 1.5 {
                return rows.clone();
            }
        }
    }
    let mut rows = Vec::with_capacity(plan.len());
    for (ci, ch) in plan.iter().enumerate() {
        let n = ch.chunks.len();
        let have = cache::rendered_count(&cache::chapter_dir(&st.cfg.work, &key, ci), n);
        let (m4a, _) = pack::chapter_files(&st.cfg.work, &key, ci);
        let size = m4a.metadata().ok().map(|m| m.len());
        let mut row = ChapterRow {
            i: ci,
            title: ch.display_title(),
            n,
            rendered: have,
            est_min: est.get(ci).map(|e| round1(e / 60.0)),
            m4a: size.is_some(),
            bytes: size,
            duration: None,
            queued: false,
            packing: false,
            pack_queued: false,
        };
        if size.is_some() {
            if let Some(man) = pack::read_manifest(&st.cfg.work, &key, ci) {
                row.duration = Some(man.duration);
                // A manifest from before the chapter was re-rendered is a lie
                // about where the chunks are; flag it rather than seek to the
                // wrong words.
                if man.chunks != n {
                    row.m4a = false;
                }
            }
        }
        rows.push(row);
    }
    if let Ok(mut g) = st.chstat.lock() {
        *g = Some((Instant::now(), key, rows.clone()));
    }
    rows
}

/// Every chapter of the loaded book: chunks rendered / total, whether the packed
/// m4a exists and how big it is.
#[utoipa::path(
    get, path = "/api/chapters", tag = "chapters",
    params(RangeQuery),
    responses((status = 200, body = ChaptersResult))
)]
pub async fn chapters_list(
    State(st): State<Arc<AppState>>,
    Query(range): Query<RangeQuery>,
) -> Response {
    let empty = st.session().plan.is_empty();
    if empty {
        return Json(ChaptersResult {
            book: None,
            key: None,
            title: None,
            chapter: None,
            chapters: Vec::new(),
            queue: None,
            building: None,
            build_error: None,
            chapters_gb: None,
            chapters_cap_gb: None,
            from: None,
            to: None,
            total: None,
        })
        .into_response();
    }
    let st2 = st.clone();
    let rows = tokio::task::spawn_blocking(move || chapter_rows(&st2))
        .await
        .unwrap_or_default();
    let s = st.session();
    let (queue, want, bq) = (s.queue.clone(), s.build_want.clone(), s.pack_queue.clone());
    let total = rows.len();
    let from = range.from.unwrap_or(0).min(total.saturating_sub(1));
    let to = range
        .to
        .unwrap_or(total.saturating_sub(1))
        .min(total.saturating_sub(1));
    let rows = rows
        .into_iter()
        .filter(|r| r.i >= from && r.i <= to)
        .map(|mut r| {
            r.queued = queue.contains(&r.i);
            r.packing = s.building == Some(r.i);
            r.pack_queued = bq.contains(&r.i) || want.contains(&r.i);
            r
        })
        .collect();
    Json(ChaptersResult {
        book: s.book_name(),
        key: s.key(),
        title: s.title.clone(),
        chapter: Some(s.chapter),
        chapters: rows,
        queue: Some(queue),
        building: Some(s.building),
        build_error: Some(s.build_error.clone()),
        chapters_gb: Some(round3(
            pack::total_bytes(&st.cfg.work) as f64 / 1024.0_f64.powi(3),
        )),
        chapters_cap_gb: Some(st.cfg.max_chapter_gb),
        from: Some(from),
        to: Some(to),
        total: Some(total),
    })
    .into_response()
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ChapterSetBody {
    #[serde(default)]
    pub chapters: Option<Vec<i64>>,
    /// `/api/chapters/render` only: also pack each chapter as it completes.
    #[serde(default)]
    pub pack: Option<bool>,
    /// `/api/chapters/build` only: rebuild even if an m4a already exists.
    #[serde(default)]
    pub force: Option<bool>,
}

/// Validate a `{chapters: [...]}` body against the loaded book: de-duplicated,
/// in the order given, bounds-checked.
fn wanted(body: &ChapterSetBody, n: usize) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    body.chapters
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| *c >= 0 && (*c as usize) < n)
        .map(|c| c as usize)
        .filter(|c| seen.insert(*c))
        .collect()
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RenderResult {
    pub ok: bool,
    pub queue: Vec<usize>,
    /// Chapters that were already complete and went straight to the packer.
    pub packing: Vec<usize>,
}

/// Queue chapters for rendering, in the order given. `pack: true` also packs
/// each one as it completes — that is what "download offline" asks for first.
#[utoipa::path(
    post, path = "/api/chapters/render", tag = "chapters",
    request_body = ChapterSetBody,
    responses((status = 200, body = RenderResult), (status = 400, body = ApiError))
)]
pub async fn chapters_render(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ChapterSetBody>,
) -> Response {
    match queue_chapters(&st, &body).await {
        Err(e) => e,
        Ok(r) => Json(r).into_response(),
    }
}

async fn queue_chapters(
    st: &Arc<AppState>,
    body: &ChapterSetBody,
) -> Result<RenderResult, Response> {
    let n = st.session().plan.len();
    if n == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "no book loaded"));
    }
    let want = wanted(body, n);
    let pack = body.pack.unwrap_or(false);
    {
        let mut s = st.session();
        for c in &want {
            if !s.queue.contains(c) {
                s.queue.push(*c);
            }
            if pack {
                s.build_want.insert(*c);
            }
        }
    }
    let mut ready = Vec::new();
    if pack {
        let st2 = st.clone();
        let rows = tokio::task::spawn_blocking(move || chapter_rows(&st2))
            .await
            .unwrap_or_default();
        for c in &want {
            let Some(r) = rows.iter().find(|r| r.i == *c) else {
                continue;
            };
            // Anything already complete never reaches the worker — pack it now.
            if r.rendered >= r.n && !r.m4a {
                render::enqueue_build(st, *c);
            }
            if r.rendered >= r.n {
                ready.push(*c);
                st.session().queue.retain(|q| q != c);
            }
        }
    }
    st.run.set();
    render::ensure_render_thread(st);
    Ok(RenderResult {
        ok: true,
        queue: st.session().queue.clone(),
        packing: ready,
    })
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BuildResult {
    pub ok: bool,
    /// Already packed; nothing to do.
    pub built: Vec<usize>,
    /// Complete and handed to the packer now.
    pub building: Vec<usize>,
    /// Not complete; queued for rendering first.
    pub rendering: Vec<usize>,
}

/// Pack now what is complete, queue the rest for rendering first.
#[utoipa::path(
    post, path = "/api/chapters/build", tag = "chapters",
    request_body = ChapterSetBody,
    responses((status = 200, body = BuildResult), (status = 400, body = ApiError))
)]
pub async fn chapters_build(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ChapterSetBody>,
) -> Response {
    let n = st.session().plan.len();
    if n == 0 {
        return err(StatusCode::BAD_REQUEST, "no book loaded");
    }
    let want = wanted(&body, n);
    let st2 = st.clone();
    let rows = tokio::task::spawn_blocking(move || chapter_rows(&st2))
        .await
        .unwrap_or_default();
    let force = body.force.unwrap_or(false);
    let (mut building, mut rendering, mut done) = (Vec::new(), Vec::new(), Vec::new());
    for c in want {
        let r = rows.iter().find(|r| r.i == c);
        match r {
            Some(r) if r.m4a && !force => done.push(c),
            Some(r) if r.rendered >= r.n => {
                render::enqueue_build(&st, c);
                building.push(c);
            }
            _ => rendering.push(c),
        }
    }
    if !rendering.is_empty() {
        let b = ChapterSetBody {
            chapters: Some(rendering.iter().map(|c| *c as i64).collect()),
            pack: Some(true),
            force: None,
        };
        if let Err(e) = queue_chapters(&st, &b).await {
            return e;
        }
    }
    Json(BuildResult {
        ok: true,
        built: done,
        building,
        rendering,
    })
    .into_response()
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CancelResult {
    pub ok: bool,
    pub queue: Vec<usize>,
}

/// Drop chapters from the queues. An empty body clears both.
#[utoipa::path(
    post, path = "/api/chapters/cancel", tag = "chapters",
    request_body = ChapterSetBody, responses((status = 200, body = CancelResult))
)]
pub async fn chapters_cancel(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ChapterSetBody>,
) -> Json<CancelResult> {
    let mut s = st.session();
    let n = s.plan.len();
    let want = body
        .chapters
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|_| wanted(&body, n));
    match want {
        None => {
            s.queue.clear();
            s.build_want.clear();
            s.pack_queue.clear();
        }
        Some(w) => {
            s.queue.retain(|c| !w.contains(c));
            for c in &w {
                s.build_want.remove(c);
            }
            let building = s.building;
            s.pack_queue
                .retain(|c| !w.contains(c) || Some(*c) == building);
        }
    }
    Json(CancelResult {
        ok: true,
        queue: s.queue.clone(),
    })
}
