//! EPUB -> a render plan: chapters, then sentence-aware chunks sized for TTS.
//!
//! A faithful port of `app/book.py`, **including its bugs**. A chunk index is
//! what a reading position *is*: every position in the vault, every rendered
//! wav, every packed chapter manifest is addressed by `(chapter, chunk)`. Moving
//! a boundary by one character silently relocates Fernando's place in a
//! 1433-chapter book and invalidates gigabytes of cache. So the two known quirks
//! below are reproduced deliberately and pinned by golden fixtures generated
//! from the Python implementation itself.
//!
//! **Quirk 1 — the abbreviation guards are inert.** `_ABBR` places lookbehinds
//! like `(?<!\bDr)` *before* `(?<=[.!?])`, so they test the two characters
//! ending at the split point, which are `r.`, never `Dr`. Every guard in the
//! list does nothing and "Dr. Smith" splits in two.
//!
//! **Quirk 2 — the split eats a closing quote.** `["'”’)\]]*` sits inside the
//! separator, so `He said "go." Then` loses the `"` after `go.`.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use unicode_general_category::{get_general_category, GeneralCategory};

use crate::err::BookError;

/// ~14 chars of text per second of finished speech, measured from the benchmark
/// passage. Estimates only — planning budgets and UI totals; the rendered wavs
/// are the truth.
pub const CHARS_PER_SEC: f64 = 14.0;
pub const DEFAULT_MAX_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub text: String,
    pub para: usize,
    pub silent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub chunks: Vec<Chunk>,
}

impl Chapter {
    /// The name the UI shows for a chapter with no TOC entry. The python server
    /// spells this out at six different call sites; it is one rule.
    pub fn display_title(&self) -> String {
        if self.title.is_empty() {
            format!("Section {}", self.index + 1)
        } else {
            self.title.clone()
        }
    }
}

/// One spine document's extracted paragraphs, before chunking.
#[derive(Debug, Clone)]
pub struct RawChapter {
    pub id: String,
    pub title: String,
    pub paragraphs: Vec<String>,
}

// ------------------------------------------------------------------ whitespace
// Python's `\s` on a str pattern is Unicode-aware and matches a slightly wider
// set than Rust's `char::is_whitespace`: CPython adds the C1 separators and
// NEXT LINE. Spelled out so a paragraph containing U+0085 chunks identically in
// both implementations.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1C}'..='\u{1F}' | '\u{85}')
}

/// `str.isalnum()`: alphabetic (L*) or numeric (Nd, Nl, No). Rust's
/// `is_alphanumeric` is Alphabetic|Nd, which disagrees on superscripts and
/// fractions — exactly the characters a footnote marker is made of. Public
/// because python's `\w` is this plus `_`, and the note filename slug needs it.
pub fn is_speakable_char(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
            | GeneralCategory::DecimalNumber
            | GeneralCategory::LetterNumber
            | GeneralCategory::OtherNumber
    )
}

fn trim_py(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// `re.sub(r"\s+", " ", t).strip()`
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if is_py_space(c) {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(c);
    }
    out
}

// -------------------------------------------------------------------- chunking

/// The characters the sentence separator swallows after the terminator.
fn is_closer(c: char) -> bool {
    matches!(c, '"' | '\'' | '\u{201D}' | '\u{2019}' | ')' | ']')
}

/// `_SENT.split(text)` — every split point of `(?<=[.!?])["'”’)\]]*\s+`,
/// hand-rolled because the abbreviation lookbehinds it carries are inert and
/// writing them out would only invite someone to "fix" them.
fn split_sentences_raw(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut parts = Vec::new();
    let mut last = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        // The separator can only start immediately after . ! ?
        if i == 0 || !matches!(b[i - 1], b'.' | b'!' | b'?') {
            i += 1;
            continue;
        }
        // `["'”’)\]]*` is greedy, and the regex engine backtracks: if `\s+`
        // fails at the greedy end, shorter runs of closers are tried, so a
        // match starts at the *first* position from which whitespace follows a
        // (possibly empty) run of closers.
        let mut j = i;
        let mut stops = vec![i];
        for (off, c) in text[i..].char_indices() {
            if is_closer(c) {
                j = i + off + c.len_utf8();
                stops.push(j);
            } else {
                break;
            }
        }
        let _ = j;
        let mut matched = None;
        // Greedy first, then shorter — the engine's own order.
        for &start in stops.iter().rev() {
            let rest = &text[start..];
            let ws: usize = rest
                .chars()
                .take_while(|c| is_py_space(*c))
                .map(char::len_utf8)
                .sum();
            if ws > 0 {
                matched = Some((start, start + ws));
                break;
            }
        }
        match matched {
            Some((_, end)) => {
                parts.push(&text[last..i]);
                last = end;
                i = end;
            }
            None => i += 1,
        }
    }
    parts.push(&text[last..]);
    parts
}

/// `sentences(text)` — split, strip, drop the empties.
pub fn sentences(text: &str) -> Vec<String> {
    split_sentences_raw(text)
        .into_iter()
        .map(trim_py)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `re.split(r"(?<=[,;:—])\s+", s)` — clause boundaries, for the rare sentence
/// longer than `max_chars`.
fn split_clauses(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut last = 0usize;
    let mut i = 0usize;
    while i < s.len() {
        if !s.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let prev_ok = i > 0
            && (matches!(b[i - 1], b',' | b';' | b':')
                || s[..i].ends_with('\u{2014}'));
        if !prev_ok {
            i += 1;
            continue;
        }
        let ws: usize = s[i..]
            .chars()
            .take_while(|c| is_py_space(*c))
            .map(char::len_utf8)
            .sum();
        if ws > 0 {
            parts.push(&s[last..i]);
            last = i + ws;
            i = last;
        } else {
            i += 1;
        }
    }
    parts.push(&s[last..]);
    parts
}

/// Python's `len()` counts characters, not bytes. Every length test in the
/// chunker goes through here; using `str::len` would move every boundary in a
/// book with an em-dash in it.
fn clen(s: &str) -> usize {
    s.chars().count()
}

/// `f"{a} {b}".strip()`
fn join_strip(a: &str, b: &str) -> String {
    let joined = format!("{a} {b}");
    trim_py(&joined).to_string()
}

/// Group whole sentences up to `max_chars`. Only a single over-long sentence is
/// ever split, and then on clause boundaries rather than mid-phrase.
pub fn chunk_paragraph(para: &str, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for s in sentences(para) {
        if clen(&s) > max_chars {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut buf = String::new();
            for p in split_clauses(&s) {
                if !buf.is_empty() && clen(&buf) + clen(p) + 1 > max_chars {
                    out.push(std::mem::replace(&mut buf, p.to_string()));
                } else {
                    buf = join_strip(&buf, p);
                }
            }
            if !buf.is_empty() {
                out.push(buf);
            }
        } else if !cur.is_empty() && clen(&cur) + clen(&s) + 1 > max_chars {
            out.push(std::mem::replace(&mut cur, s));
        } else {
            cur = join_strip(&cur, &s);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True if there is anything to pronounce.
///
/// A chunk like `…` or `“…”` has no phonetic content, but a TTS model is still
/// free to fill its token budget, so it hallucinates speech out of nothing.
/// These are marked instead and rendered as a short silence, which is what the
/// ellipsis meant anyway.
pub fn is_speakable(text: &str) -> bool {
    text.chars().any(is_speakable_char)
}

// -------------------------------------------------------------------- duration

pub fn est_chunk_s(chunk: &Chunk, silence: f64) -> f64 {
    if chunk.silent {
        silence
    } else {
        clen(&chunk.text) as f64 / CHARS_PER_SEC
    }
}

pub fn est_chapter_s(chunks: &[Chunk], gap: f64, para_gap: f64, silence: f64) -> f64 {
    let mut t = 0.0;
    let mut prev: Option<usize> = None;
    for k in chunks {
        if let Some(p) = prev {
            t += if k.para != p { para_gap } else { gap };
        }
        t += est_chunk_s(k, silence);
        prev = Some(k.para);
    }
    t
}

// ------------------------------------------------------------------- the plan

pub fn build_plan(chapters: &[RawChapter], max_chars: usize) -> Vec<Chapter> {
    let mut plan = Vec::new();
    for (ci, ch) in chapters.iter().enumerate() {
        let mut chunks = Vec::new();
        for (pi, para) in ch.paragraphs.iter().enumerate() {
            for text in chunk_paragraph(para, max_chars) {
                let silent = !is_speakable(&text);
                chunks.push(Chunk {
                    text,
                    para: pi,
                    silent,
                });
            }
        }
        if !chunks.is_empty() {
            plan.push(Chapter {
                index: ci,
                id: ch.id.clone(),
                title: ch.title.clone(),
                chunks,
            });
        }
    }
    plan
}

// --------------------------------------------------------------- EPUB parsing

/// The tags whose text becomes a paragraph, and the ones whose subtree is
/// dropped first. Both lists are `app/book.py`'s, in its order.
const KEEP: &[&str] = &["p", "h1", "h2", "h3", "blockquote"];
const DROP: &[&str] = &["script", "style", "nav", "header", "footer"];

/// Read an EPUB and walk its spine.
///
/// Lenient like `read_epub_lenient`: a manifest entry whose file is missing from
/// the archive (calibre leaves stale ones behind) becomes empty content instead
/// of failing the whole book.
pub fn extract_chapters(path: &Path) -> Result<Vec<RawChapter>, BookError> {
    let mut doc = epub::doc::EpubDoc::new(path)
        .map_err(|e| BookError::Epub(format!("{}: {e}", path.display())))?;

    let base = doc.root_base.clone();
    let strip = |p: &std::path::Path| -> String {
        let s = p.to_string_lossy().replace('\\', "/");
        let b = base.to_string_lossy().replace('\\', "/");
        match (b.is_empty(), s.strip_prefix(&b)) {
            (false, Some(rest)) => rest.trim_start_matches('/').to_string(),
            _ => s,
        }
    };

    // href -> title from the TOC, so chapters get real names. Nested nav points
    // are walked depth-first, parent before children, and a later entry for the
    // same href wins — `titles[...] = it.title` in a recursive walk.
    let mut titles: HashMap<String, String> = HashMap::new();
    fn walk(points: &[epub::doc::NavPoint], titles: &mut HashMap<String, String>, strip: &dyn Fn(&std::path::Path) -> String) {
        for p in points {
            let href = strip(&p.content);
            let href = href.split('#').next().unwrap_or("").to_string();
            titles.insert(href, p.label.clone());
            walk(&p.children, titles, strip);
        }
    }
    walk(&doc.toc.clone(), &mut titles, &strip);

    let spine: Vec<String> = doc.spine.iter().map(|s| s.idref.clone()).collect();
    let mut out = Vec::new();
    for sid in spine {
        let Some(res) = doc.resources.get(&sid).cloned() else {
            continue;
        };
        // ebooklib's ITEM_DOCUMENT is decided by the manifest's media-type.
        if !(res.mime.contains("xhtml") || res.mime.contains("html")) {
            continue;
        }
        let name = strip(&res.path);
        let html = doc
            .get_resource_str(&sid)
            .map(|(s, _)| s)
            .unwrap_or_default();
        let paras = paragraphs(&html);
        if paras.is_empty() {
            continue;
        }
        let title = titles
            .get(&name)
            .cloned()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| paras[0].chars().take(60).collect());
        out.push(RawChapter {
            id: name,
            title,
            paragraphs: paras,
        });
    }
    Ok(out)
}

/// The BeautifulSoup half: drop the noise subtrees, then take every `p`, `h1`,
/// `h2`, `h3` and `blockquote` **in document order, nested ones included**.
///
/// That last part looks like a bug and is load-bearing: `find_all` returns a
/// `<blockquote>` *and* the `<p>` inside it, so the quote's text appears twice
/// in the plan. Chunk indices are built on that, so it stays.
pub fn paragraphs(html: &str) -> Vec<String> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let Ok(drop_sel) = Selector::parse(&DROP.join(", ")) else {
        return Vec::new();
    };
    let dropped: std::collections::HashSet<_> = doc
        .select(&drop_sel)
        .flat_map(|e| e.descendants().map(|d| d.id()).collect::<Vec<_>>())
        .collect();
    let Ok(keep_sel) = Selector::parse(&KEEP.join(", ")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for el in doc.select(&keep_sel) {
        if dropped.contains(&el.id()) {
            continue;
        }
        // `get_text(" ", strip=True)`: every descendant string, stripped, empties
        // dropped, joined with a space.
        let text = el
            .text()
            .map(trim_py)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let t = collapse_ws(&text);
        if !t.is_empty() {
            out.push(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviation_guards_are_inert_bug_for_bug() {
        assert_eq!(sentences("Dr. Smith went home."), vec!["Dr.", "Smith went home."]);
        assert_eq!(sentences("e.g. this thing."), vec!["e.g.", "this thing."]);
    }

    #[test]
    fn the_split_eats_a_closing_quote() {
        assert_eq!(
            sentences("He said \"go.\" Then left."),
            vec!["He said \"go.", "Then left."]
        );
    }

    #[test]
    fn empty_and_blank_paragraphs_produce_nothing() {
        assert!(sentences("").is_empty());
        assert!(sentences("   \n\t ").is_empty());
        assert!(chunk_paragraph("", 300).is_empty());
    }

    #[test]
    fn sentences_are_grouped_up_to_max_chars() {
        let p = "One. Two. Three.";
        assert_eq!(chunk_paragraph(p, 300), vec!["One. Two. Three."]);
        // 4 + 4 + 1 > 8, so each sentence stands alone.
        assert_eq!(chunk_paragraph(p, 8), vec!["One.", "Two.", "Three."]);
        assert_eq!(chunk_paragraph(p, 9), vec!["One. Two.", "Three."]);
    }

    #[test]
    fn an_overlong_sentence_splits_on_clauses() {
        let s = format!("{}, {}, {}.", "a".repeat(40), "b".repeat(40), "c".repeat(40));
        let out = chunk_paragraph(&s, 50);
        assert!(out.len() > 1, "{out:?}");
        assert!(out.iter().all(|c| c.chars().count() <= 90));
    }

    #[test]
    fn speakable_follows_python_isalnum() {
        assert!(!is_speakable("\u{2026}"));
        assert!(!is_speakable("\u{201C}\u{2026}\u{201D}"));
        assert!(is_speakable("a"));
        assert!(is_speakable("\u{4E2D}"));
        // Superscript two is No: Python says alnum, Rust's is_alphanumeric does not.
        assert!(is_speakable("\u{B2}"));
    }

    #[test]
    fn char_lengths_not_byte_lengths() {
        // Six em-dashes: 6 chars, 18 bytes. With max_chars 6 this is one chunk.
        let p = "\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}";
        assert_eq!(chunk_paragraph(p, 6), vec![p]);
    }

    #[test]
    fn blockquote_nesting_duplicates_text() {
        let out = paragraphs("<html><body><blockquote><p>hi</p></blockquote></body></html>");
        assert_eq!(out, vec!["hi", "hi"]);
    }

    #[test]
    fn noise_subtrees_are_dropped() {
        let out = paragraphs(
            "<html><body><nav><p>toc</p></nav><header><p>h</p></header><p>real</p></body></html>",
        );
        assert_eq!(out, vec!["real"]);
    }

    #[test]
    fn estimates_match_the_python_arithmetic() {
        let chunks = vec![
            Chunk { text: "a".repeat(14), para: 0, silent: false },
            Chunk { text: "\u{2026}".into(), para: 0, silent: true },
            Chunk { text: "b".repeat(28), para: 1, silent: false },
        ];
        assert!((est_chunk_s(&chunks[0], 0.5) - 1.0).abs() < 1e-9);
        assert!((est_chunk_s(&chunks[1], 0.5) - 0.5).abs() < 1e-9);
        // 1.0 + 0.30 + 0.5 + 0.60 + 2.0
        assert!((est_chapter_s(&chunks, 0.30, 0.60, 0.5) - 4.4).abs() < 1e-9);
    }
}
