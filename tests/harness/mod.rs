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

    /// Throw this process away and start another one over the same work
    /// directory, book library and vault — which is what a redeploy is.
    ///
    /// Goes through `narrator::boot`, the same startup sequence `main` runs, so
    /// what a test sees after a restart is what the box sees.
    pub async fn restart(&mut self) {
        let st = self.state.clone();
        st.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        st.run.set();
        st.build_ev.set();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let state = AppState::new(st.cfg.clone());
        narrator::boot(&state);
        let (app, _) = narrator::api::router(state.clone());
        self.app = app;
        self.state = state;
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

    /// The whole response, headers included — what a test that is about
    /// content negotiation rather than JSON shape needs.
    pub async fn raw(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> axum::http::Response<Body> {
        let mut req = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        self.app
            .clone()
            .oneshot(req.body(Body::empty()).expect("request"))
            .await
            .expect("the router never fails")
    }

    pub async fn get_json(&self, path: &str) -> (StatusCode, Value) {
        let (code, body) = self.get(path).await;
        (code, parse(&body))
    }

    pub async fn post_json(&self, path: &str, payload: Value) -> (StatusCode, Value) {
        self.post_json_from(path, payload, &[]).await
    }

    /// A POST with extra request headers — which in practice means "as a named
    /// device". See [`Self::as_device`].
    pub async fn post_json_from(
        &self,
        path: &str,
        payload: Value,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let (code, body) = self
            .send(req.body(Body::from(payload.to_string())).expect("request"))
            .await;
        (code, parse(&body))
    }

    /// The headers a device sends. Spelled once so a test says *which* device it
    /// is speaking as rather than repeating a header name, and so the day the
    /// spelling changes it changes in one place.
    pub fn as_device(id: &str) -> [(&str, &str); 1] {
        [("x-narrator-device", id)]
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
