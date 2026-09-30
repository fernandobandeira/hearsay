//! Managed uploads and deletion from the server shelf.
use super::{err, session::BookFile, ApiError, Ok2};
use crate::{cache, state::AppState};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::{path::Path, sync::Arc};
use utoipa::{IntoParams, ToSchema};

pub fn deleted(work: &Path, key: &str) -> bool {
    work.join("deleted").join(cache::safe_key(key)).is_file()
}

#[derive(Deserialize, IntoParams)]
pub struct UploadQuery {
    pub name: String,
}

#[utoipa::path(post, path = "/api/books/upload", tag = "library", params(UploadQuery),
    request_body(content = String, content_type = "application/epub+zip"),
    responses((status = 200, body = BookFile), (status = 400, body = ApiError), (status = 409, body = ApiError)))]
pub async fn upload(
    State(st): State<Arc<AppState>>,
    Query(q): Query<UploadQuery>,
    data: Bytes,
) -> Response {
    let _mutation = st.book_mutation.lock().await;
    let st = st.clone();
    let name = q.name;
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || !name.to_lowercase().ends_with(".epub")
        || name.len() > 240
    {
        return err(
            StatusCode::BAD_REQUEST,
            "Choose an EPUB with a plain filename",
        );
    }
    let size = data.len();
    let result = tokio::task::spawn_blocking(move || -> Result<BookFile, (StatusCode, String)> {
        let root = st.cfg.work.join("uploads");
        std::fs::create_dir_all(&root)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let path = root.join(&name);
        let key = cache::book_key(&name);
        if key.is_empty() || cache::safe_key(&key) != key { return Err((StatusCode::BAD_REQUEST, "Unsafe filename".into())); }
        if cache::book_dir(&st.cfg.work, &key).exists() { return Err((StatusCode::CONFLICT, "A render cache already uses this name; finish deleting the old book or choose another filename".into())); }
        // Never replace a book: cache keys and positions are tied to its text.
        let mut roots = st.cfg.books.clone();
        roots.push(root.clone());
        fn collision(root: &Path, key: &str) -> bool {
            std::fs::read_dir(root).is_ok_and(|rd| {
                rd.flatten().any(|e| {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        collision(&e.path(), key)
                    } else {
                        e.path()
                            .extension()
                            .is_some_and(|x| x.eq_ignore_ascii_case("epub"))
                            && cache::book_key(&e.path().to_string_lossy()) == key
                    }
                })
            })
        }
        if roots.iter().any(|r| collision(r, &key)) {
            return Err((
                StatusCode::CONFLICT,
                "A book with this name or cache key already exists; choose another filename".into(),
            ));
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&root)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        std::io::Write::write_all(&mut tmp, &data)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let chapters = crate::book::extract_chapters(tmp.path())
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid EPUB: {e}")))?;
        if chapters.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "EPUB has no readable chapters".into(),
            ));
        }
        tmp.persist_noclobber(&path)
            .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
        let marker = st.cfg.work.join("deleted").join(cache::safe_key(&key));
        if let Err(e) = std::fs::remove_file(marker) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        }
        st.bus.emit("books", serde_json::json!({"changed": [name]}));
        Ok(BookFile {
            path: path.to_string_lossy().into(),
            name,
            mb: super::round1(size as f64 / 1e6),
        })
    })
    .await;
    match result {
        Ok(Ok(b)) => Json(b).into_response(),
        Ok(Err((s, e))) => err(s, e),
        Err(e) => {
            tracing::warn!("upload failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "Upload failed")
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct DeleteBody {
    pub path: String,
    /// Must exactly match the EPUB filename to confirm permanent deletion.
    pub confirm: String,
}

#[utoipa::path(post, path = "/api/books/delete", tag = "library", request_body = DeleteBody,
 responses((status = 200, body = Ok2), (status = 400, body = ApiError), (status = 404, body = ApiError), (status = 500, body = ApiError)))]
pub async fn delete_book(State(st): State<Arc<AppState>>, Json(b): Json<DeleteBody>) -> Response {
    let _mutation = st.book_mutation.lock().await;
    let st = st.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<(), (StatusCode, String)> {
        let listed = super::session::all_books(&st);
        let Some(book) = listed.iter().find(|f| f.path == b.path) else {
            return Err((StatusCode::NOT_FOUND, "Book is not in the library".into()));
        };
        if book.name != b.confirm {
            return Err((
                StatusCode::BAD_REQUEST,
                "Confirmation must match the EPUB filename".into(),
            ));
        }
        let key = cache::book_key(&b.path);
        if cache::safe_key(&key) != key || key.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "Unsafe cache key".into()));
        }
        if listed
            .iter()
            .any(|f| f.name != book.name && cache::book_key(&f.path) == key)
        {
            return Err((
                StatusCode::BAD_REQUEST,
                "Another EPUB shares this cache key; rename it before deleting".into(),
            ));
        }
        let io = |e: std::io::Error| {
            tracing::warn!("book deletion: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Deletion could not finish: {e}. Retry to finish cleanup."),
            )
        };
        // Tombstone first, then wait for in-flight render/pack/scan work. Every
        // subsequent worker pass sees the tombstone, including after a crash.
        let markers = st.cfg.work.join("deleted");
        std::fs::create_dir_all(&markers).map_err(io)?;
        std::fs::write(markers.join(&key), &b.path).map_err(io)?;
        let _disk = st.book_files.write().unwrap_or_else(|e| e.into_inner());
        {
            let mut s = st.session();
            s.pack_queue.retain(|j| j.key != key);
            if s.key().as_deref() == Some(key.as_str()) {
                st.run.clear();
                // Retain jobs for other books while unloading the deleted one.
                let queues = std::mem::take(&mut s.pack_queue);
                *s = crate::state::Session::new();
                s.pack_queue = queues;
                st.set_stalled(false);
                match std::fs::remove_file(super::session::session_file(&st)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
                    _ => {}
                }
            }
        }
        *st.chstat.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(db) = st.store() {
            db.remove_book(&key)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        for area in ["audio", "chapters", "hls", "text", "render", "export"] {
            let p = st.cfg.work.join(area).join(&key);
            match std::fs::remove_dir_all(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
                _ => {}
            }
        }
        let export = st.cfg.work.join("export").join(format!("{key}.m4b"));
        match std::fs::remove_file(export) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
            _ => {}
        }
        // Delete every duplicate filename across both roots, so a vault copy
        // cannot reappear as soon as the uploaded copy is removed.
        fn sources(root: &Path, name: &str, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(root) {
                for e in rd.flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        sources(&e.path(), name, out);
                    } else if e.file_name() == name {
                        out.push(e.path());
                    }
                }
            }
        }
        let mut paths = Vec::new();
        for root in st
            .cfg
            .books
            .iter()
            .cloned()
            .chain([st.cfg.work.join("uploads")])
        {
            sources(&root, &book.name, &mut paths);
        }
        for p in paths {
            std::fs::remove_file(p).map_err(io)?;
        }
        st.audio_bytes.store(
            cache::audio_bytes(&st.cfg.work),
            std::sync::atomic::Ordering::Relaxed,
        );
        st.bus.emit("books", serde_json::json!({"changed": [key]}));
        Ok(())
    })
    .await;
    match result {
        Ok(Ok(())) => super::ok().into_response(),
        Ok(Err((s, e))) => err(s, e),
        Err(e) => {
            tracing::warn!("delete failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "Deletion failed")
        }
    }
}

/// Explicit completed deletions, including ones made before a device updated.
/// A missing library entry is never a deletion signal.
#[utoipa::path(get, path = "/api/books/deleted", tag = "library",
 responses((status = 200, body = Vec<String>), (status = 500, body = ApiError)))]
pub async fn deleted_books(State(st): State<Arc<AppState>>) -> Response {
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(st.cfg.work.join("deleted")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let key = entry.file_name().to_string_lossy().to_string();
            if key.is_empty() || cache::safe_key(&key) != key {
                continue;
            }
            let source = std::fs::read_to_string(entry.path())?;
            // Markers precede cleanup. Publish only after the source and the
            // caches are gone, so a failed or in-flight delete is not broadcast.
            if Path::new(&source).try_exists()? {
                continue;
            }
            let mut complete = true;
            for area in ["audio", "chapters", "hls", "text", "render", "export"] {
                if st.cfg.work.join(area).join(&key).try_exists()? {
                    complete = false;
                    break;
                }
            }
            if complete {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    })
    .await;
    match result {
        Ok(Ok(keys)) => (
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(keys),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!("reading deletions: {e}");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read server deletions",
            )
        }
        Err(e) => {
            tracing::warn!("reading deletions: {e}");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read server deletions",
            )
        }
    }
}
