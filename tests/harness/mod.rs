//! A whole server in a temp directory, with the fake engine.
//!
//! No model, no network, no port: the router is driven directly through
//! `tower::ServiceExt::oneshot`, which is what makes the contract suite run in
//! milliseconds and in parallel.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use narrator::config::Config;
use narrator::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

pub struct Harness {
    pub app: Router,
    pub state: Arc<AppState>,
    dir: tempfile::TempDir,
    book: PathBuf,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// Build a server, letting the caller adjust the config first.
    pub async fn with(f: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(dir.path());
        // A harness must never be able to reach the real vault or the real
        // library, whatever the developer's environment says.
        cfg.vault = None;
        cfg.notes_dir = dir.path().join("work/notes");
        cfg.positions_dir = dir.path().join("work");
        f(&mut cfg);
        std::fs::create_dir_all(&cfg.work).expect("work");
        let books = cfg.books[0].clone();
        std::fs::create_dir_all(&books).expect("books");
        let book = books.join("Fixture (2026).epub");
        std::fs::copy(fixture_epub(), &book).expect("copy fixture epub");

        let state = AppState::new(cfg);
        let (app, _) = narrator::api::router(state.clone());
        Self {
            app,
            state,
            dir,
            book,
        }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn book_path(&self) -> String {
        self.book.to_string_lossy().to_string()
    }

    pub fn work(&self) -> PathBuf {
        self.state.cfg.work.clone()
    }

    pub async fn load(&self) -> Value {
        let (code, body) = self
            .post_json("/api/load", serde_json::json!({"path": self.book_path()}))
            .await;
        assert_eq!(code, StatusCode::OK, "load failed: {body}");
        body
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let res = self
            .app
            .clone()
            .oneshot(req)
            .await
            .expect("the router never fails");
        let code = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024 * 1024)
            .await
            .expect("body");
        (code, bytes.to_vec())
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Vec<u8>) {
        let req = Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("request");
        self.send(req).await
    }

    pub async fn get_with(&self, path: &str, header: (&str, &str)) -> (StatusCode, Vec<u8>) {
        let req = Request::builder()
            .uri(path)
            .header(header.0, header.1)
            .body(Body::empty())
            .expect("request");
        self.send(req).await
    }

    pub async fn head(&self, path: &str) -> (StatusCode, Vec<u8>) {
        let req = Request::builder()
            .method("HEAD")
            .uri(path)
            .body(Body::empty())
            .expect("request");
        self.send(req).await
    }

    pub async fn get_json(&self, path: &str) -> (StatusCode, Value) {
        let (code, body) = self.get(path).await;
        (code, parse(&body))
    }

    pub async fn post_json(&self, path: &str, payload: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .expect("request");
        let (code, body) = self.send(req).await;
        (code, parse(&body))
    }
}

fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|_| {
        Value::String(String::from_utf8_lossy(body).chars().take(400).collect())
    })
}

pub fn fixture_epub() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture.epub")
}
