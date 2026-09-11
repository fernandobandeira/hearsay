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

pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Write `<key>/index.json` + `<key>/NNN.json`.
pub fn write_bundle(
    cfg: &Config,
    plan: &[Chapter],
    est: &[f64],
    key: &str,
    name: &str,
    title: &str,
) -> std::io::Result<()> {
    let d = text_dir(&cfg.work, key);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;

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
        std::fs::write(&p, serde_json::to_vec(&doc).unwrap_or_default())?;
        total += p.metadata().map(|m| m.len()).unwrap_or(0);
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
    std::fs::write(
        d.join("index.json"),
        serde_json::to_vec(&index).unwrap_or_default(),
    )?;
    Ok(())
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
    fn a_rebuild_removes_stale_shards() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.text_shard_chapters = 1;
        write_bundle(&cfg, &plan(4, 1, 10), &[1.0; 4], "K", "k.epub", "K").expect("write");
        assert!(text_dir(&cfg.work, "K").join("003.json").exists());
        write_bundle(&cfg, &plan(2, 1, 10), &[1.0; 2], "K", "k.epub", "K").expect("write");
        assert!(!text_dir(&cfg.work, "K").join("003.json").exists());
    }
}
