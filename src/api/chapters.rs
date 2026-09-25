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
use crate::api::device::Device;
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
    /// What this chapter will weigh once packed, at the server's own
    /// `CHAPTER_BITRATE` — the arithmetic the client used to do with a
    /// hard-coded 64 kbit/s it could not see. `bytes` is the measured size of a
    /// chapter that exists; this is the estimate for one that does not, and it
    /// is null only when the duration estimate itself is missing.
    #[schema(required = true)]
    pub est_bytes: Option<u64>,
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
    /// Present, and `true`, only for a chapter the server has stopped taking up
    /// at startup: it was asked for, it has been picked back up by five restarts
    /// without a single chunk of it landing, and something about it is not
    /// working. Still wanted, no longer tried — asking for the chapter again is
    /// the retry and clears this. Absent is the ordinary answer, and absent
    /// rather than `false` so a 1433-row response pays nothing for it, which is
    /// why it is not a plain `bool` like its three neighbours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<bool>,
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

/// `?from=&to=&book=` on the chapter list. The drawer polls this every two
/// seconds while it is open, and for 1433 chapters the unwindowed scan is a few
/// thousand `stat` calls; a reader that only shows a screenful never needs the
/// rest. Both bounds are inclusive and clamped to the book.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ChaptersQuery {
    pub from: Option<usize>,
    pub to: Option<usize>,
    /// The book the answer is meant to be about. Supplied and mismatched, the
    /// response is a **409** rather than a confident description of the wrong
    /// novel — which is what a poll that raced a book switch used to get, and
    /// what the tap acting on it would then have aimed at.
    pub book: Option<String>,
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
    let per_min = pack::bytes_per_minute(&st.cfg.chapter_bitrate);
    let mut rows = Vec::with_capacity(plan.len());
    for (ci, ch) in plan.iter().enumerate() {
        let n = ch.chunks.len();
        let have = cache::rendered_count(&cache::chapter_dir(&st.cfg.work, &key, ci), n);
        let (m4a, _) = pack::chapter_files(&st.cfg.work, &key, ci);
        let size = m4a.metadata().ok().map(|m| m.len());
        let est_min = est.get(ci).map(|e| round1(e / 60.0));
        let mut row = ChapterRow {
            i: ci,
            title: ch.display_title(),
            n,
            rendered: have,
            est_min,
            est_bytes: est_min.map(|m| (m * per_min).round() as u64),
            m4a: size.is_some(),
            bytes: size,
            duration: None,
            queued: false,
            packing: false,
            pack_queued: false,
            parked: None,
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
    params(ChaptersQuery),
    responses((status = 200, body = ChaptersResult),
              (status = 409, body = ApiError, description = "the session holds another book. \
This is a *read*: to ask about a book that is not loaded, use `/api/library`, \
which answers for the whole library out of the scanned index."))
)]
pub async fn chapters_list(
    State(st): State<Arc<AppState>>,
    Query(range): Query<ChaptersQuery>,
) -> Response {
    if let Some(r) = super::session::wrong_book(&st, range.book.as_deref()) {
        return r;
    }
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
    // The size of the chapters tree is a walk of every packed file in the
    // library, so it is taken here, on the blocking pool beside the row scan —
    // not below, where it used to run on the runtime with the session lock held,
    // once every two seconds for as long as the drawer is open.
    let (rows, chapter_bytes) =
        tokio::task::spawn_blocking(move || (chapter_rows(&st2), pack::total_bytes(&st2.cfg.work)))
            .await
            .unwrap_or_default();
    // Before the session lock, and outside the row memo: a parked chapter is in
    // neither of the three queues by definition, so the memoised scan cannot
    // know about it, and a restart must not be able to make the drawer look like
    // nothing was ever asked for.
    let parked = st.wishlist().parked();
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
            r.parked = parked.contains(&r.i).then_some(true);
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
        chapters_gb: Some(round3(chapter_bytes as f64 / 1024.0_f64.powi(3))),
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
    /// The book these chapters belong to. **Additive, and the one worth
    /// sending**: this is the endpoint where a race costs hours. The reader
    /// polls `/api/chapters`, the reader taps "download the rest", and in
    /// between the watcher may have picked up an epub, the Obsidian plugin may
    /// have opened something, another device may have loaded another book — and
    /// 74 chapters of rendering then land on that one. Omitted, it means
    /// "whatever is loaded", as before.
    ///
    /// Supplied and naming a *different* book that the library knows, the order
    /// is placed on **that** book — the worker follows standing orders across
    /// the library and the packer packs them, so naming one is a request the
    /// server can honour rather than a race it has to refuse. Supplied and
    /// naming nothing the library has, it is a **409**, which is now a real
    /// refusal rather than a limitation wearing one's clothes.
    #[serde(default)]
    pub book: Option<String>,
}

/// The raw `{chapters: [...]}` list, de-duplicated and in the order given, with
/// nothing thrown away — including the indices no chapter answers to, which is
/// what lets `/api/chapters/build` say `out_of_range` instead of nothing.
fn asked(body: &ChapterSetBody) -> Vec<i64> {
    let mut seen = std::collections::HashSet::new();
    body.chapters
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| seen.insert(*c))
        .collect()
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

/// Place a standing order on a book the session is **not** holding.
///
/// `wrong_book` answers 409 for a named book that is not the loaded one, and
/// that was right while it was the only answer available: acting on the loaded
/// book when the client meant another one is the expensive mistake this endpoint
/// exists to prevent — one tap is 74 chapters of rendering, and on the wrong
/// novel that is an afternoon of the worker.
///
/// But refusing was only ever half of the right answer. The client named a book;
/// the correct thing is to act on **that** one, and since the worker learned to
/// follow orders across the library (`next_elsewhere`) and the packer to pack
/// them, nothing stands in the way. A 409 here now means "no such book",
/// which is a real refusal, rather than "not the one in front of me", which was
/// a limitation wearing a refusal's clothes.
///
/// Returns `None` when the key is not one the library knows, leaving the caller
/// to refuse as before.
#[allow(clippy::result_large_err)]
fn order_elsewhere(
    st: &Arc<AppState>,
    key: &str,
    body: &ChapterSetBody,
    dev: &Device,
    pack: bool,
) -> Option<Result<RenderResult, Response>> {
    let db = st.store()?;
    // Known to the library *and* backed by a plan. Both matter: the register
    // says the book exists, and `plan.json` is what the chapter numbers in the
    // request are indices into. Without the second, a request could queue
    // chapter 900 of a four-chapter book.
    db.book(key).ok().flatten()?;
    let plan = crate::plancache::read_raw(&st.cfg.work, key)?;
    let want = wanted(body, plan.len());
    if want.is_empty() {
        return Some(Err(err(
            StatusCode::BAD_REQUEST,
            "no chapters in range for that book",
        )));
    }
    let now_ms = chrono::Local::now().timestamp_millis();
    if let Err(e) = db.add_intent(key, &want, &dev.id, pack, now_ms) {
        return Some(Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not record the order: {e}"),
        )));
    }
    // Anything already rendered never reaches the worker, exactly as on the
    // loaded book's path — hand it straight to the packer rather than leaving it
    // to be noticed.
    let mut ready = Vec::new();
    if pack {
        for ci in &want {
            let n = plan.get(*ci).map(|c| c.chunks.len()).unwrap_or(0);
            if n == 0 {
                continue;
            }
            let dir = cache::chapter_dir(&st.cfg.work, key, *ci);
            if cache::rendered_count(&dir, n) >= n
                && !pack::chapter_packed(&st.cfg.work, key, *ci, n)
            {
                render::enqueue_build_elsewhere(st, key, *ci);
                ready.push(*ci);
            }
        }
    }
    st.run.set();
    render::ensure_render_thread(st);
    let queue = db
        .intents(key)
        .map(|rows| rows.into_iter().map(|r| r.chapter).collect())
        .unwrap_or_else(|_| want.clone());
    Some(Ok(RenderResult {
        ok: true,
        queue,
        packing: ready,
    }))
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
    responses((status = 200, body = RenderResult), (status = 400, body = ApiError),
              (status = 409, body = ApiError, description = "no such book — the name given is \
neither the loaded book nor one the library knows. A *known* book that is not the \
loaded one is acted on rather than refused: the order goes to that book and the \
worker picks it up."))
)]
pub async fn chapters_render(
    State(st): State<Arc<AppState>>,
    dev: Device,
    Json(body): Json<ChapterSetBody>,
) -> Response {
    if let Some(r) = super::session::wrong_book(&st, body.book.as_deref()) {
        // Named, and not the loaded book — but the library may still know it, in
        // which case acting on it is what was asked for. See `order_elsewhere`.
        let key = cache::safe_key(body.book.as_deref().unwrap_or_default());
        let pack = body.pack.unwrap_or(false);
        return match order_elsewhere(&st, &key, &body, &dev, pack) {
            Some(Ok(r)) => Json(r).into_response(),
            Some(Err(e)) => e,
            None => r,
        };
    }
    match queue_chapters(&st, &body).await {
        Err(e) => e,
        Ok(r) => Json(r).into_response(),
    }
}

// The `Err` carries a built axum `Response` - that is the point: this helper
// exists so the handler can `?`-style early-return a 400 it has already shaped.
// `Response` is ~128 bytes, which trips `result_large_err`, but the value never
// leaves the handler above (it is matched and returned one frame up), so boxing
// it would buy an allocation and nothing else.
#[allow(clippy::result_large_err)]
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
    // Written down before the answer goes out. A 200 here is Fernando walking
    // away from the phone believing the box has the night's work, and the queue
    // used to live only in this process's memory — a deploy ten seconds later
    // and none of it had ever happened. Asking also clears any parking record
    // for these chapters: the button is the retry.
    crate::wishlist::asked(st, &want);
    st.run.set();
    render::ensure_render_thread(st);
    Ok(RenderResult {
        ok: true,
        queue: st.session().queue.clone(),
        packing: ready,
    })
}

/// One chapter the packer would not take, and why.
///
/// The list this belongs to is the answer to the reader's oldest complaint
/// about this endpoint: a chapter that was not packable appeared in none of
/// `built`/`building`/`rendering` and so was **indistinguishable from a chapter
/// nobody asked about**. The client's only recourse was to re-ask every twenty
/// seconds and read the next `/api/chapters` poll to find out what happened.
/// With a reason per chapter it can tell "taken" from "not yet, and here is how
/// far it got", and keep the re-ask for a genuine stall.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BuildRefusal {
    pub chapter: usize,
    /// `not_rendered` — queued for rendering instead, and `rendered`/`n` say how
    /// far it is. `no_chunks` — the chapter has no speakable text, so there is
    /// nothing to pack, ever. `out_of_range` — no such chapter in this book.
    pub reason: String,
    /// How many of the chapter's chunks are on disk, and how many there are.
    /// Both 0 for `out_of_range`.
    pub rendered: usize,
    pub n: usize,
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
    /// What was refused, and why — one entry per chapter this call did **not**
    /// hand to the packer. A chapter in `rendering` appears here too, with
    /// `not_rendered` and its progress: it was taken, but not for packing.
    pub refused: Vec<BuildRefusal>,
}

/// Pack now what is complete, queue the rest for rendering first.
#[utoipa::path(
    post, path = "/api/chapters/build", tag = "chapters",
    request_body = ChapterSetBody,
    responses((status = 200, body = BuildResult), (status = 400, body = ApiError),
              (status = 409, body = ApiError, description = "no such book — the name given is \
neither the loaded book nor one the library knows. A *known* book that is not the \
loaded one is acted on rather than refused: the order goes to that book and the \
worker picks it up."))
)]
pub async fn chapters_build(
    State(st): State<Arc<AppState>>,
    dev: Device,
    Json(body): Json<ChapterSetBody>,
) -> Response {
    if let Some(r) = super::session::wrong_book(&st, body.book.as_deref()) {
        // A build on a book that is not loaded is the same standing order a
        // render with `pack: true` places — the packer cannot encode a chapter
        // whose chunks are not all there, so "pack it" and "render it and then
        // pack it" are one request for any book the worker has to visit anyway.
        // What it is *not* is a refusal, which is all this could say before.
        let key = cache::safe_key(body.book.as_deref().unwrap_or_default());
        return match order_elsewhere(&st, &key, &body, &dev, true) {
            Some(Ok(o)) => Json(BuildResult {
                ok: true,
                built: Vec::new(),
                building: o.packing,
                rendering: o.queue,
                refused: Vec::new(),
            })
            .into_response(),
            Some(Err(e)) => e,
            None => r,
        };
    }
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
    // `want` is consumed below; the wishlist wants the same list at the end.
    let all = want.clone();
    let (mut building, mut rendering, mut done) = (Vec::new(), Vec::new(), Vec::new());
    let mut refused = Vec::new();
    // Asked-for indices that no chapter answers to. `wanted()` drops them, and
    // dropping them in silence is exactly the shape of failure this list is for.
    for c in asked(&body) {
        if c < 0 || c as usize >= n {
            refused.push(BuildRefusal {
                chapter: c.max(0) as usize,
                reason: "out_of_range".into(),
                rendered: 0,
                n: 0,
            });
        }
    }
    for c in want {
        let r = rows.iter().find(|r| r.i == c);
        match r {
            Some(r) if r.m4a && !force => done.push(c),
            Some(r) if r.n == 0 => refused.push(BuildRefusal {
                chapter: c,
                reason: "no_chunks".into(),
                rendered: 0,
                n: 0,
            }),
            Some(r) if r.rendered >= r.n => {
                render::enqueue_build(&st, c);
                building.push(c);
            }
            _ => {
                refused.push(BuildRefusal {
                    chapter: c,
                    reason: "not_rendered".into(),
                    rendered: r.map(|r| r.rendered).unwrap_or(0),
                    n: r.map(|r| r.n).unwrap_or(0),
                });
                rendering.push(c);
            }
        }
    }
    if !rendering.is_empty() {
        let b = ChapterSetBody {
            chapters: Some(rendering.iter().map(|c| *c as i64).collect()),
            pack: Some(true),
            force: None,
            book: None,
        };
        if let Err(e) = queue_chapters(&st, &b).await {
            return e;
        }
    }
    // Including the ones that went straight to the packer: a chapter that is
    // already rendered never passes through `queue_chapters`, so this is the
    // only place its intent is written down. Without it, a kill between here and
    // ffmpeg finishing would lose a "download this chapter" that answered 200.
    crate::wishlist::asked(&st, &all);
    Json(BuildResult {
        ok: true,
        built: done,
        building,
        rendering,
        refused,
    })
    .into_response()
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CancelResult {
    pub ok: bool,
    pub queue: Vec<usize>,
}

/// Drop chapters from the queues. An empty body clears both.
/// Drop a standing order on a book the session is not holding.
///
/// The mirror of [`order_elsewhere`], and it exists for the same reason: an
/// order that can be placed from the library view has to be cancellable from
/// there too. Chapters named, those chapters; no chapters named, the whole
/// book's order — the same "all of it" convention the loaded-book path uses.
///
/// A chapter the packer has already picked up is left alone, exactly as the
/// loaded book's `building` is: abandoning an encode mid-chapter throws away
/// every second it has spent.
fn cancel_elsewhere(st: &Arc<AppState>, key: &str, body: &ChapterSetBody) -> Option<Response> {
    let db = st.store()?;
    db.book(key).ok().flatten()?;
    let named: Option<Vec<usize>> = body.chapters.as_ref().filter(|c| !c.is_empty()).map(|_| {
        let n = crate::plancache::read_raw(&st.cfg.work, key)
            .map(|p| p.len())
            .unwrap_or(usize::MAX);
        wanted(body, n)
    });
    let dropped: Vec<usize> = match &named {
        None => match db.intents(key) {
            Ok(rows) => {
                let all: Vec<usize> = rows.into_iter().map(|r| r.chapter).collect();
                if let Err(e) = db.drop_intents_for_book(key) {
                    tracing::warn!("could not cancel the order for {key}: {e}");
                }
                all
            }
            Err(e) => {
                tracing::warn!("could not read the order for {key}: {e}");
                Vec::new()
            }
        },
        Some(w) => w
            .iter()
            .filter(|c| db.drop_intent(key, **c).unwrap_or(false))
            .copied()
            .collect(),
    };
    {
        let mut s = st.session();
        let building = s.building;
        s.pack_elsewhere
            .retain(|(k, c)| k != key || (!dropped.contains(c) || Some(*c) == building));
    }
    Some(
        Json(CancelResult {
            ok: true,
            queue: dropped,
        })
        .into_response(),
    )
}

#[utoipa::path(
    post, path = "/api/chapters/cancel", tag = "chapters",
    request_body = ChapterSetBody,
    responses((status = 200, body = CancelResult),
              (status = 409, body = ApiError, description = "no such book — the name given is \
neither the loaded book nor one the library knows. A *known* book that is not the \
loaded one is acted on rather than refused: the order goes to that book and the \
worker picks it up."))
)]
pub async fn chapters_cancel(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ChapterSetBody>,
) -> Response {
    if let Some(r) = super::session::wrong_book(&st, body.book.as_deref()) {
        // Cancelling has to reach as far as ordering does, or a download placed
        // on a book you are not reading could be started and never stopped —
        // which on this box is hours of the worker on something nobody wants any
        // more. Same rule as `order_elsewhere`: a book the library knows is
        // acted on, anything else is a 409.
        let key = cache::safe_key(body.book.as_deref().unwrap_or_default());
        return match cancel_elsewhere(&st, &key, &body) {
            Some(r) => r,
            None => r,
        };
    }
    let (queue, want) = {
        let mut s = st.session();
        let n = s.plan.len();
        let want = body
            .chapters
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|_| wanted(&body, n));
        match &want {
            None => {
                s.queue.clear();
                s.build_want.clear();
                s.pack_queue.clear();
            }
            Some(w) => {
                s.queue.retain(|c| !w.contains(c));
                for c in w {
                    s.build_want.remove(c);
                }
                let building = s.building;
                s.pack_queue
                    .retain(|c| !w.contains(c) || Some(*c) == building);
            }
        }
        (s.queue.clone(), want)
    };
    // A cancel has to reach the disk for the same reason the request did: the
    // file is what the next boot believes, and a cancel that only happened in
    // memory would be undone by the next restart — the very bug this is the
    // other half of. It also clears any parking record, so a chapter that was
    // parked and then cancelled leaves the list entirely rather than lingering
    // as a row nobody asked for.
    crate::wishlist::cancelled(&st, want.as_deref());
    Json(CancelResult { ok: true, queue }).into_response()
}
