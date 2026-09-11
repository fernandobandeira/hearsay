//! Everything narrator writes into the Obsidian vault.
//!
//! Three files, all machine-written and all under the vault's git backup, so
//! byte-level churn is a real cost: a formatting difference between the python
//! server and this one would show up as a diff in every `vault backup:` commit.
//! `.narrator-positions.json` is therefore written exactly the way
//! `json.dumps(pos, indent=1)` writes it — one-space indent, `": "` after keys,
//! and **non-ASCII escaped**, which is `ensure_ascii=True`'s doing and the part
//! serde_json would otherwise get wrong.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::value::Value;

use crate::book::is_speakable_char;

/// `datetime.now().isoformat(timespec="seconds")` — local time, no offset.
pub fn now_iso_seconds() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// One book's last position. Field order is the python dict's, because it is the
/// order `json.dumps` writes and the file is diffed by git.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Position {
    pub chapter: i64,
    pub chunk: i64,
    pub chapter_title: String,
    pub chunks_total: i64,
    pub chapters_total: i64,
    pub updated: String,
}

/// A position as the *API* hands it back: the record above plus an unambiguous
/// instant.
///
/// `updated` is a naive local stamp with no zone, because that is what the
/// python server writes into the vault and the file has to keep matching. A
/// browser cannot resolve it — a container without `/etc/localtime` stamps UTC
/// while the same reader's other positions are local, and the reader is left
/// guessing which of two positions is newer. So the API carries the instant as
/// well, resolved here where the server's own zone is known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StampedPosition {
    pub chapter: i64,
    pub chunk: i64,
    pub chapter_title: String,
    pub chunks_total: i64,
    pub chapters_total: i64,
    /// Naive local ISO, byte-identical to the vault record.
    pub updated: String,
    /// The same moment in epoch milliseconds. Null only if `updated` cannot be
    /// parsed at all, which means it was not written by a narrator.
    pub updated_ms: Option<i64>,
}

impl Position {
    pub fn stamped(self) -> StampedPosition {
        let updated_ms = epoch_ms(&self.updated);
        StampedPosition {
            chapter: self.chapter,
            chunk: self.chunk,
            chapter_title: self.chapter_title,
            chunks_total: self.chunks_total,
            chapters_total: self.chapters_total,
            updated: self.updated,
            updated_ms,
        }
    }
}

/// Resolve a naive `YYYY-MM-DDTHH:MM:SS` against this machine's zone. An
/// ambiguous local time (the hour a DST fold repeats) takes the earlier of the
/// two; a nonexistent one (the hour DST skips) has no answer and returns None.
pub fn epoch_ms(naive: &str) -> Option<i64> {
    use chrono::TimeZone;
    let dt = chrono::NaiveDateTime::parse_from_str(naive, "%Y-%m-%dT%H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&dt)
        .earliest()
        .map(|t| t.timestamp_millis())
}

/// The whole file: book file name -> position. `serde_json`'s `preserve_order`
/// feature keeps insertion order, which is what python's dicts do and what the
/// Reading Log's stable sort falls back on for equal timestamps.
pub type Positions = serde_json::Map<String, Value>;

pub fn positions_file(dir: &Path) -> PathBuf {
    dir.join(".narrator-positions.json")
}

pub fn log_file(dir: &Path) -> PathBuf {
    dir.join("Reading Log.md")
}

pub fn load_positions(dir: &Path) -> Positions {
    match std::fs::read(positions_file(dir)) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Positions::new(),
    }
}

/// `json.dumps(obj, indent=1)`: one space per level, `": "` between key and
/// value, and every non-ASCII character escaped as `\uXXXX` (surrogate pairs for
/// astral planes).
pub fn dumps_indent1(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, 0, &mut out);
    out
}

fn write_value(v: &Value, depth: usize, out: &mut String) {
    match v {
        Value::Object(m) if m.is_empty() => out.push_str("{}"),
        Value::Object(m) => {
            out.push_str("{\n");
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(depth + 1, out);
                write_string(k, out);
                out.push_str(": ");
                write_value(val, depth + 1, out);
            }
            out.push('\n');
            pad(depth, out);
            out.push('}');
        }
        Value::Array(a) if a.is_empty() => out.push_str("[]"),
        Value::Array(a) => {
            out.push_str("[\n");
            for (i, val) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(depth + 1, out);
                write_value(val, depth + 1, out);
            }
            out.push('\n');
            pad(depth, out);
            out.push(']');
        }
        Value::String(s) => write_string(s, out),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
    }
}

fn pad(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push(' ');
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// `Reading Log.md`, byte for byte. The table is sorted by `updated` descending
/// with python's *stable* sort, so books written in the same second keep the
/// order they appear in the positions file.
pub fn reading_log(pos: &Positions) -> String {
    let mut rows = vec![
        "# Reading Log".to_string(),
        String::new(),
        "Written by narrator on every pause - last position per book.".to_string(),
        String::new(),
        "| book | chapter | chunk | updated |".to_string(),
        "|---|---|---|---|".to_string(),
    ];
    let mut entries: Vec<(&String, &Value)> = pos.iter().collect();
    // Stable, descending: python's `sorted(..., reverse=True)` reverses the
    // comparison but not the order of equal keys.
    entries.sort_by(|a, b| {
        let ka = a.1.get("updated").and_then(Value::as_str).unwrap_or("");
        let kb = b.1.get("updated").and_then(Value::as_str).unwrap_or("");
        kb.cmp(ka)
    });
    for (book, p) in entries {
        let stem = Path::new(book)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| book.clone());
        let g = |k: &str| p.get(k).and_then(Value::as_i64).unwrap_or(0);
        let title = p.get("chapter_title").and_then(Value::as_str).unwrap_or("");
        rows.push(format!(
            "| {stem} | {}/{} {title} | {}/{} | {} |",
            g("chapter") + 1,
            g("chapters_total"),
            g("chunk") + 1,
            g("chunks_total"),
            p.get("updated").and_then(Value::as_str).unwrap_or("")
        ));
    }
    rows.join("\n") + "\n"
}

/// Write both files. Every failure is a log line, never a panic: a vault on a
/// disconnected mount must not be able to stop playback.
pub fn write_positions(dir: &Path, pos: &Positions) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        positions_file(dir),
        dumps_indent1(&Value::Object(pos.clone())),
    )?;
    std::fs::write(log_file(dir), reading_log(pos))?;
    Ok(())
}

// ------------------------------------------------------------- fleeting notes

pub struct NoteInput<'a> {
    pub stamp: chrono::DateTime<chrono::Local>,
    pub book_title: &'a str,
    pub book_file: &'a str,
    pub chapter_title: &'a str,
    pub chapter: usize,
    pub chunk: usize,
    pub chunks_total: usize,
    pub context: &'a str,
    pub language: &'a str,
    pub audio_name: &'a str,
    pub text: &'a str,
}

/// The deep link the Obsidian plugin registers: it reopens the reader at exactly
/// this passage.
pub fn deep_link(book_file: &str, chapter: usize, chunk: usize) -> String {
    let enc = percent_encoding::utf8_percent_encode(book_file, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
        // python's `quote()` leaves these alone by default.
        .replace("%2F", "/")
        .replace("%2D", "-")
        .replace("%2E", ".")
        .replace("%5F", "_")
        .replace("%7E", "~");
    format!("obsidian://narrator-open?book={enc}&chapter={chapter}&chunk={chunk}")
}

/// Vault convention: `YYYYMMDDHHMM few words of the thought.md`.
pub fn note_filename(dir: &Path, stamp: &chrono::DateTime<chrono::Local>, text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| is_speakable_char(*c) || *c == '_' || *c == ' ' || *c == '-')
        .collect();
    let lowered = cleaned.to_lowercase();
    let words: Vec<&str> = lowered.split_whitespace().take(6).collect();
    let slug = if words.is_empty() {
        "voice note".to_string()
    } else {
        words.join(" ")
    };
    let name = format!("{} {slug}.md", stamp.format("%Y%m%d%H%M"));
    if dir.join(&name).exists() {
        // Same minute, same words.
        format!("{} {slug}.md", stamp.format("%Y%m%d%H%M%S"))
    } else {
        name
    }
}

/// The note body. The quote callout keeps the passage the thought was reacting
/// to, so a later pass over the vault knows what it was about.
pub fn note_markdown(n: &NoteInput<'_>) -> String {
    let deep = deep_link(n.book_file, n.chapter, n.chunk);
    [
        "---",
        "type: fleeting",
        "source: voice",
        &format!("book: \"{}\"", n.book_title),
        &format!("chapter: \"{}\"", n.chapter_title),
        &format!("position: chunk {}/{}", n.chunk + 1, n.chunks_total),
        &format!("language: {}", n.language),
        &format!("audio: {}", n.audio_name),
        &format!("captured: {}", n.stamp.format("%Y-%m-%dT%H:%M:%S")),
        "---",
        "",
        &format!(
            "> [!quote] [{} — {}]({deep})",
            n.book_title, n.chapter_title
        ),
        &format!("> {}", n.context),
        "",
        n.text,
        "",
        &format!("[⏮ this passage in narrator]({deep})"),
        "",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(chapter: i64, chunk: i64, title: &str, ct: i64, cht: i64, upd: &str) -> Value {
        serde_json::to_value(Position {
            chapter,
            chunk,
            chapter_title: title.into(),
            chunks_total: ct,
            chapters_total: cht,
            updated: upd.into(),
        })
        .unwrap_or(Value::Null)
    }

    #[test]
    fn positions_json_matches_python_dumps_indent_one() {
        let mut m = Positions::new();
        m.insert(
            "Bok \u{e5}.epub".into(),
            pos(0, 3, "Kapitel \u{e9}", 10, 2, "2026-01-01T00:00:00"),
        );
        let s = dumps_indent1(&Value::Object(m));
        assert_eq!(
            s,
            "{\n \"Bok \\u00e5.epub\": {\n  \"chapter\": 0,\n  \"chunk\": 3,\n  \
             \"chapter_title\": \"Kapitel \\u00e9\",\n  \"chunks_total\": 10,\n  \
             \"chapters_total\": 2,\n  \"updated\": \"2026-01-01T00:00:00\"\n }\n}"
        );
    }

    #[test]
    fn reading_log_sorts_newest_first_and_is_stable() {
        let mut m = Positions::new();
        m.insert(
            "a.epub".into(),
            pos(0, 0, "One", 5, 3, "2026-01-02T00:00:00"),
        );
        m.insert(
            "b.epub".into(),
            pos(1, 2, "Two", 7, 3, "2026-01-03T00:00:00"),
        );
        m.insert(
            "c.epub".into(),
            pos(2, 4, "Three", 9, 3, "2026-01-03T00:00:00"),
        );
        let log = reading_log(&m);
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "# Reading Log");
        assert_eq!(lines[4], "| book | chapter | chunk | updated |");
        assert_eq!(lines[6], "| b | 2/3 Two | 3/7 | 2026-01-03T00:00:00 |");
        assert_eq!(lines[7], "| c | 3/3 Three | 5/9 | 2026-01-03T00:00:00 |");
        assert_eq!(lines[8], "| a | 1/3 One | 1/5 | 2026-01-02T00:00:00 |");
        assert!(log.ends_with("|\n"));
    }

    #[test]
    fn an_empty_positions_file_still_writes_a_table_header() {
        let log = reading_log(&Positions::new());
        assert!(log.ends_with("|---|---|---|---|\n"));
    }

    #[test]
    fn note_slug_takes_six_cleaned_words() {
        let d = Path::new("/nonexistent");
        let t = chrono::Local
            .with_ymd_and_hms(2026, 9, 11, 15, 4, 5)
            .single()
            .expect("date");
        assert_eq!(
            note_filename(d, &t, "Hey! This, is a test of the system."),
            "202609111504 hey this is a test of.md"
        );
        assert_eq!(
            note_filename(d, &t, "!!! ???"),
            "202609111504 voice note.md"
        );
    }

    use chrono::TimeZone;
}
