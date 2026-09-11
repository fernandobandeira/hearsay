//! Chapter-level audio: one small AAC file per fully rendered chapter, plus the
//! manifest that keeps chunk indices meaningful inside it.
//!
//! A port of `app/chapters.py`. Concatenating loses the chunk index, and a chunk
//! index is what a reading position *is*, so every `.m4a` is built with a
//! manifest beside it mapping chunk -> start second, derived from the real WAV
//! durations plus the same inter-chunk (0.30 s) and paragraph (0.60 s) gaps the
//! `.m4b` export uses.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::book::Chunk;
use crate::cache;
use crate::config::Config;
use crate::err::PackError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct Manifest {
    pub book: String,
    pub chapter: usize,
    pub title: String,
    pub chunks: usize,
    /// `starts[i]` is chunk `i`'s start second. `currentTime` maps back by
    /// bisecting this.
    pub starts: Vec<f64>,
    pub duration: f64,
    pub gap: f64,
    pub para_gap: f64,
    pub bitrate: String,
    pub sample_rate: u32,
    pub bytes: u64,
    pub built: String,
    pub build_s: f64,
}

/// Chapters being packed right now. gc on either side of the cache must leave
/// these alone: the chunk wavs are the build's input and a half-written m4a is
/// not a file anyone should see.
static BUILDING: Mutex<Option<HashSet<(String, usize)>>> = Mutex::new(None);

fn building_insert(key: &str, ci: usize) {
    if let Ok(mut g) = BUILDING.lock() {
        g.get_or_insert_with(HashSet::new).insert((key.into(), ci));
    }
}

fn building_remove(key: &str, ci: usize) {
    if let Ok(mut g) = BUILDING.lock() {
        if let Some(s) = g.as_mut() {
            s.remove(&(key.into(), ci));
        }
    }
}

fn building_tags() -> HashSet<String> {
    BUILDING
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|(k, c)| format!("{k}/ch{c:03}"))
        .collect()
}

pub fn book_dir(work: &Path, key: &str) -> PathBuf {
    work.join("chapters").join(key)
}

pub fn chapter_files(work: &Path, key: &str, ci: usize) -> (PathBuf, PathBuf) {
    let d = book_dir(work, key);
    (
        d.join(format!("ch{ci:03}.m4a")),
        d.join(format!("ch{ci:03}.json")),
    )
}

pub fn read_manifest(work: &Path, key: &str, ci: usize) -> Option<Manifest> {
    let (_, j) = chapter_files(work, key, ci);
    serde_json::from_slice(&std::fs::read(j).ok()?).ok()
}

/// Chunk index playing at `seconds` — the inverse of the manifest.
pub fn chunk_at(m: &Manifest, seconds: f64) -> usize {
    let mut lo = 0usize;
    let mut hi = m.starts.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if m.starts[mid] <= seconds {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.saturating_sub(1).min(m.starts.len().saturating_sub(1))
}

/// `CHAPTER_BITRATE` as bits per second: ffmpeg's own spelling, which is a bare
/// number of bits or a number with a `k`/`m` suffix.
///
/// The reader needs this to say how big a download will be *before* anything is
/// packed, and it used to hard-code the 64 k default — so changing the env var
/// on the box silently made every size in the UI wrong by that ratio. Garbage
/// falls back to 64 k rather than to zero: an estimate that is off is worth more
/// than one that claims a chapter weighs nothing.
pub fn bitrate_bps(spec: &str) -> u64 {
    const DEFAULT: u64 = 64_000;
    let s = spec.trim().to_ascii_lowercase();
    let s = s
        .strip_suffix("bit/s")
        .or_else(|| s.strip_suffix("bits"))
        .or_else(|| s.strip_suffix("bps"))
        .or_else(|| s.strip_suffix("bit"))
        .unwrap_or(&s)
        .trim()
        .to_string();
    let (num, mult) = match s.strip_suffix('k') {
        Some(n) => (n, 1_000u64),
        None => match s.strip_suffix('m') {
            Some(n) => (n, 1_000_000),
            None => (s.as_str(), 1),
        },
    };
    match num.trim().parse::<f64>() {
        Ok(v) if v > 0.0 => ((v * mult as f64).round() as u64).max(1),
        _ => DEFAULT,
    }
}

/// Bytes a minute of packed audio takes, at `spec`.
pub fn bytes_per_minute(spec: &str) -> f64 {
    bitrate_bps(spec) as f64 / 8.0 * 60.0
}

/// concat-demuxer quoting: close the quote, escape the quote, reopen.
fn concat_line(p: &Path) -> String {
    format!("file '{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Pack chapter `ci` into `chNNN.m4a` + `chNNN.json`.
///
/// Refuses a chapter with a hole: a missing chunk would silently shift every
/// start time after it, which is worse than no file at all.
pub fn build(
    cfg: &Config,
    key: &str,
    ci: usize,
    chunks: &[Chunk],
    chunk_dir: &Path,
    title: &str,
    book_title: &str,
) -> Result<Manifest, PackError> {
    let gone = cache::missing_chunks(chunk_dir, chunks.len());
    if !gone.is_empty() {
        return Err(PackError::Incomplete(ci, gone.len()));
    }
    if chunks.is_empty() {
        return Err(PackError::Empty(ci));
    }

    let paths: Vec<PathBuf> = (0..chunks.len())
        .map(|i| chunk_dir.join(format!("{i:05}.wav")))
        .collect();
    let (ch_n, rate, width, _) = cache::wav_info(&paths[0])?;

    let mut starts = Vec::with_capacity(paths.len());
    let mut t = 0.0f64;
    for (i, p) in paths.iter().enumerate() {
        let d = cache::wav_info(p)?.3;
        if i > 0 {
            t += if chunks[i].para != chunks[i - 1].para {
                cfg.chapter_para_gap_s
            } else {
                cfg.chapter_gap_s
            };
        }
        starts.push(round3(t));
        t += d;
    }

    let (m4a, jf) = chapter_files(&cfg.work, key, ci);
    if let Some(d) = m4a.parent() {
        std::fs::create_dir_all(d)?;
    }
    building_insert(key, ci);
    let t0 = std::time::Instant::now();
    let result = (|| -> Result<Manifest, PackError> {
        let tmp = tempfile::tempdir()?;
        let gap_wav = tmp.path().join("gap.wav");
        let para_wav = tmp.path().join("para.wav");
        cache::write_silence_wav(&gap_wav, cfg.chapter_gap_s, ch_n, rate, width)?;
        cache::write_silence_wav(&para_wav, cfg.chapter_para_gap_s, ch_n, rate, width)?;

        let mut lines = Vec::with_capacity(paths.len() * 2);
        for (i, p) in paths.iter().enumerate() {
            if i > 0 {
                lines.push(concat_line(if chunks[i].para != chunks[i - 1].para {
                    &para_wav
                } else {
                    &gap_wav
                }));
            }
            lines.push(concat_line(&p.canonicalize().unwrap_or_else(|_| p.clone())));
        }
        let list = tmp.path().join("list.txt");
        std::fs::write(&list, lines.join("\n") + "\n")?;

        // Encode to a temp name in the final directory and rename: a reader (or
        // the service worker) must never be handed a half-written file. `-f ipod`
        // is not optional — the `.part` suffix leaves ffmpeg with no extension to
        // guess the muxer from, and it exits rather than guess.
        let part = m4a.with_extension("m4a.part");
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
            .arg(&list)
            .args([
                "-c:a",
                "aac",
                "-b:a",
                &cfg.chapter_bitrate,
                "-ac",
                "1",
                "-movflags",
                "+faststart",
            ]);
        if !book_title.is_empty() {
            cmd.args(["-metadata", &format!("album={book_title}")]);
        }
        if !title.is_empty() {
            cmd.args(["-metadata", &format!("title={title}")]);
        }
        cmd.args(["-f", "ipod"]).arg(&part);
        let r = cmd.output()?;
        if !r.status.success() {
            let _ = std::fs::remove_file(&part);
            let err = String::from_utf8_lossy(&r.stderr);
            return Err(PackError::Ffmpeg(
                err.trim()
                    .lines()
                    .last()
                    .unwrap_or("ffmpeg failed")
                    .to_string(),
            ));
        }
        std::fs::rename(&part, &m4a)?;

        let manifest = Manifest {
            book: key.to_string(),
            chapter: ci,
            title: title.to_string(),
            chunks: chunks.len(),
            starts,
            duration: round3(t),
            gap: cfg.chapter_gap_s,
            para_gap: cfg.chapter_para_gap_s,
            bitrate: cfg.chapter_bitrate.clone(),
            sample_rate: rate,
            bytes: m4a.metadata()?.len(),
            built: crate::vault::now_iso_seconds(),
            build_s: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        };
        let tmpj = jf.with_extension("json.part");
        std::fs::write(&tmpj, serde_json::to_vec(&manifest).unwrap_or_default())?;
        std::fs::rename(&tmpj, &jf)?;
        Ok(manifest)
    })();
    building_remove(key, ci);
    result
}

/// Does a *trustworthy* m4a exist — one whose manifest matches this chunking?
/// A manifest built before a re-render points at the wrong words; that is not a
/// packed chapter, it is a trap.
pub fn chapter_packed(work: &Path, key: &str, ci: usize, n: usize) -> bool {
    let (m4a, _) = chapter_files(work, key, ci);
    m4a.exists() && read_manifest(work, key, ci).is_some_and(|m| m.chunks == n)
}

pub fn total_bytes(work: &Path) -> u64 {
    fn walk(d: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, acc);
                } else if p.extension().is_some_and(|x| x == "m4a") {
                    *acc += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    let mut acc = 0;
    walk(&work.join("chapters"), &mut acc);
    acc
}

/// Trim built chapters to `MAX_CHAPTER_GB`, oldest first. Never touches a
/// chapter that is building or in `keep` (the one being listened to). Evicting a
/// packed chapter takes its HLS segments with it — they are a derived copy of
/// exactly that file and have no reason to outlive it.
pub fn gc(cfg: &Config, keep: &HashSet<String>) -> u64 {
    if cfg.max_chapter_gb <= 0.0 {
        return total_bytes(&cfg.work);
    }
    let cap = (cfg.max_chapter_gb * 1024.0_f64.powi(3)) as u64;
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    fn walk(d: &Path, out: &mut Vec<(std::time::SystemTime, u64, PathBuf)>) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "m4a") {
                    if let Ok(md) = e.metadata() {
                        out.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), md.len(), p));
                    }
                }
            }
        }
    }
    walk(&cfg.work.join("chapters"), &mut files);
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= cap {
        return total;
    }
    let busy = building_tags();
    files.sort();
    let floor = (cap as f64 * 0.9) as u64;
    for (_, sz, p) in files {
        if total <= floor {
            break;
        }
        let Some(tag) = p
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|s| s.to_str())
            .zip(p.file_stem().and_then(|s| s.to_str()))
            .map(|(d, s)| format!("{d}/{s}"))
        else {
            continue;
        };
        if busy.contains(&tag) || keep.contains(&tag) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            let _ = std::fs::remove_file(p.with_extension("json"));
            if let Some((k, ci)) = tag.split_once("/ch") {
                if let Ok(ci) = ci.parse::<usize>() {
                    let _ = std::fs::remove_dir_all(hls_dir(&cfg.work, k, ci));
                }
            }
            total = total.saturating_sub(sz);
        }
    }
    total
}

// ------------------------------------------------------------------------ HLS

pub fn hls_dir(work: &Path, key: &str, ci: usize) -> PathBuf {
    work.join("hls").join(key).join(format!("ch{ci:03}"))
}

/// Segment a packed chapter for streaming. Idempotent and cheap: a stream copy
/// of an m4a that already exists.
///
/// Stream-copied, never re-encoded: an independently encoded AAC segment carries
/// ~2112 samples of encoder priming, which accumulates into seconds of drift
/// against the chunk manifest — and drift means voice notes and resume points
/// land on the wrong sentences.
pub fn build_hls(cfg: &Config, key: &str, ci: usize, base_url: &str) -> Result<PathBuf, PackError> {
    let (m4a, _) = chapter_files(&cfg.work, key, ci);
    if !m4a.exists() {
        return Err(PackError::Incomplete(ci, 0));
    }
    let d = hls_dir(&cfg.work, key, ci);
    let playlist = d.join("index.m3u8");
    if playlist.exists() {
        return Ok(playlist);
    }
    building_insert(key, ci);
    let result = (|| -> Result<PathBuf, PackError> {
        let tmp = d.with_file_name(format!(
            "{}.part",
            d.file_name().and_then(|s| s.to_str()).unwrap_or("ch")
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let r = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&m4a)
            .args([
                "-c:a",
                "copy",
                "-f",
                "hls",
                "-hls_time",
                &cfg.hls_segment_s.to_string(),
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_fmp4_init_filename",
                "init.mp4",
                "-hls_base_url",
                base_url,
                "-hls_segment_filename",
            ])
            .arg(tmp.join("seg%05d.m4s"))
            .arg(tmp.join("index.m3u8"))
            .output()?;
        if !r.status.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            let err = String::from_utf8_lossy(&r.stderr);
            return Err(PackError::Ffmpeg(format!(
                "hls: {}",
                err.trim().lines().last().unwrap_or("ffmpeg failed")
            )));
        }
        // ffmpeg applies -hls_base_url to the media segments but NOT to the init
        // segment's EXT-X-MAP, which it writes as a bare filename. A player would
        // resolve it against the playlist's own URL and 404 before it plays a
        // note. Fix the one line rather than let it.
        let pl = tmp.join("index.m3u8");
        let text = std::fs::read_to_string(&pl)?.replace(
            "#EXT-X-MAP:URI=\"init.mp4\"",
            &format!("#EXT-X-MAP:URI=\"{base_url}init.mp4\""),
        );
        std::fs::write(&pl, text)?;
        let _ = std::fs::remove_dir_all(&d);
        if let Some(p) = d.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(&tmp, &d)?;
        Ok(playlist)
    })();
    building_remove(key, ci);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn man(starts: Vec<f64>) -> Manifest {
        Manifest {
            book: "B".into(),
            chapter: 0,
            title: String::new(),
            chunks: starts.len(),
            duration: *starts.last().unwrap_or(&0.0),
            starts,
            gap: 0.3,
            para_gap: 0.6,
            bitrate: "64k".into(),
            sample_rate: 24000,
            bytes: 0,
            built: String::new(),
            build_s: 0.0,
        }
    }

    #[test]
    fn chunk_at_bisects_like_python() {
        let m = man(vec![0.0, 1.5, 3.0, 10.0]);
        assert_eq!(chunk_at(&m, 0.0), 0);
        assert_eq!(chunk_at(&m, 1.4), 0);
        assert_eq!(chunk_at(&m, 1.5), 1);
        assert_eq!(chunk_at(&m, 9.9), 2);
        assert_eq!(chunk_at(&m, 1e9), 3);
        assert_eq!(chunk_at(&m, -5.0), 0);
    }

    #[test]
    fn a_bitrate_is_read_the_way_ffmpeg_spells_it() {
        assert_eq!(bitrate_bps("64k"), 64_000);
        assert_eq!(bitrate_bps("128K"), 128_000);
        assert_eq!(bitrate_bps("96kbit"), 96_000);
        assert_eq!(bitrate_bps("48000"), 48_000);
        assert_eq!(bitrate_bps("1m"), 1_000_000);
        assert_eq!(bitrate_bps(" 64k "), 64_000);
        // Garbage, zero and negatives fall back rather than promising a
        // chapter that weighs nothing.
        assert_eq!(bitrate_bps("loud"), 64_000);
        assert_eq!(bitrate_bps("0"), 64_000);
        assert_eq!(bitrate_bps("-8k"), 64_000);
        assert_eq!(bytes_per_minute("64k"), 480_000.0);
    }

    #[test]
    fn concat_quoting_survives_an_apostrophe() {
        assert_eq!(
            concat_line(Path::new("/a/it's here.wav")),
            "file '/a/it'\\''s here.wav'"
        );
    }
}
