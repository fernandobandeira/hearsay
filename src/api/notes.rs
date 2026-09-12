//! Voice memo -> whisper transcript -> a fleeting note in the vault.
//!
//! The delivery contract the reader's outbox is built on: **a 2xx carrying
//! `{ok, file, text, language}` and nothing else counts as delivered.** Every
//! failure path is a 4xx or 5xx, because a memo the reader believes was filed
//! and was not is the one loss this system cannot tolerate — the audio is in
//! IndexedDB on a phone and nowhere else until this call succeeds.
//!
//! Which is why the memo names its book. Playback is one global session, and the
//! note's quote callout is built out of a chapter's words; a memo that waited out
//! an offline stretch and arrived after a book swap would otherwise quote the
//! wrong passage. With `book` supplied, the words come from that book's on-disk
//! text bundle — rebuilt on every load of it, so the chunk indices are exactly
//! the ones the memo refers to — and a book this server has no bundle for is
//! refused rather than filed wrong.
//!
//! And why the filing is *detached* from the request: on the 2-core box one memo
//! is minutes of whisper, and a phone that locks its screen in that window takes
//! the request future with it. Everything after the recording lands on disk
//! therefore runs in a task the request does not own (`queue::Job::spawn`), and
//! the handler only watches for its result. See [`queue`] for the other half —
//! the identity that makes the reader's inevitable retry a replay rather than a
//! second transcription and a second note.

mod queue;

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use super::{err, ApiError};
use crate::cache;
use crate::state::AppState;
use crate::vault;

#[derive(Debug, Deserialize, ToSchema)]
pub struct NoteBody {
    /// base64 of the MediaRecorder blob.
    pub audio: String,
    #[serde(default)]
    pub mime: Option<String>,
    /// Where the memo was recorded. Defaults to the session's own position.
    #[serde(default)]
    pub chapter: Option<usize>,
    #[serde(default)]
    pub chunk: Option<usize>,
    /// The book the memo was recorded against, as a cache key. **Additive, and
    /// worth sending**: a memo can wait out an offline stretch in the reader's
    /// outbox and arrive after the server has loaded something else, and the
    /// quote callout is built from a chapter's words. Supplied and not the
    /// loaded book, the quote, frontmatter and deep link come from that book's
    /// on-disk text bundle instead — no session swap, so whatever another device
    /// is listening to is left alone. A key with no bundle on this server is a
    /// **404**, never a 2xx, so the recording stays queued. Omitted, it means
    /// "whatever is loaded", exactly as before.
    #[serde(default)]
    pub book: Option<String>,
    /// A stable id for this recording, so posting it twice files one note.
    /// **Additive, and worth sending**: the reader only deletes its copy when it
    /// sees the 2xx, and on a slow box the phone is often gone by then — so the
    /// same memo arrives again. With an id, the second POST replays the first
    /// one's answer instead of spending another multi-minute transcription and
    /// filing a duplicate. Treated as a file name and ignored unless it is one;
    /// omitted, the recording's own bytes identify it, which is what
    /// deduplicates today's clients and the Obsidian plugin.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NoteResult {
    pub ok: bool,
    /// The note's file name in the vault.
    pub file: String,
    pub text: String,
    pub language: String,
}

#[utoipa::path(
    post, path = "/api/note", tag = "vault",
    request_body = NoteBody,
    responses(
        (status = 200, body = NoteResult, description = "filed - the outbox may delete its copy. \
            The same body comes back for a memo already filed under this `id`, so a retry the \
            server has already answered costs nothing"),
        (status = 400, body = ApiError, description = "no book, no audio, or nothing heard"),
        (status = 404, body = ApiError, description = "`book` names a book with no text bundle \
            here, or - for an `id` sent with no audio, which asks whether that memo was filed - \
            no note has been filed for it"),
        (status = 500, body = ApiError, description = "transcription or the vault write failed"),
    )
)]
pub async fn note(State(st): State<Arc<AppState>>, Json(body): Json<NoteBody>) -> Response {
    // The bytes first, because they are what identifies the memo when the client
    // did not say — and the identity is asked about before anything else, so a
    // memo already filed replays even for a book this server has since swapped
    // out. That case is not hypothetical: it is precisely the memo that waited
    // out an offline stretch.
    let Ok(audio) = base64::engine::general_purpose::STANDARD.decode(&body.audio) else {
        return refuse(StatusCode::BAD_REQUEST, "no audio");
    };
    if audio.is_empty() {
        // An `id` with no audio is a **probe**: "did my memo become a note?".
        // It exists for the recording that ran out of retries on the phone while
        // this server was quietly finishing it anyway — the reader can collect
        // the confirmation, and delete its copy, without pushing the bytes up
        // again. A memo that is not filed is a 404, which changes nothing about
        // it: it stays in the outbox, which is where it belongs.
        if let Some(id) = body.id.as_deref().and_then(queue::client_id) {
            return match queue::filed(&st.cfg.work, &id) {
                Some(f) => respond(Ok(f)),
                None => refuse(StatusCode::NOT_FOUND, "no note filed for this memo"),
            };
        }
        return refuse(StatusCode::BAD_REQUEST, "no audio");
    }
    let id = queue::memo_id(body.id.as_deref(), &audio);
    let mut job = match queue::claim(&st.cfg.work, &id) {
        queue::Claim::Done(filed) => {
            tracing::info!("note {id}: already filed as {}, replaying", filed.file);
            return respond(Ok(filed));
        }
        queue::Claim::Attach(w) => {
            tracing::info!("note {id}: already being transcribed, attaching");
            return respond(w.wait().await);
        }
        queue::Claim::Mine(j) => *j,
    };

    let (plan, session_title, bookpath, loaded_key, ci, i) = {
        let s = st.session();
        (
            s.plan.clone(),
            s.title.clone().unwrap_or_default(),
            s.book.clone().unwrap_or_default(),
            s.key(),
            body.chapter.unwrap_or(s.chapter),
            body.chunk.unwrap_or(s.playhead),
        )
    };
    // Which book's words the note quotes, decided before anything is written:
    // the named one out of its bundle, or the session's own plan.
    let asked = body
        .book
        .as_deref()
        .map(cache::safe_key)
        .unwrap_or_default();
    let named = !asked.is_empty() && Some(&asked) != loaded_key.as_ref();
    let (title, name, ctitle, ctx, n) = if named {
        let got = tokio::task::spawn_blocking({
            let work = st.cfg.work.clone();
            let key = asked.clone();
            move || crate::text::note_chapter(&work, &key, ci)
        })
        .await
        .ok()
        .flatten();
        let Some((title, name, ctitle, texts)) = got else {
            // A 404 rather than a note quoting the wrong passage: the reader's
            // outbox keeps the recording and asks again later. Dropping the job
            // here frees the id, so "later" is a fresh attempt and not an attach
            // to something nobody is running.
            return refuse(
                StatusCode::NOT_FOUND,
                format!("no text for book '{asked}' on this server"),
            );
        };
        // The bundle has no `silent` flags — a beat is stored as the text it
        // was made from — so a blank chunk is what stands in for one here.
        let ctx = texts
            .iter()
            .skip(i.saturating_sub(1))
            .take(if i == 0 { 2 } else { 3 })
            .filter(|t| !t.trim().is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let n = texts.len();
        (title, name, ctitle, ctx, n)
    } else if !plan.is_empty() {
        let ch = plan.get(ci);
        let chunks = ch.map(|c| c.chunks.as_slice()).unwrap_or(&[]);
        let ctitle = ch
            .map(|c| c.display_title())
            .unwrap_or_else(|| format!("Section {}", ci + 1));
        // The surrounding words, so a later pass over the vault knows what the
        // thought was reacting to.
        let ctx = chunks
            .iter()
            .skip(i.saturating_sub(1))
            .take(if i == 0 { 2 } else { 3 })
            .filter(|c| !c.silent)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let name = std::path::Path::new(&bookpath)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        (session_title, name, ctitle, ctx, chunks.len())
    } else {
        return refuse(StatusCode::BAD_REQUEST, "no book loaded");
    };

    let stamp = chrono::Local::now();
    let pending = queue::Pending {
        stamp_ms: stamp.timestamp_millis(),
        // The initial prompt biases whisper toward the words it will actually
        // hear. Written down with the rest so a resumed memo is transcribed the
        // same way this one would have been.
        prompt: [
            st.cfg.whisper_prompt.as_str(),
            title.as_str(),
            ctitle.as_str(),
        ]
        .iter()
        .filter(|x| !x.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(", "),
        title,
        name,
        ctitle,
        ctx,
        ci,
        i,
        n,
    };

    // A recording an earlier attempt already saved is reused, not copied: the
    // production failure this fix is about wrote the same 135 kB memo into
    // `notes-audio/` five times.
    let apath = match job.audio() {
        Some(p) => {
            job.accept(&p, pending.clone());
            p
        }
        None => {
            let ext = if body.mime.as_deref().unwrap_or("").contains("webm") {
                "webm"
            } else {
                "ogg"
            };
            let p = queue::audio_dir(&st.cfg.work)
                .join(format!("{}.{ext}", stamp.format("%Y%m%d%H%M%S")));
            // The work order before the bytes, and the bytes before anything that
            // can fail: from here this server can finish the memo with no client
            // and no memory of this request, and a recording that is on disk
            // under its real name is always a complete one.
            job.accept(&p, pending.clone());
            if let Err(e) = queue::write_durable(&p, &audio) {
                return refuse(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not save audio: {e}"),
                );
            }
            p
        }
    };

    // From here the request owns nothing. The transcription is minutes of CPU on
    // the box and a phone does not stay awake for it; whatever happens to this
    // connection — or to this process — the note gets written.
    let waiter = job.spawn(file_note(Filing {
        st: st.clone(),
        apath,
        pending,
    }));
    respond(waiter.wait().await)
}

/// Everything the detached half needs, owned — it outlives the request, and in
/// [`resume_unfiled`] it is read back off disk with no request at all.
struct Filing {
    st: Arc<AppState>,
    apath: std::path::PathBuf,
    pending: queue::Pending,
}

/// Sweep the queue for memos this server owes the vault, and finish them.
///
/// Called when the router is built, which is every process that serves the API
/// and no process that merely prints the spec. The box is restarted often —
/// deploys, the watchdog, a benchmark — and a restart used to kill a
/// transcription outright: the recording sat in `notes-audio/` and nothing ever
/// turned it into a note. Now the record beside it says what the note should be,
/// so finishing it needs nobody.
///
/// One at a time, oldest first: whisper is serialized behind its own lock anyway
/// and the box has two cores, so a queue of memos is a queue here too.
pub fn resume_unfiled(st: Arc<AppState>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // `narrator --openapi` and anything else outside a runtime: nothing to
        // resume onto, and nothing is lost by not trying.
        tracing::debug!("notes: no runtime, not sweeping the memo queue");
        return;
    };
    handle.spawn(async move {
        let owed = queue::unfiled(&st.cfg.work);
        if owed.is_empty() {
            return;
        }
        tracing::warn!("notes: {} memo(s) unfiled, resuming", owed.len());
        for m in owed {
            let queue::Claim::Mine(mut job) = queue::claim(&st.cfg.work, &m.id) else {
                // Filed by someone else, or a POST for it arrived first.
                continue;
            };
            let n = job.resuming();
            tracing::warn!(
                "note {}: resuming ({}/{}) from {}",
                m.id,
                n,
                queue::MAX_RESUMES,
                m.audio.display()
            );
            let out = (*job)
                .spawn(file_note(Filing {
                    st: st.clone(),
                    apath: m.audio,
                    pending: m.pending,
                }))
                .wait()
                .await;
            if let Err(e) = out {
                tracing::error!(
                    "note {}: still unfiled ({}) — it stays queued",
                    m.id,
                    e.message
                );
            }
        }
    });
}

/// Transcribe the recording and write the note. Runs in a task of its own, so
/// none of this is skipped by a client that stopped listening.
async fn file_note(f: Filing) -> queue::Outcome {
    let Filing { st, apath, pending } = f;
    let queue::Pending {
        stamp_ms,
        prompt,
        title,
        name,
        ctitle,
        ctx,
        ci,
        i,
        n,
    } = pending;
    // The stamp the memo arrived with, not the one it is finally written at: a
    // note resumed after a restart belongs to the moment the thought happened,
    // and its file name is derived from it.
    let stamp = match chrono::TimeZone::timestamp_millis_opt(&chrono::Local, stamp_ms) {
        chrono::LocalResult::Single(t) => t,
        _ => chrono::Local::now(),
    };
    // `transcribe` stays exactly where it was — one blocking call, taking
    // whatever priority it takes — only now the task holding it is not the
    // request's.
    let transcript = tokio::task::spawn_blocking({
        let st = st.clone();
        let ap = apath.clone();
        move || {
            if st.cfg.fake_stt {
                fake_transcript(&ap)
            } else {
                st.whisper.transcribe(&ap, &prompt)
            }
        }
    })
    .await;
    let t = match transcript {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(failed(500, format!("transcription failed: {e}"))),
        // The blocking task panicked or the runtime is shutting down.
        Err(e) => return Err(failed(500, format!("transcription failed: {e}"))),
    };
    if t.text.is_empty() {
        return Err(failed(400, "heard nothing - try again closer to the mic"));
    }

    let md = vault::note_markdown(&vault::NoteInput {
        stamp,
        book_title: &title,
        // The named book's own file name, so the deep link reopens the reader on
        // the passage the memo was recorded at rather than on whatever is loaded.
        book_file: &name,
        chapter_title: &ctitle,
        chapter: ci,
        chunk: i,
        chunks_total: n,
        context: &ctx,
        language: &t.language,
        audio_name: &apath
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        text: &t.text,
    });
    if let Err(e) = std::fs::create_dir_all(&st.cfg.notes_dir) {
        return Err(failed(500, format!("could not write note: {e}")));
    }
    let fname = vault::note_filename(&st.cfg.notes_dir, &stamp, &t.text);
    if let Err(e) = std::fs::write(st.cfg.notes_dir.join(&fname), md) {
        return Err(failed(500, format!("could not write note: {e}")));
    }
    // The note is filed. The recording device may or may not still be there to
    // hear it — the event is for the *other* devices, and the 2xx, if anyone is
    // left to take it, is the outbox's licence to delete its copy.
    st.bus.emit(
        "note",
        json!({"file": fname, "book": name, "chapter": ci,
               "chunk": i, "language": t.language}),
    );
    tracing::info!("note filed: {fname} ({}, ch{ci}/{i})", t.language);
    Ok(queue::Filed {
        file: fname,
        text: t.text,
        language: t.language,
    })
}

/// `NARRATOR_FAKE_STT` — the recording is its own transcript.
///
/// There is no whisper counterpart to `NARRATOR_FAKE_TTS`, so until this existed
/// no test could reach past the transcription: every one of them stopped at a
/// 500 from a model that is not in the suite. That left the half of this path
/// that actually writes to the vault — and the delivery contract the reader's
/// outbox is built on — covered by nothing. So the fake is the smallest possible
/// thing: the bytes are read back as text and used as the words, which makes a
/// test's memo say whatever the test recorded. Off by default, and it has no
/// business being set on the box.
fn fake_transcript(path: &std::path::Path) -> Result<crate::stt::Transcript, crate::stt::SttError> {
    let raw = std::fs::read(path).map_err(|e| crate::stt::SttError::Decode(e.to_string()))?;
    Ok(crate::stt::Transcript {
        text: String::from_utf8_lossy(&raw).trim().to_string(),
        language: "en".into(),
    })
}

fn failed(status: u16, message: impl Into<String>) -> queue::Failed {
    let f = queue::Failed::new(status, message);
    tracing::warn!("note not filed ({}): {}", f.status, f.message);
    f
}

/// Refuse, and *say so*. Every failure here used to be silent — `err()` writes a
/// body and no log line — and the reader treats a 5xx as "keep the recording and
/// ask again", so a memo could fail the same way for days with nothing on the
/// box to show for it. That is how the bug this module fixes went unnoticed.
fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    tracing::warn!("note refused ({}): {msg}", status.as_u16());
    err(status, msg)
}

/// One place where an outcome becomes the frozen response shape, so a replay and
/// a first filing are the same bytes.
fn respond(out: queue::Outcome) -> Response {
    match out {
        Ok(f) => Json(NoteResult {
            ok: true,
            file: f.file,
            text: f.text,
            language: f.language,
        })
        .into_response(),
        Err(e) => err(
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            e.message,
        ),
    }
}
