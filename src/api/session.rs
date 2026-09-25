//! Session and playback — unchanged since phase 1 of the python server.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::ToSchema;

use super::{err, ok, round1, round2, ApiError, Ok2};
use crate::api::device::Device;
use crate::book::{build_plan, est_chapter_s, extract_chapters};
use crate::cache;
use crate::plancache;
use crate::render;
use crate::state::AppState;
use crate::text::ChapMeta;

use super::media::BookQuery;
use crate::vault;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BookFile {
    /// Absolute path on the server — what `/api/load` takes back.
    pub path: String,
    pub name: String,
    pub mb: f64,
}

/// `./books` merged with the vault's library, deduped by filename.
#[utoipa::path(
    get, path = "/api/books", tag = "session",
    responses((status = 200, body = Vec<BookFile>))
)]
pub async fn books(State(st): State<Arc<AppState>>) -> Json<Vec<BookFile>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for d in &st.cfg.books {
        let mut found = Vec::new();
        walk_epubs(d, &mut found);
        found.sort();
        for f in found {
            let Some(name) = f.file_name().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            // The same book in ./books and the vault is one book.
            if !seen.insert(name.clone()) {
                continue;
            }
            let mb = f.metadata().map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
            out.push(BookFile {
                path: f.to_string_lossy().to_string(),
                name,
                mb: round1(mb),
            });
        }
    }
    Json(out)
}

fn walk_epubs(d: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_epubs(&p, out),
            Ok(_)
                if p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("epub")) =>
            {
                out.push(p)
            }
            _ => {}
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoadBody {
    pub path: String,
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct LoadResult {
    pub title: String,
    /// The cache directory name: the file stem, truncated to 50 characters.
    pub key: String,
    pub total_min: f64,
    /// The stored position for this book, or null if it has never been opened.
    #[schema(required = true)]
    pub position: Option<vault::StampedPosition>,
    pub chapters: Vec<ChapMeta>,
}

/// Parse an EPUB and make it the session's book. Also writes `plan.json` and the
/// text bundle, both rebuilt from scratch: re-parsing can move chunk boundaries,
/// and a stale shard would put every position in it on the wrong words.
#[utoipa::path(
    post, path = "/api/load", tag = "session",
    request_body = LoadBody,
    responses(
        (status = 200, body = LoadResult),
        (status = 400, body = ApiError, description = "the file is not a readable EPUB"),
    )
)]
pub async fn load(State(st): State<Arc<AppState>>, Json(body): Json<LoadBody>) -> Response {
    let path = body.path.clone();
    let max_chars = body.max_chars.unwrap_or(crate::book::DEFAULT_MAX_CHARS);
    let key = cache::book_key(&body.path);

    // Re-parsing an unchanged book is the reader's twelve-second wait on first
    // paint, and it buys nothing: the plan is already on disk. Reuse it when the
    // file's size and mtime still match the stamp it was built from.
    //
    // A `stat` and a read of a plan that is megabytes of JSON on the big book:
    // blocking work, so it goes where blocking work goes. The box runs two
    // runtime workers, and a load that holds one of them holds half the API.
    let probed = tokio::task::spawn_blocking({
        let (work, key, path) = (st.cfg.work.clone(), key.clone(), path.clone());
        move || {
            let want = plancache::stamp(Path::new(&path), max_chars);
            let cached = want.as_ref().and_then(|s| plancache::load(&work, &key, s));
            (want, cached)
        }
    })
    .await;
    let (want, cached) = match probed {
        Ok(v) => v,
        // Only a panic gets here, and a parse is the honest fallback for one.
        Err(e) => {
            tracing::warn!("load: probing the plan cache failed ({e}); parsing instead");
            (None, None)
        }
    };
    let reused = cached.is_some();
    let plan: Arc<Vec<_>> = match cached {
        Some(p) => {
            tracing::info!(
                "load: reusing the cached plan for {key} ({} chapters)",
                p.len()
            );
            p
        }
        None => {
            let p2 = path.clone();
            let parsed = tokio::task::spawn_blocking(move || {
                extract_chapters(Path::new(&p2)).map(|c| build_plan(&c, max_chars))
            })
            .await;
            // A refusal here leaves the session exactly as it was: the renderer
            // has not been touched yet, so the book that is still loaded keeps
            // rendering. It used to be stopped first, and a path the reader got
            // wrong silenced a book nobody had asked to leave.
            match parsed {
                Ok(Ok(p)) => Arc::new(p),
                Ok(Err(e)) => return err(StatusCode::BAD_REQUEST, e.to_string()),
                Err(e) => return err(StatusCode::BAD_REQUEST, format!("parse panicked: {e}")),
            }
        }
    };

    // Stop the renderer before the plan changes underneath it — now, with the
    // new plan in hand, and not a moment before.
    st.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    st.run.clear();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    st.stop.store(false, std::sync::atomic::Ordering::SeqCst);

    let est: Vec<f64> = plan
        .iter()
        .map(|c| {
            est_chapter_s(
                &c.chunks,
                st.cfg.chapter_gap_s,
                st.cfg.chapter_para_gap_s,
                st.cfg.silence_s,
            )
        })
        .collect();
    let title = Path::new(&body.path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = Path::new(&body.path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    {
        let mut s = st.session();
        s.book = Some(body.path.clone());
        s.title = Some(title.clone());
        s.plan = plan.clone();
        s.est_s = est.clone();
        s.chapter = 0;
        s.render_idx = 0;
        s.playhead = 0;
        s.status = "idle".into();
        s.error = None;
        s.queue.clear();
        s.build_want.clear();
        s.build_error = None;
        s.pack_queue.clear();
    }

    // Everything from here to the response is files and sqlite — the wishlist,
    // `plan.json`, a text bundle that is ~17 MB of level-9 gzip on the big book,
    // a library scan, `session.json` — so it runs as one blocking job, in the
    // order it always ran in. The response waits for it, exactly as before: a
    // reader that gets its 200 can ask for a shard straight away.
    let finished = tokio::task::spawn_blocking({
        let st = st.clone();
        let done = Loaded {
            plan: plan.clone(),
            est: est.clone(),
            key: key.clone(),
            name: name.clone(),
            title: title.clone(),
            path: path.clone(),
            max_chars,
            reused,
            want,
        };
        move || finish_load(&st, &done)
    })
    .await;
    if let Err(e) = finished {
        // The book is loaded; what failed is bookkeeping around it, and each
        // part of that already logs and degrades on its own.
        tracing::warn!("load: finishing {key} failed: {e}");
    }

    let position = st
        .positions()
        .get(&name)
        .cloned()
        .and_then(|v| serde_json::from_value::<vault::Position>(v).ok())
        .map(|p| p.stamped());
    Json(LoadResult {
        title,
        key,
        total_min: round1(est.iter().sum::<f64>() / 60.0),
        position,
        chapters: plan
            .iter()
            .zip(&est)
            .map(|(c, e)| ChapMeta {
                i: c.index,
                title: c.display_title(),
                n: c.chunks.len(),
                est_min: round1(e / 60.0),
                shard: None,
            })
            .collect(),
    })
    .into_response()
}

/// What the blocking half of `/api/load` works from, owned so it can leave the
/// runtime.
struct Loaded {
    plan: Arc<Vec<crate::book::Chapter>>,
    est: Vec<f64>,
    key: String,
    name: String,
    title: String,
    path: String,
    max_chars: usize,
    reused: bool,
    want: Option<plancache::Stamp>,
}

/// The blocking half of `/api/load`, after the session holds the new plan.
fn finish_load(st: &Arc<AppState>, l: &Loaded) {
    let key = l.key.as_str();
    // ... and then this book's own wishlist back, if it has one.
    //
    // Clearing the queues above is right — they are indices into the plan that
    // has just been replaced — but on its own it would quietly undo the thing
    // `wishlist` exists for. The reader re-opens the book it was on when the app
    // starts, which is an `/api/load`, so a 74-chapter download that a restart
    // had just picked back up would be wiped by a phone coming out of a pocket.
    // The list is the book's, not the session's: it comes back with the book,
    // and switching to something else for ten minutes no longer costs it.
    let taken = crate::wishlist::adopt(st);
    if !taken.is_empty() {
        // Queued work is what starts the worker, exactly as the POST that
        // created the list would have.
        tracing::info!("load: {} chapter(s) still wanted for {key}", taken.len());
        st.run.set();
        render::ensure_render_thread(st);
    }

    // Drop the plan next to the cached audio: `narrator export` packs the
    // streaming cache into an .m4b from it without the container, and the stamp
    // beside it is what lets the next load skip the parse entirely.
    if !l.reused {
        if let Some(s) = &l.want {
            if let Err(e) = plancache::store(&st.cfg.work, key, &l.plan, s) {
                tracing::warn!("could not write plan.json: {e}");
            }
        }
    }
    // The bundle is rebuilt whenever the plan was: re-parsing can move chunk
    // boundaries and a stale shard puts every position in it on the wrong
    // words. A reused plan is by definition the same words, so the bundle is
    // only written when it is missing — which is also how a load recovers from
    // a half-written one.
    //
    // A bundle written before the files were pre-gzipped has no `.gz` beside
    // it, and a reused plan would never give it one; the words have not moved,
    // so rewriting the bundle is both harmless and the cheapest way to earn the
    // compressed copy every device then reads for the life of the book.
    let bundle = crate::text::text_dir(&st.cfg.work, key).join("index.json");
    if !l.reused || !bundle.exists() || !crate::text::gz_path(&bundle).exists() {
        if let Err(e) = crate::text::write_bundle(&st.cfg, &l.plan, &l.est, key, &l.name, &l.title)
        {
            tracing::warn!("could not write text bundle: {e}");
        }
    }

    // The library register, *after* the plan and the bundle are on disk.
    //
    // `/api/load` is the one moment the server learns a book's key, name, path,
    // title and chapter count all at once, and writing it down is what lets
    // everything else answer about a book the session is not holding — the
    // readiness view, the scheduler's "the most recently opened book", a
    // standing order placed on something else entirely.
    //
    // The index is written from the plan **in memory** rather than through
    // `library::rescan_book`, which re-reads `plan.json`. That is not an
    // optimisation: on a cold load the plan is written a few lines above this,
    // so a version of this that read the file had to sit below it anyway — and
    // reading back what we are already holding is a way to be subtly wrong for
    // no gain. Without it the book reports zero chapters and zero chunks until
    // the scanner's next tick, which is up to `LIBRARY_SCAN_EVERY_S` of a
    // freshly opened book looking empty.
    if let Some(db) = st.store() {
        let now_ms = chrono::Local::now().timestamp_millis();
        let row = crate::store::BookRow {
            key: key.to_string(),
            name: l.name.clone(),
            path: l.path.clone(),
            title: l.title.clone(),
            chapters: l.plan.len(),
            last_open_ms: Some(now_ms),
            scanned_ms: Some(now_ms),
        };
        if let Err(e) = db.put_book(&row) {
            tracing::warn!("could not register {key}: {e}");
        } else if let Err(e) = db.touch_book_open(key, now_ms) {
            tracing::warn!("could not stamp {key} as opened: {e}");
        }
        let rows = crate::library::scan_book(&st.cfg, key, &l.plan);
        if let Err(e) = db.put_chapter_index(key, &rows) {
            tracing::warn!("could not index {key}: {e}");
        }
    }

    // Which book this process is on is worth one small file: it is what lets the
    // next process come back on it. See `restore_session`.
    save_session(st, &l.path, l.max_chars);
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ChapterText {
    pub i: usize,
    pub title: String,
    pub chunks: Vec<String>,
    /// What lets the reader re-form real paragraphs from chunks. Additive:
    /// existing clients (the Obsidian plugin) only read title + chunks.
    pub paras: Vec<usize>,
}

/// One chapter's words.
///
/// **`?book=` is honoured**, which the python server does not do: it answers for
/// whichever book the process last loaded and ignores the query. That one
/// asymmetry is why the reader's first paint was coupled to `/api/load` at all —
/// the cheapest possible "give me the words of chapter 576" could not be asked
/// until the server had been told which book it was on. Here a key that is not
/// the loaded book is served straight out of that book's text bundle, with no
/// session involved. Omitted, it still means the loaded book.
#[utoipa::path(
    get, path = "/api/chapter/{ci}", tag = "session",
    params(("ci" = usize, Path, description = "chapter index"), BookQuery),
    responses((status = 200, body = ChapterText), (status = 404, body = ApiError))
)]
pub async fn chapter(
    State(st): State<Arc<AppState>>,
    AxPath(ci): AxPath<usize>,
    Query(q): Query<BookQuery>,
) -> Response {
    let asked = q.book.as_deref().map(cache::safe_key).unwrap_or_default();
    let (plan, key) = {
        let s = st.session();
        (s.plan.clone(), s.key())
    };
    if asked.is_empty() || Some(&asked) == key.as_ref() {
        if let Some(c) = plan.get(ci) {
            return Json(ChapterText {
                i: ci,
                title: c.title.clone(),
                chunks: c.chunks.iter().map(|k| k.text.clone()).collect(),
                paras: c.chunks.iter().map(|k| k.para).collect(),
            })
            .into_response();
        }
        if asked.is_empty() {
            return err(StatusCode::NOT_FOUND, "range");
        }
    }
    match tokio::task::spawn_blocking({
        let work = st.cfg.work.clone();
        move || crate::text::chapter_from_bundle(&work, &asked, ci)
    })
    .await
    {
        Ok(Some((title, paras, chunks))) => Json(ChapterText {
            i: ci,
            title,
            chunks,
            paras,
        })
        .into_response(),
        _ => err(StatusCode::NOT_FOUND, "range"),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct OpenBody {
    #[serde(default)]
    pub chapter: usize,
    #[serde(default)]
    pub chunk: usize,
    /// The book this is meant for. **Additive, and worth sending.** Playback is
    /// one global session, so an open issued while the server holds a different
    /// book moves the wrong book's render frontier and saves the position into
    /// the wrong record. Supplied and mismatched, this is refused with a 409
    /// instead; omitted, it means "whatever is loaded", exactly as before.
    #[serde(default)]
    pub book: Option<String>,
}

/// Select a chapter and start rendering from a given chunk.
///
/// The opened chunk is *guaranteed* to render: the worker renders whatever is
/// under the playhead before anything else, and this sets the playhead.
#[utoipa::path(
    post, path = "/api/open", tag = "session",
    request_body = OpenBody,
    params(
        ("X-Narrator-Device" = Option<String>, Header,
         description = "Who is reporting. Additive: omitted, this is the anonymous \
legacy device and the behaviour is exactly the pre-device one. Supplied, it is \
echoed on the `position` event so other devices can tell a real move from their \
own echo."),
        ("X-Narrator-Device-Name" = Option<String>, Header,
         description = "A human label for that device, for display only."),
    ),
    responses((status = 200, body = Ok2),
              (status = 409, body = ApiError, description = "the session holds another book"))
)]
pub async fn open_chapter(
    State(st): State<Arc<AppState>>,
    dev: Device,
    Json(body): Json<OpenBody>,
) -> Response {
    if let Some(r) = wrong_book(&st, body.book.as_deref()) {
        return r;
    }
    let (ci, chunk, key) = {
        let mut s = st.session();
        s.chapter = body.chapter;
        s.playhead = body.chunk;
        s.render_idx = body.chunk;
        s.status = "starting".into();
        (s.chapter, s.playhead, s.key())
    };
    save_position(&st, true, &dev);
    st.run.set();
    render::ensure_render_thread(&st);
    st.bus.emit_render(
        "chapter",
        json!({"key": key, "chapter": ci, "render_idx": chunk,
               "playhead": chunk, "status": "starting"}),
    );
    ok().into_response()
}

/// Refuse an action aimed at a book the session is not holding.
///
/// The alternative is what the python server does: act on whatever is loaded,
/// which silently drags another book's render frontier around and can file a
/// position under the wrong name. A 409 is a thing the reader can react to; a
/// wrong write is not.
///
/// Shared with the chapter endpoints, where the stake is highest: one tap can
/// queue 74 chapters of rendering, and on the wrong book that is an afternoon of
/// the worker spent on a novel nobody asked for.
pub fn wrong_book(st: &Arc<AppState>, asked: Option<&str>) -> Option<Response> {
    let asked = cache::safe_key(asked?);
    if asked.is_empty() {
        return None;
    }
    let have = st.session().key();
    match have {
        Some(k) if k == asked => None,
        _ => Some(err(
            StatusCode::CONFLICT,
            format!(
                "the session holds {}, not {asked}",
                have.unwrap_or_else(|| "no book".into())
            ),
        )),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PlayheadBody {
    pub chunk: usize,
    /// See [`OpenBody::book`]: additive, and it stops a report meant for one
    /// book from moving another one's frontier.
    #[serde(default)]
    pub book: Option<String>,
}

/// Report the playhead. Drags `render_idx` forward on a forward jump, never
/// backwards — anything already on disk still plays, and the worker skips files
/// that exist, so nothing is re-rendered.
#[utoipa::path(
    post, path = "/api/playhead", tag = "session",
    request_body = PlayheadBody,
    params(
        ("X-Narrator-Device" = Option<String>, Header,
         description = "Who is reporting. Additive: omitted, this is the anonymous \
legacy device and the behaviour is exactly the pre-device one. Supplied, it is \
echoed on the `position` event so other devices can tell a real move from their \
own echo."),
        ("X-Narrator-Device-Name" = Option<String>, Header,
         description = "A human label for that device, for display only."),
    ),
    responses((status = 200, body = Ok2),
              (status = 409, body = ApiError, description = "the session holds another book"))
)]
pub async fn playhead(
    State(st): State<Arc<AppState>>,
    dev: Device,
    Json(body): Json<PlayheadBody>,
) -> Response {
    if let Some(r) = wrong_book(&st, body.book.as_deref()) {
        return r;
    }
    {
        let mut s = st.session();
        s.playhead = body.chunk;
        if body.chunk > s.render_idx {
            s.render_idx = body.chunk;
        }
    }
    // A playhead report is a reader that is listening right now. Normally
    // `/api/open` has already started the worker; after a restart it has not —
    // the reader was *already* mid-chapter and has no reason to open anything —
    // so this is the only signal the worker gets. Gated on the thread never
    // having started in this process, so an explicit `/api/renderer {"on":
    // false}` is not quietly undone by the next chunk advance.
    if !render::render_alive(&st) {
        st.run.set();
        render::ensure_render_thread(&st);
    }
    save_position(&st, false, &dev);
    ok().into_response()
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PositionBody {
    pub book: String,
    pub chapter: Option<i64>,
    pub chunk: Option<i64>,
    pub chapter_title: Option<String>,
    pub chunks_total: Option<i64>,
    pub chapters_total: Option<i64>,
}

/// Record a position for a *named* book without loading it.
///
/// This is what the offline queue pushes on reconnect, possibly for a book the
/// server has since swapped out. Last write wins, which is the right rule for one
/// reader on several devices. Fields not supplied are kept from the existing
/// record.
#[utoipa::path(
    post, path = "/api/position", tag = "vault",
    request_body = PositionBody,
    params(
        ("X-Narrator-Device" = Option<String>, Header,
         description = "Who is reporting. Additive: omitted, this is the anonymous \
legacy device and the behaviour is exactly the pre-device one. Supplied, it is \
echoed on the `position` event so other devices can tell a real move from their \
own echo."),
        ("X-Narrator-Device-Name" = Option<String>, Header,
         description = "A human label for that device, for display only."),
    ),
    responses((status = 200, body = Ok2), (status = 400, body = ApiError),
              (status = 500, body = ApiError))
)]
pub async fn position(
    State(st): State<Arc<AppState>>,
    dev: Device,
    Json(body): Json<PositionBody>,
) -> Response {
    let name = body.book.trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "book required");
    }
    let record = {
        let mut pos = st.positions();
        let cur = pos.get(&name).cloned().unwrap_or(Value::Null);
        let geti = |k: &str, given: Option<i64>| -> i64 {
            given
                .or_else(|| cur.get(k).and_then(Value::as_i64))
                .unwrap_or(0)
        };
        let gets = |k: &str, given: Option<String>| -> String {
            given
                .or_else(|| cur.get(k).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default()
        };
        let p = vault::Position {
            chapter: geti("chapter", body.chapter),
            chunk: geti("chunk", body.chunk),
            chapter_title: gets("chapter_title", body.chapter_title),
            chunks_total: geti("chunks_total", body.chunks_total),
            chapters_total: geti("chapters_total", body.chapters_total),
            updated: vault::now_iso_seconds(),
        };
        pos.insert(
            name.clone(),
            serde_json::to_value(&p).unwrap_or(Value::Null),
        );
        p
    };
    if let Err(e) = vault::write_positions_from(&st.cfg.positions_dir, || st.positions().clone()) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not save: {e}"),
        );
    }
    if let Ok(mut w) = st.pos_written.lock() {
        *w = Some(Instant::now());
    }
    let seq = record_position(&st, &name, &record, &dev);
    emit_position(&st, &name, &record, "api", &dev, seq);
    ok().into_response()
}

/// Broadcast a position that has just been written, as `position`.
///
/// One place, because the payload is a contract with three readers (this repo's
/// PWA, the Obsidian plugin, anything else on the tunnel) and it used to be
/// assembled twice, slightly differently. What it carries beyond the vault
/// record is the part that was missing and that the reader could not work
/// without:
///
/// * **`updated_ms`** — the same instant as `updated`, in epoch milliseconds.
///   `updated` is a naive local stamp with no zone because the vault file has to
///   stay byte-identical to the python server's, and a browser genuinely cannot
///   order two of those: a container without `/etc/localtime` writes UTC while
///   the reader's own stamps are local. `StampedPosition` has resolved this at
///   the API's edge since requirement 3; the *event* never carried it, so every
///   recency rule in the reader was reasoning from a string it could not trust.
/// * **`device`** — whose report caused this write. The reader's own-echo test
///   was a two-chunk distance guess standing in for this question, which swallows
///   a real one-chunk move and follows a laptop that is three chapters behind.
/// * **`seq`** — monotonic, the tie-break when two writes share a millisecond or
///   when the clock steps under them.
/// * **`device_name`** — that device's own label, so the line the other readers
///   show can name it. Display only; the id is the identity.
///
/// All three are additive. A reader that reads none of them sees exactly the
/// payload it saw before.
fn emit_position(
    st: &Arc<AppState>,
    book: &str,
    record: &vault::Position,
    source: &str,
    dev: &Device,
    seq: u64,
) {
    let stamped = record.clone().stamped();
    let mut payload = serde_json::to_value(&stamped).unwrap_or(Value::Null);
    if let Some(o) = payload.as_object_mut() {
        o.insert("book".into(), Value::String(book.to_string()));
        o.insert("source".into(), Value::String(source.to_string()));
        o.insert("device".into(), Value::String(dev.id.clone()));
        // The label, so the other devices can say "moved on iPhone" rather than
        // "moved on another device". Display only, never matched on — the id is
        // the identity and this is the caption.
        o.insert("device_name".into(), Value::String(dev.name.clone()));
        o.insert("seq".into(), Value::from(seq));
    }
    st.bus.emit("position", payload);
}

/// Record a position against the device that reported it, and say what sequence
/// number it got.
///
/// The store keeps **one row per (book, device)** where the vault keeps one row
/// per book, and that difference is the point. The vault record is a projection
/// — last write wins, which is the right rule and always was — but a projection
/// throws away exactly the fact the reader needed: that the laptop is at chapter
/// 10 and the phone is at chapter 40, rather than that "the position" is
/// wherever the most recent report happened to come from. Keeping both means the
/// vault file stays byte-identical for the Obsidian plugin while the server can
/// still answer who is where.
///
/// The high-water mark moves here too, and only ever forwards. It is a different
/// question from "where am I" — re-reading a scene must not shrink it — and it
/// is what the reader's auto-trim and its behind-the-high-water-mark rule are
/// both anchored on.
///
/// Returns the store's sequence number, or the in-memory counter's when there is
/// no store. Either way it is monotonic within a process; with a store it is
/// monotonic across restarts too, which is what makes it a real tie-break rather
/// than a decoration.
fn record_position(st: &Arc<AppState>, book: &str, record: &vault::Position, dev: &Device) -> u64 {
    let Some(db) = st.store() else {
        return st.next_seq();
    };
    let now_ms = chrono::Local::now().timestamp_millis();
    if dev.known() {
        if let Err(e) = db.touch_device(&dev.id, &dev.name, now_ms) {
            tracing::warn!("could not record device {}: {e}", dev.id);
        }
    }
    let row = crate::store::PositionRow {
        chapter: record.chapter,
        chunk: record.chunk,
        chapter_title: record.chapter_title.clone(),
        chunks_total: record.chunks_total,
        chapters_total: record.chapters_total,
        updated_ms: vault::epoch_ms(&record.updated).unwrap_or(now_ms),
        seq: 0, // the store stamps its own; a number the caller picks is one two
                // callers can pick twice.
    };
    if let Err(e) = db.bump_furthest(
        book,
        &dev.id,
        record.chapter.max(0) as usize,
        record.chunk.max(0) as usize,
        now_ms,
    ) {
        tracing::warn!("could not move the high-water mark for {book}: {e}");
    }
    match db.put_position(book, &dev.id, row) {
        Ok(seq) => seq,
        Err(e) => {
            // The vault write has already happened or is about to; losing the
            // per-device row costs the cross-device answers, not the position.
            tracing::warn!("could not record the position for {book}: {e}");
            st.next_seq()
        }
    }
}

/// Persist chapter+chunk of the loaded book.
///
/// Throttled: `/api/playhead` fires on every chunk advance, and rewriting a note
/// in the vault that often is churn for nothing. `force` (open/pause) writes now.
pub fn save_position(st: &Arc<AppState>, force: bool, dev: &Device) {
    let (book, ci, playhead, plan, chapters_total) = {
        let s = st.session();
        match &s.book {
            None => return,
            Some(b) => (
                b.clone(),
                s.chapter,
                s.playhead,
                s.plan.clone(),
                s.plan.len(),
            ),
        }
    };
    if !force {
        if let Ok(w) = st.pos_written.lock() {
            if let Some(t) = *w {
                if t.elapsed().as_secs_f64() < 15.0 {
                    return;
                }
            }
        }
    }
    let ch = plan.get(ci);
    let name = Path::new(&book)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let record = vault::Position {
        chapter: ci as i64,
        chunk: playhead as i64,
        chapter_title: ch.map(|c| c.display_title()).unwrap_or_default(),
        chunks_total: ch.map(|c| c.chunks.len()).unwrap_or(0) as i64,
        chapters_total: chapters_total as i64,
        updated: vault::now_iso_seconds(),
    };
    st.positions().insert(
        name.clone(),
        serde_json::to_value(&record).unwrap_or(Value::Null),
    );
    // The snapshot is taken under the vault's writer lock, not here, so a slower
    // writer holding an older map can never land after this one.
    match vault::write_positions_from(&st.cfg.positions_dir, || st.positions().clone()) {
        Ok(()) => {
            if let Ok(mut w) = st.pos_written.lock() {
                *w = Some(Instant::now());
            }
        }
        Err(e) => {
            // A vault on a disconnected mount must not be able to stop playback.
            tracing::warn!("could not save position: {e}");
            return;
        }
    }
    // Tell the other devices. This fires exactly when the position is *written*,
    // so the 15 s throttle above is also the rate every open reader follows at —
    // which is the right rate: a position is a place in a book, not a cursor.
    let seq = record_position(st, &name, &record, dev);
    emit_position(st, &name, &record, "session", dev, seq);
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PauseResult {
    pub ok: bool,
    /// Whether the renderer keeps filling the buffer while playback is stopped.
    pub still_rendering: bool,
}

/// Pause playback. The time you are *not* listening is exactly when the buffer
/// should grow, so this stops audio, not the renderer — unless
/// `PREFETCH_WHILE_PAUSED=0`.
#[utoipa::path(
    post, path = "/api/pause", tag = "session",
    params(
        ("X-Narrator-Device" = Option<String>, Header,
         description = "Who is reporting. Additive: omitted, this is the anonymous \
legacy device and the behaviour is exactly the pre-device one. Supplied, it is \
echoed on the `position` event so other devices can tell a real move from their \
own echo."),
        ("X-Narrator-Device-Name" = Option<String>, Header,
         description = "A human label for that device, for display only."),
    ),
    responses((status = 200, body = PauseResult))
)]
pub async fn pause(State(st): State<Arc<AppState>>, dev: Device) -> Json<PauseResult> {
    if !st.cfg.prefetch_while_paused {
        st.run.clear();
    }
    st.session().status = "paused".into();
    save_position(&st, true, &dev);
    Json(PauseResult {
        ok: true,
        still_rendering: st.cfg.prefetch_while_paused,
    })
}

#[utoipa::path(post, path = "/api/resume", tag = "session", responses((status = 200, body = Ok2)))]
pub async fn resume(State(st): State<Arc<AppState>>) -> Json<Ok2> {
    st.run.set();
    render::ensure_render_thread(&st);
    st.session().status = "rendering".into();
    ok()
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RendererBody {
    #[serde(default = "yes")]
    pub on: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RendererResult {
    pub ok: bool,
    pub rendering: bool,
}

/// Explicitly stop/start the render worker, independent of playback.
#[utoipa::path(
    post, path = "/api/renderer", tag = "session",
    request_body = RendererBody, responses((status = 200, body = RendererResult))
)]
pub async fn renderer(
    State(st): State<Arc<AppState>>,
    Json(body): Json<RendererBody>,
) -> Json<RendererResult> {
    if body.on {
        st.run.set();
        render::ensure_render_thread(&st);
    } else {
        st.run.clear();
    }
    Json(RendererResult {
        ok: true,
        rendering: body.on,
    })
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PrerenderBody {
    /// Hours of estimated audio to build ahead. 0 or null falls back to the
    /// static `PRERENDER_CHAPTERS`.
    pub hours: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PrerenderResult {
    pub ok: bool,
    #[schema(required = true)]
    pub hours: Option<f64>,
    pub chapters_ahead: usize,
}

/// The old "render ahead N hours" target. Its UI is gone — the chapter manager
/// replaced it — but the endpoint stays for the CLI and anything else pointed
/// at it.
#[utoipa::path(
    post, path = "/api/prerender", tag = "session",
    request_body = PrerenderBody,
    responses((status = 200, body = PrerenderResult), (status = 400, body = ApiError))
)]
pub async fn prerender(
    State(st): State<Arc<AppState>>,
    Json(body): Json<PrerenderBody>,
) -> Response {
    let (hours, ci) = {
        let mut s = st.session();
        if s.plan.is_empty() {
            return err(StatusCode::BAD_REQUEST, "no book loaded");
        }
        s.prerender_hours = body.hours.filter(|h| *h > 0.0);
        (s.prerender_hours, s.chapter)
    };
    // The target used to live only in memory, and a redeploy silently dropped a
    // 13-chapter buffer back to PRERENDER_CHAPTERS=2. It is one number the user
    // chose — it belongs on disk.
    save_prerender(&st, hours);
    st.run.set();
    render::ensure_render_thread(&st);
    let span = st.session().prerender_span(ci, &st.cfg);
    Json(PrerenderResult {
        ok: true,
        hours,
        chapters_ahead: span,
    })
    .into_response()
}

// ------------------------------------------------------- surviving a restart

/// Which book the process was on, so the next one can pick it up.
///
/// **The deploy bug, 2026-09-11.** Playback is one global session held in
/// memory, and a container restart empties it: `book` is `None` until some
/// client calls `/api/load` again. Nothing the reader does mid-chapter is that
/// call — it loads a book when you *pick* one — so a restart under a listening
/// reader left the server in a state where
///
/// * `/api/chunk/{ci}/{i}.wav` is session-scoped and 404s ("not ready"),
/// * `/api/open` and `/api/playhead` carrying `?book=` answer **409**,
/// * `/api/chapters?book=` answers 409 and the drawer shows an empty book,
/// * and the render worker has no plan, so nothing is being rendered for the
///   reader that is sitting there waiting on exactly one chunk.
///
/// The reader's own half of the heal is in `web/src/lib/live.ts`, but the server
/// should not need rescuing in the first place: everything it needs is already
/// on disk. The plan is in `plan.json` with its `parse.json` stamp beside it,
/// the position is in the vault; the only thing that was missing was the name of
/// the book, which is this file. Restoring costs one plan read — 0.36 s on the
/// 1433-chapter book, the same read `/api/load` does — and no parse.
///
/// It is deliberately *only* a restore of what was already true. It does not
/// start the renderer: a process that comes up rendering a book nobody is
/// reading is a worse failure than the one being fixed.
#[derive(Debug, Serialize, Deserialize)]
struct LastBook {
    book: String,
    #[serde(default)]
    max_chars: Option<usize>,
}

pub fn session_file(st: &AppState) -> std::path::PathBuf {
    st.cfg.work.join("session.json")
}

fn save_session(st: &AppState, book: &str, max_chars: usize) {
    let v = LastBook {
        book: book.to_string(),
        max_chars: Some(max_chars),
    };
    if let Err(e) = std::fs::write(
        session_file(st),
        serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec()),
    ) {
        // Losing this costs a restart its memory, nothing else.
        tracing::warn!("could not record the loaded book: {e}");
    }
}

/// Put the last-loaded book back in the session. Called once at startup.
///
/// Returns the key it restored, or None — a missing file, a book that has been
/// deleted or edited since, a plan that is not there any more. Every one of
/// those simply means "start empty", exactly as before; none is an error.
pub fn restore_session(st: &Arc<AppState>) -> Option<String> {
    let last: LastBook = serde_json::from_slice(&std::fs::read(session_file(st)).ok()?).ok()?;
    let path = Path::new(&last.book);
    let max_chars = last.max_chars.unwrap_or(crate::book::DEFAULT_MAX_CHARS);
    let key = cache::book_key(&last.book);
    // Only ever from the parse cache: re-parsing here would put a 12-second
    // EPUB parse in front of the port opening, and a plan whose stamp no longer
    // matches is a book that changed — which is a `/api/load`'s business, not a
    // restart's.
    let want = plancache::stamp(path, max_chars)?;
    let plan = plancache::load(&st.cfg.work, &key, &want)?;

    let est: Vec<f64> = plan
        .iter()
        .map(|c| {
            est_chapter_s(
                &c.chunks,
                st.cfg.chapter_gap_s,
                st.cfg.chapter_para_gap_s,
                st.cfg.silence_s,
            )
        })
        .collect();
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // Where the reader was is the vault's answer, not a second copy of it here.
    let pos = st
        .positions()
        .get(&name)
        .cloned()
        .and_then(|v| serde_json::from_value::<vault::Position>(v).ok());
    let ci = pos
        .as_ref()
        .map(|p| p.chapter.max(0) as usize)
        .filter(|c| *c < plan.len())
        .unwrap_or(0);
    let n = plan.get(ci).map(|c| c.chunks.len()).unwrap_or(0);
    let chunk = pos
        .as_ref()
        .map(|p| p.chunk.max(0) as usize)
        .filter(|i| *i < n)
        .unwrap_or(0);

    let mut s = st.session();
    s.book = Some(last.book.clone());
    s.title = Some(
        path.file_stem()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    s.plan = plan;
    s.est_s = est;
    s.chapter = ci;
    s.playhead = chunk;
    s.render_idx = chunk;
    s.status = "idle".into();
    Some(key)
}

pub fn prerender_file(st: &AppState) -> std::path::PathBuf {
    st.cfg.work.join("prerender.json")
}

pub fn load_prerender(st: &AppState) -> Option<f64> {
    let v: Value = serde_json::from_slice(&std::fs::read(prerender_file(st)).ok()?).ok()?;
    v.get("hours").and_then(Value::as_f64).filter(|h| *h > 0.0)
}

fn save_prerender(st: &AppState, hours: Option<f64>) {
    let p = prerender_file(st);
    if let Some(d) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(d) {
            tracing::warn!("could not save prerender target: {e}");
            return;
        }
    }
    if let Err(e) = std::fs::write(&p, json!({"hours": hours}).to_string()) {
        tracing::warn!("could not save prerender target: {e}");
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Status {
    pub status: String,
    #[schema(required = true)]
    pub error: Option<String>,
    pub chapter: usize,
    #[schema(required = true)]
    pub book: Option<String>,
    #[schema(required = true)]
    pub title: Option<String>,
    #[schema(required = true)]
    pub key: Option<String>,
    pub queue: Vec<usize>,
    #[schema(required = true)]
    pub building: Option<usize>,
    #[schema(required = true)]
    pub build_error: Option<String>,
    /// What the packer is about to do, not only what it is doing. Without it a
    /// chapter waiting to be packed looks idle.
    pub pack_queue: Vec<usize>,
    pub pack_want: Vec<usize>,
    pub render_idx: usize,
    pub playhead: usize,
    pub total: usize,
    pub chapters: usize,
    pub model_ready: bool,
    #[schema(required = true)]
    pub rtf: Option<f64>,
    pub rendered_min: f64,
    pub voice: String,
    #[schema(required = true)]
    pub prerender: Option<usize>,
    pub prerender_chapters: usize,
    #[schema(required = true)]
    pub prerender_hours: Option<f64>,
    pub prerender_span: usize,
    #[schema(required = true)]
    pub book_min: Option<f64>,
    #[schema(required = true)]
    pub done_min: Option<f64>,
    pub disk_gb: f64,
    pub disk_cap_gb: f64,
    /// `CHAPTER_BITRATE` as configured — `"64k"`. The reader multiplies a
    /// chapter's estimated minutes by this to size a download before anything
    /// has been packed; without it, it hard-codes the default and is silently
    /// wrong by whatever ratio the box was set to.
    pub bitrate: String,
    /// The same number as bytes per minute of audio, so nobody has to parse the
    /// suffix: `64k` → 480000.
    pub bitrate_bytes_per_min: f64,
}

/// The heartbeat. It is also how the reader notices the server came back.
#[utoipa::path(get, path = "/api/status", tag = "session", responses((status = 200, body = Status)))]
pub async fn status(State(st): State<Arc<AppState>>) -> Json<Status> {
    let st2 = st.clone();
    let done = tokio::task::spawn_blocking(move || {
        let has_book = st2.session().book.is_some();
        let done = if has_book {
            Some(render::done_seconds(&st2))
        } else {
            None
        };
        (done, cache::audio_bytes(&st2.cfg.work))
    })
    .await
    .unwrap_or((None, 0));

    let s = st.session();
    let ci = s.chapter;
    let total = s.plan.get(ci).map(|c| c.chunks.len()).unwrap_or(0);
    let rtf = (s.render_time > 1.0).then(|| round2(s.rendered_s / s.render_time));
    Json(Status {
        status: s.status.clone(),
        error: s.error.clone(),
        chapter: ci,
        book: s.book_name(),
        title: s.title.clone(),
        key: s.key(),
        queue: s.queue.clone(),
        building: s.building,
        build_error: s.build_error.clone(),
        pack_queue: s.pack_queue.clone(),
        pack_want: s.build_want.iter().copied().collect(),
        render_idx: s.render_idx,
        playhead: s.playhead,
        total,
        chapters: s.plan.len(),
        model_ready: s.model_ready,
        rtf,
        rendered_min: round1(s.rendered_s / 60.0),
        voice: st.engine.voice().to_string(),
        prerender: s.prerender,
        prerender_chapters: st.cfg.prerender_chapters,
        prerender_hours: s.prerender_hours,
        prerender_span: s.prerender_span(ci, &st.cfg),
        book_min: (!s.est_s.is_empty()).then(|| round1(s.est_s.iter().sum::<f64>() / 60.0)),
        done_min: done.0.map(|d| round1(d / 60.0)),
        disk_gb: round2(done.1 as f64 / 1024.0_f64.powi(3)),
        disk_cap_gb: st.cfg.max_audio_gb,
        bitrate: st.cfg.chapter_bitrate.clone(),
        bitrate_bytes_per_min: crate::chapters::bytes_per_minute(&st.cfg.chapter_bitrate),
    })
}
