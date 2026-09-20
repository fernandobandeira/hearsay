//! Chunking parity: the Rust chunker against golden JSON produced by running
//! narrator's own `app/book.py`.
//!
//! This is the test that matters most in the whole suite. A chunk index is a
//! reading position, a rendered wav's filename and a manifest entry all at once;
//! if the two implementations disagree by one boundary anywhere, every stored
//! position in that book lands on the wrong words and every packed chapter in the
//! cache becomes a lie. Regenerate the fixtures with `scripts/golden/regen.sh`.

use std::path::{Path, PathBuf};

use narrator::book::{
    build_plan_with, chunk_paragraph, est_chapter_s, est_chunk_s, extract_chapters, is_speakable,
    sentences, Chapter, Guards,
};
use serde::Deserialize;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let p = fixtures().join(name);
    let b = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_slice(&b).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[derive(Debug, Deserialize)]
struct ChunkCase {
    input: String,
    max_chars: usize,
    speakable: bool,
    sentences: Vec<String>,
    chunks: Vec<String>,
}

/// The one place this crate deliberately parts company with `app/book.py`.
///
/// Python's `_ABBR` guards are inert — the lookbehinds sit *before*
/// `(?<=[.!?])`, so each tests the two characters ending at the split point and
/// never the abbreviation — and `Dr. Smith` splits in two. That put a chunk
/// boundary, a full stop's worth of silence and a sentence-final fall in the
/// middle of a name, and the migration that fixed it is written up in
/// AGENTS.md.
///
/// The golden fixture is **not** re-blessed: it is what the python produces and
/// saying otherwise would make it worthless. Instead every case that moved is
/// listed here with what this chunker now does, so the divergence is exactly
/// this long and a change to any *other* case is still a failure. Ten cases
/// moved, and all ten are abbreviations.
const DIVERGES: &[(&str, &[&str], &[&str])] = &[
    (
        "Dr. Smith went home. He slept.",
        &["Dr. Smith went home.", "He slept."],
        &["Dr. Smith went home. He slept."],
    ),
    (
        "Mr. Brown and Mrs. Green and Ms. White met.",
        &["Mr. Brown and Mrs. Green and Ms. White met."],
        &["Mr. Brown and Mrs. Green and Ms. White met."],
    ),
    (
        "It was St. Peter, Jr. and Sr. together.",
        &["It was St. Peter, Jr. and Sr. together."],
        &["It was St. Peter, Jr. and Sr. together."],
    ),
    (
        "Cats vs. dogs. That is the question.",
        &["Cats vs. dogs.", "That is the question."],
        &["Cats vs. dogs. That is the question."],
    ),
    (
        "Tokenizers, e.g. this one, are slow.",
        &["Tokenizers, e.g. this one, are slow."],
        &["Tokenizers, e.g. this one, are slow."],
    ),
    (
        "Tokenizers, i.e. this one, are slow.",
        &["Tokenizers, i.e. this one, are slow."],
        &["Tokenizers, i.e. this one, are slow."],
    ),
    (
        "Bananas, apples, etc. were on the list.",
        &["Bananas, apples, etc. were on the list."],
        &["Bananas, apples, etc. were on the list."],
    ),
    ("See Dr. Smith.", &["See Dr. Smith."], &["See Dr. Smith."]),
    ("vs. that", &["vs. that"], &["vs. that"]),
    ("e.g. this", &["e.g. this"], &["e.g. this"]),
];

fn diverges(input: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    DIVERGES
        .iter()
        .find(|(i, _, _)| *i == input)
        .map(|(_, s, c)| (*s, *c))
}

#[test]
fn every_divergence_from_python_is_an_abbreviation_and_is_listed() {
    let cases: Vec<ChunkCase> = read_json("chunking_cases.json");
    let listed: Vec<&str> = DIVERGES.iter().map(|(i, _, _)| *i).collect();
    // Nothing in the list is stale: each one really is still in the fixture and
    // really does still differ from what python wrote.
    for input in &listed {
        let c = cases
            .iter()
            .find(|c| c.input == *input)
            .unwrap_or_else(|| panic!("{input:?} is no longer in the fixture"));
        assert_ne!(
            (sentences(&c.input), chunk_paragraph(&c.input, c.max_chars)),
            (c.sentences.clone(), c.chunks.clone()),
            "{input:?} no longer diverges — take it off the list"
        );
    }
    // And the divergence really is only ever about a `.` after an abbreviation.
    for (input, _, _) in DIVERGES {
        assert!(input.contains('.'), "{input:?} is not an abbreviation case");
    }
}

#[test]
fn sentence_splitting_and_chunking_match_python_exactly() {
    let cases: Vec<ChunkCase> = read_json("chunking_cases.json");
    assert!(cases.len() >= 40, "only {} cases", cases.len());
    let mut bad = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let got_s = sentences(&c.input);
        let got_c = chunk_paragraph(&c.input, c.max_chars);
        let got_sp = is_speakable(&c.input);
        // The listed abbreviation cases are asserted against what this chunker
        // does now; every other case is still asserted against the python.
        let (want_s, want_c) = match diverges(&c.input) {
            Some((s, k)) => (
                s.iter().map(|x| (*x).to_string()).collect::<Vec<_>>(),
                k.iter().map(|x| (*x).to_string()).collect::<Vec<_>>(),
            ),
            None => (c.sentences.clone(), c.chunks.clone()),
        };
        let (c_sentences, c_chunks) = (want_s, want_c);
        let c = &ChunkCase {
            input: c.input.clone(),
            max_chars: c.max_chars,
            speakable: c.speakable,
            sentences: c_sentences,
            chunks: c_chunks,
        };
        if got_s != c.sentences {
            bad.push(format!(
                "case {i} sentences\n  input: {:?}\n  want:  {:?}\n  got:   {:?}",
                c.input, c.sentences, got_s
            ));
        }
        if got_c != c.chunks {
            bad.push(format!(
                "case {i} chunks\n  input: {:?}\n  want:  {:?}\n  got:   {:?}",
                c.input, c.chunks, got_c
            ));
        }
        if got_sp != c.speakable {
            bad.push(format!(
                "case {i} speakable: input {:?} want {} got {got_sp}",
                c.input, c.speakable
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn the_fixture_epub_produces_the_same_plan() {
    let want: Vec<Chapter> = read_json("fixture_plan.json");
    let raw = extract_chapters(&fixtures().join("fixture.epub")).expect("parse fixture epub");
    let got = build_plan_with(&raw, 300, Guards::Inert);
    assert_eq!(got.len(), want.len(), "chapter count\ngot {got:#?}");
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(g.index, w.index);
        assert_eq!(g.id, w.id, "chapter {} id", w.index);
        assert_eq!(g.title, w.title, "chapter {} title", w.index);
        assert_eq!(
            g.chunks.len(),
            w.chunks.len(),
            "chapter {} chunk count\ngot {:#?}\nwant {:#?}",
            w.index,
            g.chunks,
            w.chunks
        );
        for (i, (gc, wc)) in g.chunks.iter().zip(&w.chunks).enumerate() {
            assert_eq!(gc, wc, "chapter {} chunk {i}", w.index);
        }
    }
}

#[derive(Debug, Deserialize)]
struct EstFixture {
    chars_per_sec: f64,
    params: EstParams,
    chunks: std::collections::BTreeMap<String, Vec<f64>>,
    chapters: std::collections::BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct EstParams {
    gap: f64,
    para_gap: f64,
    silence: f64,
}

#[test]
fn duration_estimates_match_python() {
    let want: EstFixture = read_json("fixture_est.json");
    assert_eq!(want.chars_per_sec, narrator::book::CHARS_PER_SEC);
    let raw = extract_chapters(&fixtures().join("fixture.epub")).expect("parse");
    let plan = build_plan_with(&raw, 300, Guards::Inert);
    for (ci, ch) in plan.iter().enumerate() {
        for (i, k) in ch.chunks.iter().enumerate() {
            let w = want.chunks[&ci.to_string()][i];
            let got = est_chunk_s(k, want.params.silence);
            assert!(
                (got - w).abs() < 1e-9,
                "chapter {ci} chunk {i}: {got} != {w}"
            );
        }
        let got = est_chapter_s(
            &ch.chunks,
            want.params.gap,
            want.params.para_gap,
            want.params.silence,
        );
        let w = want.chapters[&ci.to_string()];
        assert!((got - w).abs() < 1e-9, "chapter {ci}: {got} != {w}");
    }
}

// ------------------------------------------------------------------ real book

#[derive(Debug, Deserialize)]
struct RealCounts {
    source_name: String,
    bundled_path: Option<String>,
    sha256: String,
    max_chars: usize,
    chapters: Vec<RealChapter>,
}

#[derive(Debug, Deserialize)]
struct RealChapter {
    index: usize,
    id: String,
    title: String,
    n_chunks: usize,
    first_chunk_text: String,
    last_chunk_text: String,
    total_chars: usize,
}

fn sha256_hex(bytes: &[u8]) -> String {
    // A tiny SHA-256 so the test can verify it is looking at the same epub the
    // fixture was generated from without pulling in a crypto crate.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = bytes.to_vec();
    let bitlen = (bytes.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn real_book_path(f: &RealCounts) -> Option<PathBuf> {
    let p = f
        .bundled_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fixtures().join("real").join(&f.source_name));
    let p = if p.is_absolute() {
        p
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(p)
    };
    p.exists().then_some(p)
}

// The real-book fixtures (counts, head plan, and the epub itself) carry text
// from a commercial book, so they are never committed - these two tests run
// only on a machine where scripts/golden regenerated them locally.
fn real_fixtures_present() -> bool {
    fixtures().join("real_book_counts.json").exists()
}

#[test]
fn a_real_book_chunks_identically_to_python() {
    if !real_fixtures_present() {
        eprintln!("skipping: real-book fixtures not present (local-only, never committed)");
        return;
    }
    let want: RealCounts = read_json("real_book_counts.json");
    let Some(path) = real_book_path(&want) else {
        eprintln!("skipping: {} not bundled", want.source_name);
        return;
    };
    // The epub is copied out of the vault; never open the vault's own copy.
    let bytes = std::fs::read(&path).expect("read epub");
    assert_eq!(
        sha256_hex(&bytes),
        want.sha256,
        "the bundled epub is not the one the fixture was generated from"
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let copy = tmp.path().join(&want.source_name);
    std::fs::write(&copy, &bytes).expect("copy");

    let plan = build_plan_with(
        &extract_chapters(&copy).expect("parse"),
        want.max_chars,
        Guards::Inert,
    );
    assert_eq!(plan.len(), want.chapters.len(), "chapter count");
    let mut bad = Vec::new();
    for (g, w) in plan.iter().zip(&want.chapters) {
        if g.index != w.index || g.id != w.id || g.title != w.title {
            bad.push(format!(
                "chapter {}: id/title  got ({}, {:?})  want ({}, {:?})",
                w.index, g.id, g.title, w.id, w.title
            ));
        }
        if g.chunks.len() != w.n_chunks {
            bad.push(format!(
                "chapter {}: {} chunks, want {}",
                w.index,
                g.chunks.len(),
                w.n_chunks
            ));
            continue;
        }
        let total: usize = g.chunks.iter().map(|c| c.text.chars().count()).sum();
        if total != w.total_chars {
            bad.push(format!(
                "chapter {}: {total} chars, want {}",
                w.index, w.total_chars
            ));
        }
        if g.chunks[0].text != w.first_chunk_text {
            bad.push(format!(
                "chapter {} first chunk:\n  got  {:?}\n  want {:?}",
                w.index, g.chunks[0].text, w.first_chunk_text
            ));
        }
        let last = &g.chunks[g.chunks.len() - 1].text;
        if *last != w.last_chunk_text {
            bad.push(format!(
                "chapter {} last chunk:\n  got  {last:?}\n  want {:?}",
                w.index, w.last_chunk_text
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn the_first_chapters_of_a_real_book_match_chunk_for_chunk() {
    if !real_fixtures_present() {
        eprintln!("skipping: real-book fixtures not present (local-only, never committed)");
        return;
    }
    let counts: RealCounts = read_json("real_book_counts.json");
    let Some(path) = real_book_path(&counts) else {
        eprintln!("skipping: {} not bundled", counts.source_name);
        return;
    };
    #[derive(Debug, Deserialize)]
    struct HeadPlan {
        plan: Vec<Chapter>,
    }
    let want: Vec<Chapter> = read_json::<HeadPlan>("real_book_head_plan.json").plan;
    let tmp = tempfile::tempdir().expect("tempdir");
    let copy = tmp.path().join(&counts.source_name);
    std::fs::copy(&path, &copy).expect("copy");
    let plan = build_plan_with(
        &extract_chapters(&copy).expect("parse"),
        counts.max_chars,
        Guards::Inert,
    );
    for w in &want {
        let g = &plan[w.index];
        assert_eq!(g.chunks.len(), w.chunks.len(), "chapter {} count", w.index);
        for (i, (gc, wc)) in g.chunks.iter().zip(&w.chunks).enumerate() {
            assert_eq!(gc, wc, "chapter {} chunk {i}", w.index);
        }
    }
}

/// The strongest check there is, and the one that cannot live in git: compare
/// against the `plan.json` the *python server itself* wrote for the book
/// Fernando is actually reading.
///
/// Opt-in, because it needs a real narrator working tree. Point
/// `NARRATOR_REF_WORK` at one (default `~/git/narrator/work`) with a book
/// already loaded, and this re-chunks the same epub and compares chunk for
/// chunk. It only ever reads.
#[test]
fn a_python_written_plan_matches_chunk_for_chunk() {
    let work = std::env::var("NARRATOR_REF_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join("git/narrator/work"))
                .unwrap_or_default()
        });
    let books = std::env::var("NARRATOR_REF_BOOKS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join("git/narrator/books"))
                .unwrap_or_default()
        });
    let Ok(dirs) = std::fs::read_dir(work.join("audio")) else {
        eprintln!("skipping: no reference work dir at {}", work.display());
        return;
    };
    let mut checked = 0usize;
    for d in dirs.flatten() {
        let plan_file = d.path().join("plan.json");
        if !plan_file.exists() {
            continue;
        }
        let key = d.file_name().to_string_lossy().to_string();
        // The cache key is the stem truncated to 50, so find the epub by stem.
        let Some(epub) = std::fs::read_dir(&books).ok().and_then(|rd| {
            rd.flatten().map(|e| e.path()).find(|p| {
                p.extension().is_some_and(|x| x == "epub")
                    && p.file_stem()
                        .map(|s| s.to_string_lossy().chars().take(50).collect::<String>())
                        .as_deref()
                        == Some(key.as_str())
            })
        }) else {
            eprintln!("skipping {key}: no matching epub under {}", books.display());
            continue;
        };
        let want: Vec<Chapter> = match std::fs::read(&plan_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            Some(p) => p,
            None => continue,
        };
        // Copy out before parsing: the reference tree is read-only, always.
        let tmp = tempfile::tempdir().expect("tempdir");
        let copy = tmp.path().join(epub.file_name().unwrap_or_default());
        std::fs::copy(&epub, &copy).expect("copy");
        let got = build_plan_with(&extract_chapters(&copy).expect("parse"), 300, Guards::Inert);

        assert_eq!(got.len(), want.len(), "{key}: chapter count");
        let mut chunks = 0usize;
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(g.index, w.index, "{key}: index");
            assert_eq!(g.id, w.id, "{key}: chapter {} id", w.index);
            assert_eq!(g.title, w.title, "{key}: chapter {} title", w.index);
            assert_eq!(
                g.chunks.len(),
                w.chunks.len(),
                "{key}: chapter {} chunk count",
                w.index
            );
            for (i, (gc, wc)) in g.chunks.iter().zip(&w.chunks).enumerate() {
                assert_eq!(gc, wc, "{key}: chapter {} chunk {i}", w.index);
            }
            chunks += g.chunks.len();
        }
        eprintln!("{key}: {} chapters, {chunks} chunks, identical", got.len());
        checked += 1;
    }
    if checked == 0 {
        eprintln!(
            "skipping: nothing comparable under {} - a reference plan.json needs its \
             epub still present under {}",
            work.display(),
            books.display()
        );
    }
}
