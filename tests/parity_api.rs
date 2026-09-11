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
    assert_eq!(paths.len(), 25, "the contract is 25 paths");
}
