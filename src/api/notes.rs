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
        (status = 200, body = NoteResult, description = "filed - the outbox may delete its copy"),
        (status = 400, body = ApiError, description = "no book, no audio, or nothing heard"),
        (status = 404, body = ApiError, description = "`book` names a book with no text bundle here"),
        (status = 500, body = ApiError, description = "transcription or the vault write failed"),
    )
)]
pub async fn note(State(st): State<Arc<AppState>>, Json(body): Json<NoteBody>) -> Response {
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
            // outbox keeps the recording and asks again later.
            return err(
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
        return err(StatusCode::BAD_REQUEST, "no book loaded");
    };
    let Ok(audio) = base64::engine::general_purpose::STANDARD.decode(&body.audio) else {
        return err(StatusCode::BAD_REQUEST, "no audio");
    };
    if audio.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no audio");
    }

    let stamp = chrono::Local::now();
    let adir = st.cfg.work.join("notes-audio");
    if let Err(e) = std::fs::create_dir_all(&adir) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not save audio: {e}"),
        );
    }
    let ext = if body.mime.as_deref().unwrap_or("").contains("webm") {
        "webm"
    } else {
        "ogg"
    };
    let apath = adir.join(format!("{}.{ext}", stamp.format("%Y%m%d%H%M%S")));
    // Raw memo audio is never deleted — it lands before anything can fail.
    if let Err(e) = std::fs::write(&apath, &audio) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not save audio: {e}"),
        );
    }

    let prompt = [
        st.cfg.whisper_prompt.as_str(),
        title.as_str(),
        ctitle.as_str(),
    ]
    .iter()
    .filter(|x| !x.is_empty())
    .copied()
    .collect::<Vec<_>>()
    .join(", ");

    let st2 = st.clone();
    let ap = apath.clone();
    let transcript =
        tokio::task::spawn_blocking(move || st2.whisper.transcribe(&ap, &prompt)).await;
    let t = match transcript {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("transcription failed: {e}"),
            )
        }
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("transcription failed: {e}"),
            )
        }
    };
    if t.text.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "heard nothing - try again closer to the mic",
        );
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
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not write note: {e}"),
        );
    }
    let fname = vault::note_filename(&st.cfg.notes_dir, &stamp, &t.text);
    if let Err(e) = std::fs::write(st.cfg.notes_dir.join(&fname), md) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not write note: {e}"),
        );
    }
    // The note is filed. The recording device already knows — this response is
    // its delivery contract; the event is for the *other* devices.
    st.bus.emit(
        "note",
        json!({"file": fname, "book": name, "chapter": ci,
               "chunk": i, "language": t.language}),
    );
    Json(NoteResult {
        ok: true,
        file: fname,
        text: t.text,
        language: t.language,
    })
    .into_response()
}
