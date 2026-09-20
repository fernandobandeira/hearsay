//! `narrator migrate` — re-chunk the cached books and drop only what that
//! invalidated.
//!
//! A CLI path, not a server one, for the same reasons `export` is: it is a
//! one-shot over a work directory, it is destructive, and nobody should be
//! waiting on an HTTP response while it runs.
//!
//! **Why it has to exist.** A chunk index *is* a reading position and *is* the
//! name of a wav on disk. Change the chunker and `chNNN/00042.wav` still exists
//! and still looks rendered, but it now holds the audio of some other text —
//! and the render worker's disk-truth rule reads a present file as done, so
//! nothing would ever fix it. Moving a boundary therefore means finding every
//! artifact that described the old boundaries and removing it.
//!
//! **What it deletes, and what it deliberately keeps.** Chunks before the first
//! one whose text changed are byte-identical under both chunkers, so their
//! audio is still correct and is kept: on *Lord of Mysteries* that is the
//! difference between re-rendering 11168 chunks and re-rendering 19332. What
//! goes is every chunk from the first changed index to the end of that chapter,
//! plus anything derived from the whole chapter — its packed m4a, its manifest,
//! its HLS — because those are built from all of it.
//!
//! A chapter whose chunks are identical under both chunkers is not touched at
//! all.
//!
//! **Positions.** A position in an unaffected chapter is still exactly right. A
//! position at or past the first changed chunk of an affected chapter no longer
//! means what it did, so it is pulled back to that first changed chunk — the
//! last point the two chunkings agree on. Never forward: re-reading a paragraph
//! is a much smaller injury than skipping one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::book::{self, Chapter};
use crate::{cache, chapters, plancache};

pub const USAGE: &str = "\
narrator migrate — re-chunk cached books and drop only what that invalidated

  narrator migrate                 report what would change, touch nothing
  narrator migrate --apply         do it
  narrator migrate --book <key>    just this cache directory
  narrator migrate --dir <path>    work directory (default: NARRATOR_WORK)

Without --apply nothing is written, deleted or renamed.";

pub const RETRIM_USAGE: &str = "\
narrator retrim — take Kokoro's padding out of chunk wavs already on disk

  narrator retrim                  report what would change, touch nothing
  narrator retrim --apply          rewrite them
  narrator retrim --book <key>     just this cache directory
  narrator retrim --dir <path>     work directory (default: NARRATOR_WORK)

Idempotent: a wav the renderer already trimmed is left alone. Chunk indices,
manifests and positions are untouched — only the silence around the speech.";

#[derive(Debug, Clone)]
pub struct Args {
    pub work: PathBuf,
    pub books: Vec<PathBuf>,
    pub positions: Option<PathBuf>,
    pub only: Option<String>,
    pub apply: bool,
    pub max_chars: usize,
}

/// What a single book's migration comes to.
#[derive(Debug, Default, Clone)]
pub struct BookPlan {
    pub key: String,
    pub source: PathBuf,
    /// chapter index -> first chunk index whose text changed.
    pub changed: BTreeMap<usize, usize>,
    pub chapters_total: usize,
    pub chunks_before: usize,
    pub chunks_after: usize,
    /// Chunk wavs that exist on disk and would be deleted.
    pub wavs: Vec<PathBuf>,
    /// Packed m4a/manifest and HLS directories that would be deleted.
    pub derived: Vec<PathBuf>,
    /// The new plan, kept so `--apply` does not parse twice.
    pub plan: Vec<Chapter>,
}

impl BookPlan {
    pub fn is_noop(&self) -> bool {
        self.changed.is_empty()
    }
}

/// Compare the plan on disk with what the current chunker produces.
///
/// The first differing chunk is the whole answer: everything before it is
/// identical text in an identically numbered slot, and everything from it on
/// has to go. A chapter that gained or lost chunks with no earlier difference
/// changes from the length they had in common.
pub fn diff_chapters(old: &[Chapter], new: &[Chapter]) -> BTreeMap<usize, usize> {
    let mut out = BTreeMap::new();
    for (o, n) in old.iter().zip(new.iter()) {
        let common = o
            .chunks
            .iter()
            .zip(n.chunks.iter())
            .position(|(a, b)| a.text != b.text || a.para != b.para || a.silent != b.silent);
        let first = common.or_else(|| {
            (o.chunks.len() != n.chunks.len()).then_some(o.chunks.len().min(n.chunks.len()))
        });
        if let Some(f) = first {
            out.insert(o.index, f);
        }
    }
    // A chapter count that moved is not something this can reconcile chunk by
    // chunk; every chapter past the shorter list is new or gone.
    for extra in old.len().min(new.len())..old.len().max(new.len()) {
        out.entry(extra).or_insert(0);
    }
    out
}

/// Work out what migrating one cached book would do. Reads only.
pub fn plan_book(args: &Args, key: &str) -> Result<BookPlan, String> {
    let dir = cache::book_dir(&args.work, key);
    let plan_path = dir.join("plan.json");
    let raw =
        std::fs::read_to_string(&plan_path).map_err(|e| format!("{}: {e}", plan_path.display()))?;
    let old: Vec<Chapter> =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", plan_path.display()))?;

    let source = find_source(args, key).ok_or_else(|| {
        format!(
            "no epub found for {key:?} under {:?} — cannot re-chunk it",
            args.books
        )
    })?;
    let raws = book::extract_chapters(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    let new = book::build_plan(&raws, args.max_chars);

    let changed = diff_chapters(&old, &new);
    let mut out = BookPlan {
        key: key.to_string(),
        source,
        chapters_total: old.len(),
        chunks_before: old.iter().map(|c| c.chunks.len()).sum(),
        chunks_after: new.iter().map(|c| c.chunks.len()).sum(),
        changed,
        plan: new,
        ..Default::default()
    };

    for (&ci, &first) in &out.changed {
        let n = old.get(ci).map_or(0, |c| c.chunks.len());
        for i in first..n {
            let p = cache::chunk_path(&args.work, key, ci, i);
            if p.is_file() {
                out.wavs.push(p);
            }
        }
        let (m4a, manifest) = chapters::chapter_files(&args.work, key, ci);
        for p in [m4a, manifest] {
            if p.is_file() {
                out.derived.push(p);
            }
        }
        let hls = chapters::hls_dir(&args.work, key, ci);
        if hls.is_dir() {
            out.derived.push(hls);
        }
    }
    Ok(out)
}

/// The epub a cache directory came from. The key is the file stem truncated to
/// 50 characters, so the match is on that same truncation rather than on
/// equality — a long title has a key that is not its stem.
fn find_source(args: &Args, key: &str) -> Option<PathBuf> {
    for root in &args.books {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        let mut stack: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    stack.extend(rd.flatten().map(|e| e.path()));
                }
                continue;
            }
            if p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
                && cache::book_key(&p.to_string_lossy()) == key
            {
                return Some(p);
            }
        }
    }
    None
}

/// Apply one book's plan: delete the invalidated artifacts, then write the new
/// plan and its stamp. The plan is written **last**, so an interrupted run
/// leaves a cache with holes and the old plan — which the render worker heals
/// by itself — rather than a new plan over stale audio, which it cannot.
pub fn apply_book(args: &Args, b: &BookPlan) -> Result<(), String> {
    for p in &b.wavs {
        std::fs::remove_file(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    for p in &b.derived {
        let r = if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        };
        r.map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let stamp = plancache::stamp(&b.source, args.max_chars)
        .ok_or_else(|| format!("could not stamp {}", b.source.display()))?;
    plancache::store(&args.work, &b.key, &b.plan, &stamp)
        .map_err(|e| format!("writing the plan for {}: {e}", b.key))?;
    Ok(())
}

/// Every cache directory that has a plan to migrate.
pub fn cached_books(work: &Path) -> Vec<String> {
    let mut keys: Vec<String> = std::fs::read_dir(cache::audio_root(work))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("plan.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    keys.sort();
    keys
}

/// A position that no longer means what it did, and where it is pulled back to.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionFix {
    pub book: String,
    pub chapter: i64,
    pub from: i64,
    pub to: i64,
}

/// Which stored positions this migration would move, and where to.
///
/// Only a position at or past its chapter's first changed chunk is touched;
/// everything before that point chunks identically, so the position still names
/// the same words. The fix is always backwards, to the last chunk the two
/// chunkings agree on — a reader who finds themselves a paragraph early has
/// lost a few seconds, and one who finds themselves a paragraph late has lost
/// the thread.
pub fn position_fixes(pos: &crate::vault::Positions, books: &[BookPlan]) -> Vec<PositionFix> {
    let mut out = Vec::new();
    for b in books {
        for (name, v) in pos.iter() {
            if cache::book_key(name) != b.key {
                continue;
            }
            let (Some(ch), Some(ck)) = (
                v.get("chapter").and_then(serde_json::Value::as_i64),
                v.get("chunk").and_then(serde_json::Value::as_i64),
            ) else {
                continue;
            };
            let Ok(ci) = usize::try_from(ch) else {
                continue;
            };
            let Some(&first) = b.changed.get(&ci) else {
                continue;
            };
            let first = first as i64;
            if ck >= first {
                out.push(PositionFix {
                    book: name.clone(),
                    chapter: ch,
                    from: ck,
                    to: first,
                });
            }
        }
    }
    out
}

/// Write the pulled-back positions and the chunk totals that went with the old
/// chunking, leaving every other field of the record as it was.
///
/// `updated` is restamped, and it has to be. A device that still holds the old
/// position in its outbox compares timestamps to decide who is further along
/// (`resolveResume` in the reader), and a healed record carrying the old stamp
/// loses that comparison — the reader would put itself straight back on a chunk
/// index that no longer names those words. The field means "when this record was
/// written", and it is being written now.
pub fn apply_position_fixes(
    dir: &Path,
    fixes: &[PositionFix],
    books: &[BookPlan],
) -> Result<(), String> {
    if fixes.is_empty() {
        return Ok(());
    }
    let mut pos = crate::vault::load_positions(dir);
    for f in fixes {
        let Some(rec) = pos.get_mut(&f.book).and_then(|v| v.as_object_mut()) else {
            continue;
        };
        rec.insert("chunk".into(), f.to.into());
        rec.insert("updated".into(), crate::vault::now_iso_seconds().into());
        if let Some(b) = books.iter().find(|b| cache::book_key(&f.book) == b.key) {
            if let Some(c) = b.plan.get(f.chapter as usize) {
                rec.insert("chunks_total".into(), (c.chunks.len() as i64).into());
            }
        }
    }
    crate::vault::write_positions(dir, &pos).map_err(|e| format!("{}: {e}", dir.display()))
}

/// `--dir`, `--book`, `--apply`; everything else comes from the environment the
/// server itself reads, so a migration run in the container sees the same work
/// directory, book roots and vault the server does.
pub fn args_from(argv: &[String], cfg: &crate::config::Config) -> Result<Args, String> {
    let mut a = Args {
        work: cfg.work.clone(),
        books: cfg.books.clone(),
        positions: Some(cfg.positions_dir.clone()),
        only: None,
        apply: false,
        max_chars: book::DEFAULT_MAX_CHARS,
    };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--apply" => a.apply = true,
            "--dry-run" => a.apply = false,
            "--dir" => a.work = it.next().ok_or("--dir needs a path")?.into(),
            "--book" => a.only = Some(it.next().ok_or("--book needs a key")?.clone()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    Ok(a)
}

// ------------------------------------------------------------------- retrim

/// What trimming the padding out of the wavs already on disk comes to.
#[derive(Debug, Default, Clone)]
pub struct TrimReport {
    pub examined: usize,
    pub trimmed: usize,
    pub failed: usize,
    pub seconds_before: f64,
    pub seconds_after: f64,
}

impl TrimReport {
    pub fn saved(&self) -> f64 {
        self.seconds_before - self.seconds_after
    }
}

/// Take Kokoro's padding out of the chunk wavs that are already rendered.
///
/// The trim is the same function the renderer now applies, so a wav that has
/// already been through it is left exactly as it is — the pass is idempotent
/// and can be re-run. A wav that will not read is counted and skipped rather
/// than deleted: a file this cannot parse is one the packer will complain about
/// with a much better error than an empty chapter directory.
///
/// Rewriting through `write_wav` means every file goes via a `.part` and a
/// rename, so an interrupted run can leave a stray `.part` but never a
/// truncated wav in the cache.
pub fn retrim(work: &Path, key: &str, apply: bool) -> TrimReport {
    let mut r = TrimReport::default();
    let dir = cache::book_dir(work, key);
    let Ok(chapters) = std::fs::read_dir(&dir) else {
        return r;
    };
    let mut chapter_dirs: Vec<PathBuf> = chapters
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    chapter_dirs.sort();
    for cd in chapter_dirs {
        let Ok(rd) = std::fs::read_dir(&cd) else {
            continue;
        };
        let mut wavs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "wav"))
            .collect();
        wavs.sort();
        for p in wavs {
            r.examined += 1;
            let Ok(samples) = read_wav_f32(&p) else {
                r.failed += 1;
                continue;
            };
            let before = samples.len();
            let mut out = samples;
            crate::tts::kokoro::trim_padding(&mut out);
            r.seconds_before += before as f64 / cache::SR as f64;
            r.seconds_after += out.len() as f64 / cache::SR as f64;
            if out.len() == before {
                continue;
            }
            r.trimmed += 1;
            if apply && cache::write_wav(&p, &out).is_err() {
                r.failed += 1;
            }
        }
    }
    r
}

fn read_wav_f32(path: &Path) -> Result<Vec<f32>, String> {
    let mut rd = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = rd.spec();
    if spec.channels != 1 || spec.bits_per_sample != 16 {
        return Err(format!(
            "{}: expected mono s16, got {} channels at {} bits",
            path.display(),
            spec.channels,
            spec.bits_per_sample
        ));
    }
    rd.samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32767.0).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Chunk;

    fn chap(index: usize, texts: &[&str]) -> Chapter {
        Chapter {
            index,
            id: format!("c{index}"),
            title: format!("Chapter {index}"),
            chunks: texts
                .iter()
                .map(|t| Chunk {
                    text: (*t).to_string(),
                    para: 0,
                    silent: false,
                })
                .collect(),
        }
    }

    #[test]
    fn an_unchanged_chapter_is_not_in_the_diff_at_all() {
        let a = vec![chap(0, &["one", "two", "three"])];
        assert!(diff_chapters(&a, &a).is_empty());
    }

    #[test]
    fn the_diff_is_the_first_chunk_whose_text_moved() {
        let old = vec![chap(0, &["one", "two", "three", "four"])];
        let new = vec![chap(0, &["one", "two", "CHANGED", "four"])];
        assert_eq!(diff_chapters(&old, &new), BTreeMap::from([(0, 2)]));
    }

    #[test]
    fn a_chapter_that_lost_a_chunk_changes_from_where_they_stop_agreeing() {
        // The abbreviation fix merges two sentences, so a chunk disappears and
        // everything after it shifts down one.
        let old = vec![chap(0, &["a", "Mr.", "Franky came.", "b"])];
        let new = vec![chap(0, &["a", "Mr. Franky came.", "b"])];
        assert_eq!(diff_chapters(&old, &new), BTreeMap::from([(0, 1)]));
        // And one that only grew at the end.
        let old = vec![chap(0, &["a", "b"])];
        let new = vec![chap(0, &["a", "b", "c"])];
        assert_eq!(diff_chapters(&old, &new), BTreeMap::from([(0, 2)]));
    }

    #[test]
    fn only_the_chapters_that_moved_are_listed() {
        let old = vec![chap(0, &["a"]), chap(1, &["b", "c"]), chap(2, &["d"])];
        let new = vec![chap(0, &["a"]), chap(1, &["b", "CHANGED"]), chap(2, &["d"])];
        assert_eq!(diff_chapters(&old, &new), BTreeMap::from([(1, 1)]));
    }

    fn book_with(changed: &[(usize, usize)], plan: Vec<Chapter>) -> BookPlan {
        BookPlan {
            key: "Book".into(),
            changed: changed.iter().copied().collect(),
            plan,
            ..Default::default()
        }
    }

    fn positions(json: &str) -> crate::vault::Positions {
        serde_json::from_str(json).expect("positions")
    }

    #[test]
    fn a_position_before_the_first_change_does_not_move() {
        let p = positions(r#"{"Book.epub":{"chapter":3,"chunk":4}}"#);
        let b = book_with(&[(3, 9)], vec![]);
        assert!(position_fixes(&p, &[b]).is_empty());
    }

    #[test]
    fn a_position_in_an_untouched_chapter_does_not_move() {
        let p = positions(r#"{"Book.epub":{"chapter":3,"chunk":40}}"#);
        let b = book_with(&[(7, 0)], vec![]);
        assert!(position_fixes(&p, &[b]).is_empty());
    }

    #[test]
    fn a_position_past_the_first_change_is_pulled_back_to_it() {
        let p = positions(r#"{"Book.epub":{"chapter":3,"chunk":40}}"#);
        let b = book_with(&[(3, 9)], vec![]);
        let f = position_fixes(&p, &[b]);
        assert_eq!(
            f,
            vec![PositionFix {
                book: "Book.epub".into(),
                chapter: 3,
                from: 40,
                to: 9,
            }]
        );
        // Backwards, always: never past where the two chunkings agree.
        assert!(f[0].to <= f[0].from);
    }

    #[test]
    fn a_position_for_another_book_is_left_alone() {
        let p = positions(r#"{"Other.epub":{"chapter":3,"chunk":40}}"#);
        let b = book_with(&[(3, 0)], vec![]);
        assert!(position_fixes(&p, &[b]).is_empty());
    }

    #[test]
    fn retrimming_is_idempotent_and_leaves_the_speech_alone() {
        let d = tempfile::tempdir().expect("tempdir");
        let work = d.path();
        let p = cache::chunk_path(work, "Book", 0, 0);
        let sr = cache::SR as usize;
        let mut w = vec![0.0f32; sr];
        for s in w.iter_mut().skip(sr / 2).take(sr / 10) {
            *s = 0.5;
        }
        cache::write_wav(&p, &w).expect("write");

        let dry = retrim(work, "Book", false);
        assert_eq!((dry.examined, dry.trimmed, dry.failed), (1, 1, 0));
        assert!(dry.saved() > 0.5, "{}", dry.saved());
        // --dry-run wrote nothing.
        assert_eq!(cache::wav_info(&p).expect("info").3, 1.0);

        let first = retrim(work, "Book", true);
        assert_eq!(first.trimmed, 1);
        let after = cache::wav_info(&p).expect("info").3;
        assert!(after < 0.2, "{after}");

        // Run it again: nothing left to take off.
        let second = retrim(work, "Book", true);
        assert_eq!((second.examined, second.trimmed), (1, 0));
        assert_eq!(cache::wav_info(&p).expect("info").3, after);
    }

    #[test]
    fn retrimming_a_book_with_no_cache_is_not_an_error() {
        let d = tempfile::tempdir().expect("tempdir");
        let r = retrim(d.path(), "Nothing", true);
        assert_eq!((r.examined, r.trimmed, r.failed), (0, 0, 0));
    }
}
