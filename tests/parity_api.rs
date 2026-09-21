//! Contract tests: every endpoint's JSON shape against a fixture transcribed
//! from the python server, plus the status codes the reader's retry policy and
//! the Obsidian plugin depend on.
//!
//! The fixture is *not* generated from this server — it is written down from
//! narrator's AGENTS.md, `app/server.py` and `web/src/lib/types.ts`, so it can
//! catch this server drifting rather than blessing whatever it happens to emit.

mod harness;

use axum::http::StatusCode;
use harness::Harness;
use serde_json::{json, Value};
use url::form_urlencoded;

fn contract() -> Value {
    let p =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api_contract.json");
    serde_json::from_slice(&std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .expect("api_contract.json")
}

/// Assert a value against the fixture's little type language.
fn check_type(spec: &str, v: &Value, where_: &str, bad: &mut Vec<String>) {
    let nullable = spec.ends_with('?') || spec == "null";
    let base = spec.trim_end_matches('?');
    let ok = match base {
        "null" => v.is_null(),
        "str" => v.is_string(),
        "int" => v.is_i64() || v.is_u64(),
        "num" => v.is_number(),
        "bool" => v.is_boolean(),
        "obj" => v.is_object(),
        "arr" => v.is_array(),
        _ => true,
    } || (nullable && v.is_null());
    if !ok {
        bad.push(format!("{where_}: expected {spec}, got {v}"));
    }
}

fn check_object(fields: &Value, v: &Value, where_: &str, bad: &mut Vec<String>) {
    let Some(obj) = v.as_object() else {
        bad.push(format!("{where_}: expected an object, got {v}"));
        return;
    };
    for (k, spec) in fields.as_object().into_iter().flatten() {
        match obj.get(k) {
            None => bad.push(format!("{where_}: missing field {k:?}")),
            Some(val) => check_type(
                spec.as_str().unwrap_or(""),
                val,
                &format!("{where_}.{k}"),
                bad,
            ),
        }
    }
}

fn check(spec: &Value, name: &str, body: &Value, bad: &mut Vec<String>) {
    let Some(ep) = spec.get("endpoints").and_then(|e| e.get(name)) else {
        bad.push(format!("no fixture for {name}"));
        return;
    };
    match ep.get("kind").and_then(Value::as_str) {
        Some("array") => {
            let Some(arr) = body.as_array() else {
                bad.push(format!("{name}: expected an array, got {body}"));
                return;
            };
            if let Some(item) = ep.get("item") {
                for (i, v) in arr.iter().enumerate() {
                    check_object(item, v, &format!("{name}[{i}]"), bad);
                }
            }
        }
        _ => {
            if let Some(fields) = ep.get("fields") {
                check_object(fields, body, name, bad);
            }
            if let Some(ai) = ep.get("array_item") {
                let at = ai.get("at").and_then(Value::as_str).unwrap_or("");
                let Some(arr) = body.get(at).and_then(Value::as_array) else {
                    bad.push(format!("{name}.{at}: not an array"));
                    return;
                };
                for (i, v) in arr.iter().enumerate() {
                    check_object(
                        ai.get("fields").unwrap_or(&Value::Null),
                        v,
                        &format!("{name}.{at}[{i}]"),
                        bad,
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn every_endpoint_answers_with_the_contract_shape() {
    let spec = contract();
    let h = Harness::new().await;
    let mut bad = Vec::new();

    // Nothing loaded yet: the chapter manager's empty shape.
    let (code, body) = h.get_json("/api/chapters").await;
    assert_eq!(code, StatusCode::OK);
    check(&spec, "GET /api/chapters (no book)", &body, &mut bad);

    let (code, body) = h.get_json("/api/books").await;
    assert_eq!(code, StatusCode::OK);
    check(&spec, "GET /api/books", &body, &mut bad);
    assert_eq!(body.as_array().map(Vec::len), Some(1), "the fixture epub");

    let (code, body) = h
        .post_json("/api/load", json!({"path": h.book_path()}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    check(&spec, "POST /api/load", &body, &mut bad);
    let key: String =
        form_urlencoded::byte_serialize(body["key"].as_str().unwrap_or("").as_bytes()).collect();

    for (name, path) in [
        ("GET /api/chapter/{ci}", "/api/chapter/0".to_string()),
        ("GET /api/status", "/api/status".into()),
        ("GET /api/book.json", format!("/api/book.json?book={key}")),
        (
            "GET /api/text/{s}.json",
            format!("/api/text/0.json?book={key}"),
        ),
        ("GET /healthz", "/healthz".into()),
    ] {
        let (code, body) = h.get_json(&path).await;
        assert_eq!(code, StatusCode::OK, "{path}: {body}");
        check(&spec, name, &body, &mut bad);
    }

    for (name, path, payload) in [
        (
            "POST /api/open",
            "/api/open",
            json!({"chapter": 0, "chunk": 0}),
        ),
        ("POST /api/playhead", "/api/playhead", json!({"chunk": 1})),
        ("POST /api/pause", "/api/pause", json!({})),
        ("POST /api/resume", "/api/resume", json!({})),
        ("POST /api/renderer", "/api/renderer", json!({"on": true})),
        ("POST /api/prerender", "/api/prerender", json!({"hours": 1})),
        (
            "POST /api/position",
            "/api/position",
            json!({"book": "Other.epub", "chapter": 2, "chunk": 3}),
        ),
        (
            "POST /api/chapters/render",
            "/api/chapters/render",
            json!({"chapters": [1]}),
        ),
        (
            "POST /api/chapters/build",
            "/api/chapters/build",
            json!({"chapters": [1]}),
        ),
        (
            "POST /api/chapters/cancel",
            "/api/chapters/cancel",
            json!({}),
        ),
    ] {
        let (code, body) = h.post_json(path, payload).await;
        assert_eq!(code, StatusCode::OK, "{path}: {body}");
        check(&spec, name, &body, &mut bad);
    }

    // With a book loaded the chapter manager's full shape appears.
    let (code, body) = h.get_json("/api/chapters").await;
    assert_eq!(code, StatusCode::OK);
    check(&spec, "GET /api/chapters", &body, &mut bad);

    assert!(
        bad.is_empty(),
        "{} contract violations:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[tokio::test]
async fn the_status_codes_the_clients_branch_on() {
    let spec = contract();
    let h = Harness::new().await;
    h.load().await;
    let want = spec["statuses"].as_object().cloned().unwrap_or_default();

    let mut bad = Vec::new();
    let mut expect = |name: &str, got: StatusCode| {
        let w = want.get(name).and_then(Value::as_u64).unwrap_or(0) as u16;
        if got.as_u16() != w {
            bad.push(format!("{name}: got {got}, want {w}"));
        }
    };

    expect(
        "GET /api/chapter/9999",
        h.get_json("/api/chapter/9999").await.0,
    );
    expect(
        "GET /api/chunk/0/9999.wav",
        h.get_json("/api/chunk/0/9999.wav").await.0,
    );
    expect(
        "GET /api/chapters/9999.m4a",
        h.get_json("/api/chapters/9999.m4a").await.0,
    );
    expect(
        "GET /api/chapters/9999.json",
        h.get_json("/api/chapters/9999.json").await.0,
    );
    expect(
        "GET /api/chapters/9999.m3u8",
        h.get_json("/api/chapters/9999.m3u8").await.0,
    );
    expect(
        "GET /api/text/99.json",
        h.get_json("/api/text/99.json").await.0,
    );
    expect(
        "GET /api/hls/x/0/evil.sh",
        h.get_json("/api/hls/x/0/evil.sh").await.0,
    );
    expect(
        "POST /api/position (no book)",
        h.post_json("/api/position", json!({"book": "  "})).await.0,
    );
    expect(
        "POST /api/note (no audio)",
        h.post_json("/api/note", json!({"audio": "!!!not base64!!!"}))
            .await
            .0,
    );
    // A memo naming a book this server has no text for: refused, so the
    // reader's outbox keeps the recording instead of deleting it on a 2xx.
    expect(
        "POST /api/note (unknown book)",
        h.post_json(
            "/api/note",
            json!({"audio": SOME_AUDIO, "book": "A Book Never Loaded Here"}),
        )
        .await
        .0,
    );
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[tokio::test]
async fn every_error_body_is_just_an_error_field() {
    let spec = contract();
    let h = Harness::new().await;
    let mut bad = Vec::new();
    for path in [
        "/api/chapter/9999",
        "/api/chunk/0/9999.wav",
        "/api/chapters/9999.m4a",
        "/api/text/99.json",
    ] {
        let (code, body) = h.get_json(path).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{path}");
        check(&spec, "error body", &body, &mut bad);
        assert_eq!(
            body.as_object().map(|o| o.len()),
            Some(1),
            "{path}: an error body carries nothing else, got {body}"
        );
    }
    // A body with no book loaded, on an endpoint that needs one.
    let (code, body) = h.post_json("/api/prerender", json!({"hours": 1})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    check(&spec, "error body", &body, &mut bad);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[tokio::test]
async fn a_bad_epub_is_a_400_not_a_panic() {
    let h = Harness::new().await;
    let junk = h.root().join("books/not-a-book.epub");
    std::fs::write(&junk, b"this is not a zip").expect("write");
    let (code, body) = h
        .post_json("/api/load", json!({"path": junk.to_string_lossy()}))
        .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.get("error").is_some(), "{body}");
    // And the server still answers.
    assert_eq!(h.get_json("/healthz").await.0, StatusCode::OK);
}

#[tokio::test]
async fn the_openapi_document_covers_the_urls_the_reader_builds() {
    let h = Harness::new().await;
    let (code, spec) = h.get_json("/openapi.json").await;
    assert_eq!(code, StatusCode::OK);
    let paths = spec["paths"].as_object().cloned().unwrap_or_default();
    for (p, method) in [
        ("/api/books", "get"),
        ("/api/status", "get"),
        ("/api/chapters", "get"),
        ("/api/load", "post"),
        ("/api/chapter/{ci}", "get"),
        ("/api/open", "post"),
        ("/api/playhead", "post"),
        ("/api/pause", "post"),
        ("/api/resume", "post"),
        ("/api/renderer", "post"),
        ("/api/prerender", "post"),
        ("/api/position", "post"),
        ("/api/note", "post"),
        ("/api/events", "get"),
        ("/api/book.json", "get"),
        ("/api/text/{s}.json", "get"),
        ("/api/chunk/{ci}/{i}.wav", "get"),
        ("/api/chapters/render", "post"),
        ("/api/chapters/build", "post"),
        ("/api/chapters/cancel", "post"),
        ("/api/chapters/{ci}.m4a", "get"),
        ("/api/chapters/{ci}.json", "get"),
        ("/api/chapters/{ci}.m3u8", "get"),
        ("/api/hls/{book}/{ci}/{name}", "get"),
        ("/healthz", "get"),
    ] {
        let ops = paths
            .get(p)
            .unwrap_or_else(|| panic!("{p} missing from the spec"));
        assert!(ops.get(method).is_some(), "{p} has no {method}");
    }
    // The 25 paths above are the python server's contract and are frozen: a
    // *removal* or a rename here breaks the Obsidian plugin, which is why they
    // are listed one by one rather than counted.
    //
    // Anything beyond them is this server's own, and is listed here too — so
    // that adding one stays a deliberate act with a line in a diff, rather than
    // something that happens by accident. Every addition must be additive: no
    // client is obliged to call it, and none of the 25 changes shape because it
    // exists.
    let extra = ["/api/library"];
    for p in extra {
        assert!(paths.contains_key(p), "{p} missing from the spec");
    }
    assert_eq!(
        paths.len(),
        25 + extra.len(),
        "a path was added or removed without saying so here: {:?}",
        paths.keys().collect::<Vec<_>>()
    );
}

// ------------------------------------------------------- pre-gzipped book text
// Content negotiation only: the decoded body is byte-identical to the plain
// file, so the frozen contract above is untouched. The assertions are on the
// response headers and on the bytes on disk, never on a decoded body alone — a
// client that decompresses transparently would make that pass vacuously.

/// Every built text file of the loaded book, as (url, path) pairs.
async fn text_files(h: &Harness) -> Vec<(String, std::path::PathBuf)> {
    let (code, index) = h.get_json("/api/book.json").await;
    assert_eq!(code, StatusCode::OK, "{index}");
    let key = index["key"].as_str().unwrap_or_default().to_string();
    let d = narrator::text::text_dir(&h.work(), &key);
    let mut out = vec![("/api/book.json".to_string(), d.join("index.json"))];
    for s in 0..index["shards"].as_u64().unwrap_or(0) {
        out.push((
            format!("/api/text/{s}.json"),
            d.join(format!("{s:03}.json")),
        ));
    }
    out
}

fn gunzip(b: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(b)
        .read_to_end(&mut out)
        .expect("a .gz that is really gzip");
    out
}

fn header(res: &axum::http::Response<axum::body::Body>, name: &str) -> Option<String> {
    res.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn body_of(res: axum::http::Response<axum::body::Body>) -> Vec<u8> {
    axum::body::to_bytes(res.into_body(), 64 * 1024 * 1024)
        .await
        .expect("body")
        .to_vec()
}

#[tokio::test]
async fn every_text_file_is_gzipped_beside_itself() {
    // The .gz is written in the same breath as the .json — one can never be
    // staler than the other — and it really is that file, byte for byte. One
    // chapter per shard, so every shard the writer can emit is covered, not
    // just the single one the fixture fits in.
    let h = Harness::with(|c| c.text_shard_chapters = 1).await;
    h.load().await;
    let files = text_files(&h).await;
    assert!(files.len() > 2, "index plus several shards, got {files:?}");
    for (_, p) in files {
        let gz = narrator::text::gz_path(&p);
        let raw = std::fs::read(&gz).unwrap_or_else(|e| panic!("{}: {e}", gz.display()));
        assert!(!raw.is_empty(), "{} is empty", gz.display());
        assert_eq!(gunzip(&raw), std::fs::read(&p).expect("plain"));
    }
}

#[tokio::test]
async fn gzip_is_served_when_it_is_accepted() {
    let h = Harness::new().await;
    h.load().await;
    for (url, p) in text_files(&h).await {
        let res = h.raw("GET", &url, &[("accept-encoding", "gzip")]).await;
        assert_eq!(res.status(), StatusCode::OK, "{url}");
        assert_eq!(
            header(&res, "content-encoding").as_deref(),
            Some("gzip"),
            "{url}"
        );
        assert_eq!(
            header(&res, "vary").as_deref(),
            Some("Accept-Encoding"),
            "{url}"
        );
        assert_eq!(
            header(&res, "cache-control").as_deref(),
            Some("public, max-age=3600"),
            "{url}"
        );
        assert_eq!(
            header(&res, "content-type").as_deref(),
            Some("application/json"),
            "{url}"
        );
        let plain = std::fs::read(&p).expect("plain");
        let len: usize = header(&res, "content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let body = body_of(res).await;
        // The wire really was smaller than the file it carried, and it decodes
        // to exactly that file.
        assert_eq!(len, body.len(), "{url}");
        assert!(
            len < plain.len(),
            "{url}: {len} is not smaller than {}",
            plain.len()
        );
        assert_eq!(gunzip(&body), plain, "{url}");
    }
}

#[tokio::test]
async fn identity_is_served_when_gzip_is_not_accepted() {
    let h = Harness::new().await;
    h.load().await;
    for (url, p) in text_files(&h).await {
        let res = h.raw("GET", &url, &[("accept-encoding", "identity")]).await;
        assert_eq!(res.status(), StatusCode::OK, "{url}");
        assert_eq!(header(&res, "content-encoding"), None, "{url}");
        assert_eq!(
            header(&res, "vary").as_deref(),
            Some("Accept-Encoding"),
            "{url}"
        );
        let plain = std::fs::read(&p).expect("plain");
        assert_eq!(
            header(&res, "content-length").as_deref(),
            Some(plain.len().to_string().as_str()),
            "{url}"
        );
        assert_eq!(body_of(res).await, plain, "{url}");
    }
}

#[tokio::test]
async fn accept_encoding_is_parsed_tolerantly() {
    let h = Harness::new().await;
    h.load().await;
    for (hdr, want) in [
        ("gzip", true),
        ("gzip;q=0.5", true),
        ("br, gzip", true),
        ("*", true),
        ("GZIP", true),
        ("gzip, deflate, br", true),
        ("gzip;q=nonsense", true), // a q we cannot read is not a refusal
        ("gzip;q=0", false),
        ("identity", false),
        ("", false),
        ("br", false),
        ("*;q=0", false),
        ("gzip;q=0, *", false), // an explicit refusal beats the wildcard
    ] {
        let res = h
            .raw("GET", "/api/book.json", &[("accept-encoding", hdr)])
            .await;
        assert_eq!(res.status(), StatusCode::OK, "{hdr:?}");
        assert_eq!(
            header(&res, "content-encoding").as_deref() == Some("gzip"),
            want,
            "accept-encoding: {hdr:?}"
        );
    }
    // No Accept-Encoding at all is the identity case too.
    let res = h.raw("GET", "/api/book.json", &[]).await;
    assert_eq!(header(&res, "content-encoding"), None);
}

#[tokio::test]
async fn head_answers_the_same_headers_with_no_body() {
    let h = Harness::new().await;
    h.load().await;
    for (url, p) in text_files(&h).await {
        let plain = std::fs::read(&p).expect("plain");
        for (hdr, gz) in [("gzip", true), ("identity", false)] {
            let res = h.raw("HEAD", &url, &[("accept-encoding", hdr)]).await;
            assert_eq!(res.status(), StatusCode::OK, "{url}");
            assert_eq!(
                header(&res, "content-encoding").is_some(),
                gz,
                "{url} {hdr}"
            );
            assert_eq!(header(&res, "vary").as_deref(), Some("Accept-Encoding"));
            assert_eq!(
                header(&res, "content-type").as_deref(),
                Some("application/json")
            );
            let len: usize = header(&res, "content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            assert_eq!(len < plain.len(), gz, "{url} {hdr}: content-length {len}");
            assert!(
                body_of(res).await.is_empty(),
                "{url} {hdr}: HEAD has no body"
            );
        }
    }
}

#[tokio::test]
async fn text_without_a_gz_still_serves() {
    // Text built by an older server has no .gz; that is a fallback, not a 500.
    let h = Harness::new().await;
    h.load().await;
    let (_, p) = text_files(&h).await.remove(0);
    std::fs::remove_file(narrator::text::gz_path(&p)).expect("remove the .gz");
    let res = h
        .raw("GET", "/api/book.json", &[("accept-encoding", "gzip")])
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header(&res, "content-encoding"), None);
    assert_eq!(header(&res, "vary").as_deref(), Some("Accept-Encoding"));
    assert_eq!(body_of(res).await, std::fs::read(&p).expect("plain"));
}

#[tokio::test]
async fn a_missing_text_file_is_still_a_404_with_an_error_body() {
    let h = Harness::new().await;
    h.load().await;
    for url in ["/api/text/99.json", "/api/book.json?book=nobody"] {
        let res = h.raw("GET", url, &[("accept-encoding", "gzip")]).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{url}");
        assert_eq!(header(&res, "content-encoding"), None, "{url}");
        let body: Value = serde_json::from_slice(&body_of(res).await).expect("error body");
        assert!(body.get("error").is_some(), "{url}: {body}");
    }
}

#[tokio::test]
async fn a_bundle_without_a_gz_is_rebuilt_on_the_next_load() {
    // The upgrade path: text written before this feature existed has no .gz,
    // and a reused plan would otherwise never give it one.
    let h = Harness::new().await;
    h.load().await;
    let files = text_files(&h).await;
    for (_, p) in &files {
        std::fs::remove_file(narrator::text::gz_path(p)).expect("remove the .gz");
    }
    h.load().await; // the plan is reused; the bundle is not
    for (url, p) in files {
        assert!(
            narrator::text::gz_path(&p).exists(),
            "{url}: the .gz came back"
        );
        let res = h.raw("GET", &url, &[("accept-encoding", "gzip")]).await;
        assert_eq!(
            header(&res, "content-encoding").as_deref(),
            Some("gzip"),
            "{url}"
        );
    }
}

// --------------------------------------------------- a memo names its book

/// A text bundle for a book this server never loaded — the shape `/api/load`
/// leaves behind, written by hand so the test does not need a second epub.
fn seed_bundle(h: &Harness, key: &str, name: &str, title: &str, chunks: &[&str]) {
    let d = h.work().join("text").join(key);
    std::fs::create_dir_all(&d).expect("bundle dir");
    std::fs::write(
        d.join("index.json"),
        json!({
            "key": key, "name": name, "title": title, "total_min": 1.0,
            "shards": 1, "text_bytes": 1,
            "chapters": [{"i": 0, "title": "Opening", "n": chunks.len(), "est_min": 1.0, "shard": 0}],
        })
        .to_string(),
    )
    .expect("index.json");
    std::fs::write(
        d.join("000.json"),
        json!({
            "shard": 0, "from": 0, "to": 0,
            "chapters": [{"i": 0, "paras": vec![0usize; chunks.len()], "chunks": chunks}],
        })
        .to_string(),
    )
    .expect("shard");
}

/// Base64 of something that is not audio (`not really a webm`): enough to get
/// past the decode, which is as far as a suite with no whisper model can go.
const SOME_AUDIO: &str = "bm90IHJlYWxseSBhIHdlYm0=";

#[tokio::test]
async fn a_memo_naming_a_book_with_no_bundle_here_is_a_404_not_a_note() {
    // The delivery contract: anything but a 2xx leaves the recording in the
    // reader's outbox. Filing it against the loaded book would quote the wrong
    // passage and the phone would then delete the only copy.
    let h = Harness::new().await;
    h.load().await;
    for body in [
        json!({"audio": SOME_AUDIO, "book": "A Book Never Loaded Here"}),
        // A bundle that exists, at a chapter it does not have.
        json!({"audio": SOME_AUDIO, "book": "Other Book (2019)", "chapter": 9}),
    ] {
        let named = body["book"].as_str().unwrap_or_default().to_string();
        seed_bundle(
            &h,
            "Other Book (2019)",
            "Other Book (2019).epub",
            "Other Book",
            &["one.", "two."],
        );
        let (code, got) = h.post_json("/api/note", body).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{named}: {got}");
        assert!(
            got["error"].as_str().unwrap_or_default().contains(&named),
            "the refusal names the book it could not answer for: {got}"
        );
        assert_eq!(got.as_object().map(|o| o.len()), Some(1), "{got}");
    }
}

#[tokio::test]
async fn a_memo_naming_another_book_is_answered_from_that_books_bundle() {
    let h = Harness::new().await;
    let loaded = h.load().await;
    seed_bundle(
        &h,
        "Other Book (2019)",
        "Other Book (2019).epub",
        "Other Book",
        &["one.", "two.", "three."],
    );
    let (code, body) = h
        .post_json(
            "/api/note",
            json!({"audio": SOME_AUDIO, "mime": "audio/webm",
                   "chapter": 0, "chunk": 1, "book": "Other Book (2019)"}),
        )
        .await;
    // The bundle answered — this is neither the 404 of a book with no text nor
    // the 400 of a body that never got that far. What stops it here is whisper,
    // which has no model in this suite; the words it would have quoted are
    // asserted at the bundle reader in `src/text.rs`.
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("transcription"),
        "{body}"
    );
    // The raw memo is on disk before anything can fail, and it never was the
    // session's business: the loaded book is untouched.
    let kept = std::fs::read_dir(h.work().join("notes-audio"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(kept, 1, "the recording landed before transcription");
    let (_, status) = h.get_json("/api/status").await;
    assert_eq!(status["key"], loaded["key"], "no session swap: {status}");
}

#[tokio::test]
async fn naming_the_loaded_book_is_the_same_as_naming_nothing() {
    let h = Harness::new().await;
    let key = h.load().await["key"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    for (name, payload) in [
        ("nothing heard", json!({"audio": SOME_AUDIO, "chunk": 1})),
        ("a body with no audio", json!({"audio": "!!!not base64!!!"})),
    ] {
        let (plain, a) = h.post_json("/api/note", payload.clone()).await;
        let mut named = payload;
        named["book"] = json!(key);
        let (with_book, b) = h.post_json("/api/note", named).await;
        assert_eq!(plain, with_book, "{name}: {a} vs {b}");
        assert_eq!(a, b, "{name}");
    }
}
