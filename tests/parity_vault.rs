//! Vault-write parity: the three files narrator puts in Fernando's Obsidian
//! vault, against golden output from the python implementation.
//!
//! These are not "close enough" comparisons. `Reading Log.md` and the fleeting
//! notes are committed by obsidian-git within minutes of being written, so any
//! byte that differs between the two servers becomes a diff in the vault's
//! history — and `.narrator-positions.json` is machine truth that a python
//! narrator may read back after a Rust one wrote it.

use std::path::{Path, PathBuf};

use chrono::TimeZone;
use narrator::vault::{
    self, dumps_indent1, note_filename, note_markdown, reading_log, NoteInput, Positions,
};
use serde::Deserialize;
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(name: &str) -> Vec<u8> {
    let p = fixtures().join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn reading_log_is_byte_identical_to_python() {
    let pos: Positions = serde_json::from_slice(&read("reading_log.json")).expect("parse");
    let want = String::from_utf8(read("reading_log.md")).expect("utf8");
    let got = reading_log(&pos);
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

#[test]
fn a_pipe_in_a_title_is_left_unescaped_like_python() {
    // Not a nicety: the python server does not escape table cells, so a `|` in a
    // book stem genuinely breaks the row. Escaping it here would be a *better*
    // Reading Log and a worse port — the file would stop matching what the
    // python server writes for the same book, and the vault would churn.
    let pos: Positions = serde_json::from_slice(&read("reading_log.json")).expect("parse");
    let log = reading_log(&pos);
    assert!(
        log.contains("| Weird [draft] | v2 (2024) | 8/9 A | piped ] title | 3/3 |"),
        "{log}"
    );
}

#[test]
fn positions_json_round_trips_through_python_shaped_output() {
    let pos: Positions = serde_json::from_slice(&read("reading_log.json")).expect("parse");
    let text = dumps_indent1(&Value::Object(pos.clone()));
    // One-space indent, `": "` after a key, ASCII-escaped non-ASCII.
    assert!(text.starts_with("{\n \"/books/"), "{}", &text[..60]);
    assert!(text.contains("\\u041f\\u0440"), "cyrillic must be escaped");
    assert!(!text.contains('\u{41f}'), "no raw non-ASCII");
    // And it is still the same data.
    let back: Positions = serde_json::from_str(&text).expect("reparse");
    assert_eq!(back, pos);
}

#[test]
fn an_empty_positions_map_is_an_empty_object() {
    assert_eq!(dumps_indent1(&Value::Object(Positions::new())), "{}");
}

// ------------------------------------------------------------ fleeting notes

#[derive(Debug, Deserialize)]
struct NoteCase {
    input: NoteCaseInput,
    derived: NoteDerived,
    filename: String,
    markdown: String,
}

#[derive(Debug, Deserialize)]
struct NoteCaseInput {
    book_title: String,
    book_path: String,
    chapter_index: usize,
    chapter_title: String,
    chunks: Vec<NoteChunk>,
    chunk_index: usize,
    language: String,
    audio: String,
    captured: String,
    transcript: String,
}

#[derive(Debug, Deserialize)]
struct NoteChunk {
    text: String,
    silent: bool,
}

#[derive(Debug, Deserialize)]
struct NoteDerived {
    context: String,
    chapter_title_used: String,
    deep_link: String,
}

fn parse_stamp(s: &str) -> chrono::DateTime<chrono::Local> {
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").expect("stamp");
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .unwrap_or_else(|| chrono::Local.timestamp_opt(0, 0).single().expect("epoch"))
}

/// The context window the python `note()` builds: `chunks[max(0, i-1):i+2]`,
/// silent chunks dropped, joined with one space.
fn context(chunks: &[NoteChunk], i: usize) -> String {
    let start = i.saturating_sub(1);
    chunks
        .iter()
        .skip(start)
        .take(i + 2 - start)
        .filter(|c| !c.silent)
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn fleeting_notes_match_python_byte_for_byte() {
    let cases: Vec<NoteCase> = serde_json::from_slice(&read("note_markdown.json")).expect("parse");
    assert!(!cases.is_empty());
    let dir = tempfile::tempdir().expect("tempdir");
    for (n, c) in cases.iter().enumerate() {
        let stamp = parse_stamp(&c.input.captured);
        let ctx = context(&c.input.chunks, c.input.chunk_index);
        assert_eq!(ctx, c.derived.context, "case {n}: context window");

        let book_file = Path::new(&c.input.book_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        assert_eq!(
            vault::deep_link(&book_file, c.input.chapter_index, c.input.chunk_index),
            c.derived.deep_link,
            "case {n}: deep link"
        );

        let got = note_markdown(&NoteInput {
            stamp,
            book_title: &c.input.book_title,
            book_file: &book_file,
            chapter_title: &c.derived.chapter_title_used,
            chapter: c.input.chapter_index,
            chunk: c.input.chunk_index,
            chunks_total: c.input.chunks.len(),
            context: &ctx,
            language: &c.input.language,
            audio_name: &c.input.audio,
            text: &c.input.transcript,
        });
        assert_eq!(got, c.markdown, "case {n}: markdown");

        assert_eq!(
            note_filename(dir.path(), &stamp, &c.input.transcript),
            c.filename,
            "case {n}: filename"
        );
        // An untitled chapter is "Section <index+1>", which is what the fixture
        // recorded as `chapter_title_used`.
        if c.input.chapter_title.is_empty() {
            assert_eq!(
                c.derived.chapter_title_used,
                format!("Section {}", c.input.chapter_index + 1)
            );
        }
    }
}

#[test]
fn a_second_note_in_the_same_minute_gets_a_seconds_suffix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stamp = parse_stamp("2026-09-11T09:15:07");
    let first = note_filename(dir.path(), &stamp, "one two three");
    assert_eq!(first, "202609110915 one two three.md");
    std::fs::write(dir.path().join(&first), "x").expect("write");
    assert_eq!(
        note_filename(dir.path(), &stamp, "one two three"),
        "202609110915 07 one two three.md".replace(" 07 ", "07 ")
    );
}

#[test]
fn writing_the_vault_produces_both_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pos: Positions = serde_json::from_slice(&read("reading_log.json")).expect("parse");
    vault::write_positions(dir.path(), &pos).expect("write");
    assert_eq!(
        std::fs::read_to_string(vault::log_file(dir.path())).expect("log"),
        String::from_utf8(read("reading_log.md")).expect("utf8")
    );
    let back = vault::load_positions(dir.path());
    assert_eq!(back, pos);
}

#[test]
fn concurrent_position_writes_leave_one_whole_file() {
    // Many writers at once, each with a map one book larger than the last. The
    // file must always parse, must be exactly `dumps_indent1` of *some* map a
    // writer handed over, and nothing temporary may be left beside it.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Positions::new()));
    let threads: Vec<_> = (0..16)
        .map(|n| {
            let path = path.clone();
            let shared = shared.clone();
            std::thread::spawn(move || {
                for k in 0..10 {
                    shared.lock().expect("lock").insert(
                        format!("book {n}-{k}.epub"),
                        serde_json::json!({"chapter": k, "chunk": n, "chapter_title": "é",
                            "chunks_total": 1, "chapters_total": 1,
                            "updated": "2026-09-25T10:00:00"}),
                    );
                    vault::write_positions_from(&path, || shared.lock().expect("lock").clone())
                        .expect("write");
                    let back = vault::load_positions(&path);
                    assert!(!back.is_empty(), "a reader saw an empty or torn file");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("join");
    }
    // The last write took the last snapshot, so every book is on disk.
    let fin = shared.lock().expect("lock").clone();
    let raw = std::fs::read_to_string(vault::positions_file(&path)).expect("read");
    assert_eq!(
        raw,
        vault::dumps_indent1(&serde_json::Value::Object(fin.clone()))
    );
    assert_eq!(vault::load_positions(&path).len(), 160);
    let names: Vec<_> = std::fs::read_dir(&path)
        .expect("dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}
