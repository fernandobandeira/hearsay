//! The book as text: a small index plus byte-budgeted shards.
//!
//! Audio is expensive and render-bound; the words are not. A 200-minute business
//! book is ~180 kB of text, so the reader takes all of it on first open and can
//! then be read anywhere with voice notes still landing at the right passage. The
//! 1433-chapter *Lord of Mysteries* is ~17 MB, which is too much for one blob on
//! a phone — hence shards.
//!
//! Both files are rebuilt from scratch on every `/api/load`: re-parsing can move
//! chunk boundaries, and a stale shard would put every position in it on the
//! wrong words.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::book::Chapter;
use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChapMeta {
    pub i: usize,
    pub title: String,
    pub n: usize,
    pub est_min: f64,
    /// Which shard holds this chapter's words. Omitted entirely by `/api/load`,
    /// always a number in `book.json` — never `null`, which is why the schema
    /// overrides utoipa's default nullable-because-Option rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = usize)]
    pub shard: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BookIndex {
    pub key: String,
    pub name: String,
    pub title: String,
    pub total_min: f64,
    pub shards: usize,
    pub text_bytes: u64,
    pub chapters: Vec<ChapMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ShardChapter {
    pub i: usize,
    /// What lets the reader re-form real paragraphs from chunks — a wall of
    /// one-sentence blocks is not a page.
    pub paras: Vec<usize>,
    pub chunks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TextShard {
    pub shard: usize,
    pub from: usize,
    pub to: usize,
    pub chapters: Vec<ShardChapter>,
}

pub fn text_dir(work: &Path, key: &str) -> PathBuf {
    work.join("text").join(key)
}

/// `foo.json` -> `foo.json.gz`. The suffix is appended, never substituted: the
/// `.gz` is a sibling *of* the file, so a client that cannot take one can always
/// name the plain file it decodes to.
pub fn gz_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".gz");
    PathBuf::from(s)
}

/// Write a built `.json` and its `.gz` sibling in one breath, and return the
/// plain size.
///
/// Always together: a `.gz` that outlived its `.json` would serve the wrong
/// words, which is the same failure a stale shard is. A bundle is built in a
/// fresh directory and swapped in whole, so the pair can never be half-stale — and the plain
/// file is written first, so the only crash window leaves a *missing* `.gz`,
/// which the server falls back from silently.
///
/// `mtime = 0` keeps the output byte-deterministic. Level 9 because this is
/// written once per load and read by every device, over a tunnel, for the life
/// of the book.
pub fn write_json_gz(p: &Path, data: &[u8]) -> std::io::Result<u64> {
    use std::io::Write;
    std::fs::write(p, data)?;
    let mut enc = flate2::GzBuilder::new().mtime(0).write(
        Vec::with_capacity(data.len() / 3 + 64),
        flate2::Compression::new(9),
    );
    enc.write_all(data)?;
    std::fs::write(gz_path(p), enc.finish()?)?;
    Ok(data.len() as u64)
}

pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Write `<key>/index.json` + `<key>/NNN.json`.
///
/// Built whole in a hidden sibling directory and renamed into place, never
/// rewritten where it is served from. The old way — wipe the directory, then
/// write file by file — was a window of seconds on the big book (17 MB of
/// level-9 gzip) in which a device asking for its shards got a 404, a
/// half-written `.json`, or a truncated `.gz` whose plain twin already existed,
/// and each of those is served with an hour of `max-age`. Now a request sees
/// the old bundle or the new one: the only gap is between two renames.
pub fn write_bundle(
    cfg: &Config,
    plan: &[Chapter],
    est: &[f64],
    key: &str,
    name: &str,
    title: &str,
) -> std::io::Result<()> {
    // One build at a time: two loads of one book at once would otherwise sweep
    // each other's half-built directory away as a leftover.
    static BUILDING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = BUILDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let live = text_dir(&cfg.work, key);
    let parent = live.parent().unwrap_or(Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&parent)?;
    sweep_leftovers(&parent, key);
    let tag = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let d = parent.join(format!(".{key}.building-{tag}"));
    std::fs::create_dir_all(&d)?;
    let built = build_bundle_in(&d, cfg, plan, est, key, name, title);
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&d);
        return Err(e);
    }
    // A directory cannot be renamed over a non-empty one, so the live bundle
    // steps aside first and is deleted once the new one is in its place. If the
    // second rename fails the old one is put back rather than leaving the book
    // with no words at all.
    let old = parent.join(format!(".{key}.old-{tag}"));
    let had_old = match std::fs::rename(&live, &old) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&d);
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&d, &live) {
        if had_old {
            let _ = std::fs::rename(&old, &live);
        }
        let _ = std::fs::remove_dir_all(&d);
        return Err(e);
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

/// Remove what an interrupted [`write_bundle`] for this book left behind. Best
/// effort: a leftover costs disk, never correctness, since nothing serves from
/// a dot-directory.
fn sweep_leftovers(parent: &Path, key: &str) {
    let Ok(rd) = std::fs::read_dir(parent) else {
        return;
    };
    let (building, old) = (format!(".{key}.building-"), format!(".{key}.old-"));
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with(&building) || n.starts_with(&old) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn build_bundle_in(
    d: &Path,
    cfg: &Config,
    plan: &[Chapter],
    est: &[f64],
    key: &str,
    name: &str,
    title: &str,
) -> std::io::Result<()> {
    let mut shards: Vec<Vec<ShardChapter>> = Vec::new();
    let mut cur: Vec<ShardChapter> = Vec::new();
    let mut cur_bytes = 0usize;
    for c in plan {
        let body = ShardChapter {
            i: c.index,
            paras: c.chunks.iter().map(|k| k.para).collect(),
            chunks: c.chunks.iter().map(|k| k.text.clone()).collect(),
        };
        // The python estimate, verbatim: it is only a budget, but using a
        // different one would put different chapters in different shards and a
        // cached shard URL would answer with the wrong run.
        let size: usize = c.chunks.iter().map(|k| k.text.len() + 12).sum::<usize>() + 40;
        if !cur.is_empty()
            && (cur_bytes + size > cfg.text_shard_bytes || cur.len() >= cfg.text_shard_chapters)
        {
            shards.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(body);
        cur_bytes += size;
    }
    if !cur.is_empty() {
        shards.push(cur);
    }

    let mut of_shard = std::collections::HashMap::new();
    let mut total = 0u64;
    for (si, sh) in shards.iter().enumerate() {
        for c in sh {
            of_shard.insert(c.i, si);
        }
        let p = d.join(format!("{si:03}.json"));
        let doc = TextShard {
            shard: si,
            from: sh.first().map(|c| c.i).unwrap_or(0),
            to: sh.last().map(|c| c.i).unwrap_or(0),
            chapters: sh.clone(),
        };
        total += write_json_gz(&p, &serde_json::to_vec(&doc).unwrap_or_default())?;
    }

    let index = BookIndex {
        key: key.to_string(),
        name: name.to_string(),
        title: title.to_string(),
        total_min: round1(est.iter().sum::<f64>() / 60.0),
        shards: shards.len(),
        text_bytes: total,
        chapters: plan
            .iter()
            .zip(est)
            .map(|(c, e)| ChapMeta {
                i: c.index,
                title: c.display_title(),
                n: c.chunks.len(),
                est_min: round1(e / 60.0),
                shard: of_shard.get(&c.index).copied(),
            })
            .collect(),
    };
    write_json_gz(
        &d.join("index.json"),
        &serde_json::to_vec(&index).unwrap_or_default(),
    )?;
    Ok(())
}

/// One chapter out of a written bundle: the book's index, that chapter's index
/// entry, and its words. None if the book has no bundle or no such chapter.
///
/// Only the chapter's own shard is read, so the cost is a small index plus at
/// most ~1.5 MB even for a 1433-chapter book. Every reader below goes through
/// here: the bundle is the one description of a book that does not need the
/// session, and two of them reading it two ways is how they drift apart.
fn bundle_chapter(
    work: &Path,
    key: &str,
    ci: usize,
) -> Option<(BookIndex, ChapMeta, ShardChapter)> {
    if key.is_empty() {
        return None;
    }
    let d = text_dir(work, key);
    let index: BookIndex =
        serde_json::from_slice(&std::fs::read(d.join("index.json")).ok()?).ok()?;
    let meta = index.chapters.iter().find(|c| c.i == ci)?.clone();
    let shard: TextShard =
        serde_json::from_slice(&std::fs::read(d.join(format!("{:03}.json", meta.shard?))).ok()?)
            .ok()?;
    let c = shard.chapters.into_iter().find(|c| c.i == ci)?;
    Some((index, meta, c))
}

/// Read one chapter's words back out of a written bundle: `(title, paras,
/// chunks)`, or None if that book has no bundle or no such chapter.
///
/// This is what makes `/api/chapter/{ci}?book=` answerable without a session.
pub fn chapter_from_bundle(
    work: &Path,
    key: &str,
    ci: usize,
) -> Option<(String, Vec<usize>, Vec<String>)> {
    let (_, meta, c) = bundle_chapter(work, key, ci)?;
    Some((meta.title, c.paras, c.chunks))
}

/// The voice memo's view of a chapter: `(book title, file name, chapter title,
/// chunk texts)`, the loaded book not required.
///
/// A memo recorded against one book can arrive after the server has swapped to
/// another — it waited out an offline stretch in the reader's outbox — and the
/// note's quote, frontmatter and deep link have to describe the book it was
/// recorded against, not the one that happens to be open. The bundle is rebuilt
/// on every load of that book, so its chunk indices are exactly the ones the
/// memo refers to.
pub fn note_chapter(
    work: &Path,
    key: &str,
    ci: usize,
) -> Option<(String, String, String, Vec<String>)> {
    let (index, meta, c) = bundle_chapter(work, key, ci)?;
    Some((index.title, index.name, meta.title, c.chunks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Chunk;

    fn plan(n_chapters: usize, chunks_each: usize, chars: usize) -> Vec<Chapter> {
        (0..n_chapters)
            .map(|i| Chapter {
                index: i,
                id: format!("c{i}.xhtml"),
                title: String::new(),
                chunks: (0..chunks_each)
                    .map(|j| Chunk {
                        text: "x".repeat(chars),
                        para: j,
                        silent: false,
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn a_rebuild_swaps_the_bundle_whole_and_leaves_nothing_beside_it() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_bytes = 500;
        let est = vec![60.0; 6];
        write_bundle(&cfg, &plan(6, 4, 100), &est, "K", "k.epub", "K").expect("first");
        let live = text_dir(&cfg.work, "K");
        assert!(live.join("005.json").exists());
        // A leftover from a build that was killed half way.
        let stale = live.parent().expect("parent").join(".K.building-1-2");
        std::fs::create_dir_all(&stale).expect("stale");

        // Fewer chapters: a shard the old bundle had must not survive the swap.
        write_bundle(&cfg, &plan(2, 4, 100), &est[..2], "K", "k.epub", "K").expect("second");
        assert!(live.join("001.json").exists());
        assert!(live.join("001.json.gz").exists());
        assert!(
            !live.join("005.json").exists(),
            "a stale shard outlived the rebuild"
        );
        let names: Vec<String> = std::fs::read_dir(live.parent().expect("parent"))
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["K".to_string()],
            "nothing left beside the bundle"
        );
    }

    #[test]
    fn shards_respect_the_byte_budget_and_the_chapter_cap() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_bytes = 500;
        cfg.text_shard_chapters = 200;
        let p = plan(6, 4, 100); // ~452 bytes a chapter
        let est = vec![60.0; 6];
        write_bundle(&cfg, &p, &est, "K", "k.epub", "K").expect("write");
        let idx: BookIndex = serde_json::from_slice(
            &std::fs::read(text_dir(&cfg.work, "K").join("index.json")).expect("read"),
        )
        .expect("parse");
        assert_eq!(idx.shards, 6, "one chapter per shard at this budget");
        assert_eq!(idx.chapters[3].shard, Some(3));
        assert_eq!(idx.total_min, 6.0);
    }

    #[test]
    fn the_chapter_cap_wins_when_the_budget_does_not() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_bytes = 10_000_000;
        cfg.text_shard_chapters = 2;
        let p = plan(5, 1, 10);
        write_bundle(&cfg, &p, &[1.0; 5], "K", "k.epub", "K").expect("write");
        let idx: BookIndex = serde_json::from_slice(
            &std::fs::read(text_dir(&cfg.work, "K").join("index.json")).expect("read"),
        )
        .expect("parse");
        assert_eq!(idx.shards, 3);
        assert_eq!(idx.chapters[4].shard, Some(2));
    }

    #[test]
    fn a_chapter_reads_back_out_of_its_own_shard() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_chapters = 2;
        let p = plan(5, 3, 20);
        write_bundle(&cfg, &p, &[1.0; 5], "K", "k.epub", "K").expect("write");
        let (title, paras, chunks) = chapter_from_bundle(&cfg.work, "K", 4).expect("chapter 4");
        assert_eq!(title, "Section 5");
        assert_eq!(paras, vec![0, 1, 2]);
        assert_eq!(chunks.len(), 3);
        assert!(chapter_from_bundle(&cfg.work, "K", 99).is_none());
        assert!(chapter_from_bundle(&cfg.work, "nope", 0).is_none());
        assert!(chapter_from_bundle(&cfg.work, "", 0).is_none());
    }

    #[test]
    fn a_voice_memo_can_name_a_book_the_session_does_not_hold() {
        // Everything the fleeting note needs — the book's own title for the
        // frontmatter and the whisper prompt, its file name for the deep link,
        // the chapter's title, and the words to quote — out of the bundle, with
        // no session and no plan in memory.
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_chapters = 2;
        write_bundle(
            &cfg,
            &plan(5, 3, 20),
            &[1.0; 5],
            "Other Book (2019)",
            "Other Book (2019).epub",
            "Other Book",
        )
        .expect("write");
        let (title, name, ctitle, chunks) =
            note_chapter(&cfg.work, "Other Book (2019)", 3).expect("chapter 3");
        assert_eq!(title, "Other Book");
        assert_eq!(name, "Other Book (2019).epub");
        assert_eq!(ctitle, "Section 4");
        assert_eq!(chunks, vec!["x".repeat(20); 3]);
        // A book with no bundle, and a chapter the book does not have: both are
        // None, which is the 404 that keeps the recording in the outbox.
        assert!(note_chapter(&cfg.work, "Other Book (2019)", 99).is_none());
        assert!(note_chapter(&cfg.work, "never-loaded", 0).is_none());
        assert!(note_chapter(&cfg.work, "", 0).is_none());
    }

    #[test]
    fn a_rebuild_removes_stale_shards() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_chapters = 1;
        write_bundle(&cfg, &plan(4, 1, 10), &[1.0; 4], "K", "k.epub", "K").expect("write");
        let stale = text_dir(&cfg.work, "K").join("003.json");
        assert!(stale.exists() && gz_path(&stale).exists());
        write_bundle(&cfg, &plan(2, 1, 10), &[1.0; 2], "K", "k.epub", "K").expect("write");
        // The pair goes together: a .gz that outlived its .json would serve the
        // wrong words, which is the same failure a stale shard is.
        assert!(!stale.exists() && !gz_path(&stale).exists());
    }

    #[test]
    fn every_written_file_has_a_deterministic_gz_beside_it() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_chapters = 2;
        let p = plan(5, 6, 120);
        write_bundle(&cfg, &p, &[1.0; 5], "K", "k.epub", "K").expect("write");
        let dir = text_dir(&cfg.work, "K");
        let mut seen = 0;
        for f in ["index.json", "000.json", "001.json", "002.json"] {
            let plain = std::fs::read(dir.join(f)).expect("plain");
            let raw = std::fs::read(gz_path(&dir.join(f))).expect("gz");
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&raw[..]), &mut out)
                .expect("gzip");
            assert_eq!(out, plain, "{f}");
            seen += 1;
        }
        assert_eq!(seen, 4);
        // Written again from the same input, byte for byte: mtime=0 is what
        // keeps a rebuild from churning the bytes a proxy may have cached.
        let before: Vec<Vec<u8>> = ["index.json", "000.json"]
            .iter()
            .map(|f| std::fs::read(gz_path(&dir.join(f))).expect("gz"))
            .collect();
        write_bundle(&cfg, &p, &[1.0; 5], "K", "k.epub", "K").expect("rewrite");
        for (f, was) in ["index.json", "000.json"].iter().zip(before) {
            assert_eq!(
                std::fs::read(gz_path(&dir.join(f))).expect("gz"),
                was,
                "{f}"
            );
        }
    }
}
