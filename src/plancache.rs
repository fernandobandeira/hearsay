//! The parse cache: `/api/load` must not re-parse a book that has not changed.
//!
//! Measured on the reader side against the 1433-chapter *Lord of Mysteries*
//! (7.8 MB EPUB, 17.5 MB of text): **12.4 s per `/api/load`**, every time, even
//! when nothing about the file had changed. The reading position comes back in
//! that response, so opening a book on a device that has never seen it could not
//! paint anything for twelve seconds.
//!
//! The plan is already written to disk as `plan.json` for `narrator export`. All
//! that was missing was a way to know it is still valid, which is what
//! `parse.json` beside it is: the source path, size and mtime the plan was built
//! from, plus `max_chars` and a format version. When those match, the plan is
//! read back instead of rebuilt — and because it is on disk, a container restart
//! is fast too, not just a second call.
//!
//! The safety argument matters more than the speed: re-parsing can move chunk
//! boundaries, and a moved boundary puts every stored position on the wrong
//! words. That is a *content* change, not a per-call one. Keying on
//! (path, size, mtime, max_chars) is exactly the statement "the content has not
//! changed"; if any of them differs the book is parsed again and the text bundle
//! is rebuilt from scratch, as before.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::book::Chapter;

/// Bumped whenever the chunker changes. A bump invalidates every cached plan,
/// which is the point: a chunker change *is* a migration.
const FORMAT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stamp {
    pub format: u32,
    pub path: String,
    pub size: u64,
    /// Nanoseconds since the epoch, as the filesystem reports them.
    pub mtime_ns: i128,
    pub max_chars: usize,
}

pub fn stamp(path: &Path, max_chars: usize) -> Option<Stamp> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let ns = match mtime.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    Some(Stamp {
        format: FORMAT,
        path: path.to_string_lossy().to_string(),
        size: md.len(),
        mtime_ns: ns,
        max_chars,
    })
}

fn stamp_path(work: &Path, key: &str) -> std::path::PathBuf {
    crate::cache::book_dir(work, key).join("parse.json")
}

/// Read back a plan whose stamp still matches the file on disk.
///
/// Returns None for anything unexpected — a missing file, a stamp that does not
/// match, JSON that will not parse. Every one of those simply means "parse it
/// again", never an error the caller has to handle.
pub fn load(work: &Path, key: &str, want: &Stamp) -> Option<Arc<Vec<Chapter>>> {
    let have: Stamp = serde_json::from_slice(&std::fs::read(stamp_path(work, key)).ok()?).ok()?;
    if have != *want {
        return None;
    }
    let plan: Vec<Chapter> =
        serde_json::from_slice(&std::fs::read(crate::cache::plan_path(work, key)).ok()?).ok()?;
    if plan.is_empty() {
        return None;
    }
    Some(Arc::new(plan))
}

/// Write `plan.json` and the stamp beside it. The stamp is written *last* and
/// only if the plan wrote cleanly, so a half-written plan can never be adopted.
pub fn store(work: &Path, key: &str, plan: &[Chapter], s: &Stamp) -> std::io::Result<()> {
    let p = crate::cache::plan_path(work, key);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("json.part");
    std::fs::write(
        &tmp,
        serde_json::to_vec(plan).unwrap_or_else(|_| b"[]".to_vec()),
    )?;
    std::fs::rename(&tmp, &p)?;
    std::fs::write(
        stamp_path(work, key),
        serde_json::to_vec(s).unwrap_or_default(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Chunk;

    fn plan() -> Vec<Chapter> {
        vec![Chapter {
            index: 0,
            id: "a.xhtml".into(),
            title: "A".into(),
            chunks: vec![Chunk {
                text: "hello".into(),
                para: 0,
                silent: false,
            }],
        }]
    }

    #[test]
    fn a_matching_stamp_reads_the_plan_back() {
        let d = tempfile::tempdir().expect("tempdir");
        let epub = d.path().join("b.epub");
        std::fs::write(&epub, b"pretend").expect("write");
        let s = stamp(&epub, 300).expect("stamp");
        store(d.path(), "B", &plan(), &s).expect("store");
        assert_eq!(load(d.path(), "B", &s).map(|p| p.len()), Some(1));
    }

    #[test]
    fn a_changed_file_invalidates_the_plan() {
        let d = tempfile::tempdir().expect("tempdir");
        let epub = d.path().join("b.epub");
        std::fs::write(&epub, b"pretend").expect("write");
        let s = stamp(&epub, 300).expect("stamp");
        store(d.path(), "B", &plan(), &s).expect("store");
        std::fs::write(&epub, b"pretend it is longer now").expect("rewrite");
        let s2 = stamp(&epub, 300).expect("stamp");
        assert!(load(d.path(), "B", &s2).is_none(), "size changed");
        // And a different chunking is a different plan, whatever the file says.
        let mut s3 = s.clone();
        s3.max_chars = 200;
        assert!(load(d.path(), "B", &s3).is_none());
    }

    #[test]
    fn a_missing_or_corrupt_cache_is_simply_a_miss() {
        let d = tempfile::tempdir().expect("tempdir");
        let epub = d.path().join("b.epub");
        std::fs::write(&epub, b"x").expect("write");
        let s = stamp(&epub, 300).expect("stamp");
        assert!(load(d.path(), "B", &s).is_none());
        std::fs::create_dir_all(crate::cache::book_dir(d.path(), "B")).expect("dir");
        std::fs::write(stamp_path(d.path(), "B"), b"{ not json").expect("write");
        assert!(load(d.path(), "B", &s).is_none());
    }
}
