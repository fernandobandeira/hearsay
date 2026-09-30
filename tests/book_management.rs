mod harness;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use harness::Harness;
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
async fn deletion_wipes_sources_caches_and_orders_without_losing_history() {
    let mut h = Harness::new().await;
    let plan = h.load().await;
    let path = h.book_path();
    let key = plan["key"].as_str().unwrap();
    h.post_json("/api/open", json!({"chapter":0,"chunk":0}))
        .await;
    h.post_json("/api/chapters/render", json!({"chapters":[0],"pack":true}))
        .await;
    for area in ["audio", "chapters", "hls", "text", "render", "export"] {
        let root = h.work().join(area).join(key);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("sentinel"), b"cache").unwrap();
    }
    let note = h.work().join("notes-audio/keep.webm");
    std::fs::create_dir_all(note.parent().unwrap()).unwrap();
    std::fs::write(&note, b"memo").unwrap();
    let history = h.state.positions().clone();
    assert!(!history.is_empty());
    let (code, _) = h
        .post_json("/api/books/delete", json!({"path":path,"confirm":"wrong"}))
        .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(std::path::Path::new(&path).exists());
    let (code, body) = h
        .post_json(
            "/api/books/delete",
            json!({"path":path,"confirm":"Fixture (2026).epub"}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(!std::path::Path::new(&path).exists());
    assert!(h.state.session().book.is_none());
    assert!(!h.work().join("session.json").exists());
    assert!(h.state.store().unwrap().book(key).unwrap().is_none());
    assert!(h.state.store().unwrap().intents(key).unwrap().is_empty());
    assert!(h
        .state
        .store()
        .unwrap()
        .chapter_index(key)
        .unwrap()
        .is_empty());
    assert_eq!(*h.state.positions(), history);
    for area in ["audio", "chapters", "hls", "text", "render", "export"] {
        assert!(!h.work().join(area).join(key).exists());
    }
    assert!(note.exists());
    h.restart().await;
    let (_, raw) = h.get("/api/books").await;
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), json!([]));
    assert!(!h.work().join("audio").join(key).exists());
}

async fn upload(h: &Harness, name: &str, data: Vec<u8>) -> StatusCode {
    h.app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/books/upload?name={name}"))
                .header("content-type", "application/epub+zip")
                .body(Body::from(data))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn uploads_validate_publish_and_refuse_overwrites_and_traversal() {
    let h = Harness::new().await;
    let raw = std::fs::read(h.book_path()).unwrap();
    assert_eq!(upload(&h, "New.epub", raw.clone()).await, StatusCode::OK);
    assert_eq!(upload(&h, "New.epub", raw).await, StatusCode::CONFLICT);
    assert_eq!(
        upload(&h, "Bad.epub", b"not a zip".to_vec()).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        upload(&h, "..%2FEscape.epub", vec![]).await,
        StatusCode::BAD_REQUEST
    );
    assert!(!h.work().join("uploads/Bad.epub").exists());
    let (_, raw) = h.get("/api/books").await;
    let rows: Value = serde_json::from_slice(&raw).unwrap();
    assert!(rows
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "New.epub"));
    assert_eq!(
        h.post_json(
            "/api/books/delete",
            json!({"path":"/etc/passwd","confirm":"passwd"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn uploaded_books_can_be_deleted_and_uploaded_again() {
    let h = Harness::new().await;
    let raw = std::fs::read(h.book_path()).unwrap();
    assert_eq!(upload(&h, "New.epub", raw.clone()).await, StatusCode::OK);
    let path = h.work().join("uploads/New.epub");
    assert_eq!(
        h.post_json(
            "/api/books/delete",
            json!({"path":path,"confirm":"New.epub"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(!path.exists());
    assert_eq!(upload(&h, "New.epub", raw).await, StatusCode::OK);
    assert!(path.exists());
    assert!(!narrator::api::books::deleted(&h.work(), "New"));
}

#[tokio::test]
async fn deletion_waits_for_in_flight_disk_work() {
    let h = Harness::new().await;
    h.load().await;
    let state = h.state.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _disk = state.book_files.read().unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    ready_rx.await.unwrap();
    let app = h.app.clone();
    let request = Request::builder()
        .method("POST")
        .uri("/api/books/delete")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"path":h.book_path(),"confirm":"Fixture (2026).epub"}).to_string(),
        ))
        .unwrap();
    let deletion = tokio::spawn(async move { app.oneshot(request).await.unwrap().status() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!deletion.is_finished());
    assert!(std::path::Path::new(&h.book_path()).exists());
    release_tx.send(()).unwrap();
    assert_eq!(deletion.await.unwrap(), StatusCode::OK);
    reader.join().unwrap();
    assert!(!std::path::Path::new(&h.book_path()).exists());
}

#[tokio::test]
async fn only_completed_explicit_deletions_are_reported_to_devices() {
    let h = Harness::new().await;
    let (_, raw) = h.get("/api/books/deleted").await;
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), json!([]));
    // A source missing from the shelf is not an explicit deletion.
    std::fs::remove_file(h.book_path()).unwrap();
    let (_, raw) = h.get("/api/books/deleted").await;
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), json!([]));
    let path = h.add_book("Finished.epub");
    let markers = h.work().join("deleted");
    std::fs::create_dir_all(&markers).unwrap();
    std::fs::write(markers.join("Finished"), &path).unwrap();
    let (_, raw) = h.get("/api/books/deleted").await;
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), json!([]));
    assert_eq!(
        h.post_json(
            "/api/books/delete",
            json!({"path":path,"confirm":"Finished.epub"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, raw) = h.get("/api/books/deleted").await;
    assert_eq!(
        serde_json::from_slice::<Value>(&raw).unwrap(),
        json!(["Finished"])
    );
    let fixture = std::fs::read(harness::fixture_epub()).unwrap();
    assert_eq!(upload(&h, "Finished.epub", fixture).await, StatusCode::OK);
    let (_, raw) = h.get("/api/books/deleted").await;
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), json!([]));
}
