//! The chunk audio cache: `work/audio/<key>/chNNN/IIIII.wav`, 24 kHz mono s16le.
//!
//! Layout parity with the python server is not cosmetic — a Rust deploy has to
//! adopt the cache that is already on the VPS in place, so the directory names,
//! the zero padding and the wav format are all fixed points.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::err::PackError;

pub const SR: u32 = 24_000;

/// `Path(S["book"]).stem[:50]` — the cache directory name for a book.
pub fn book_key(book_path: &str) -> String {
    Path::new(book_path)
        .file_stem()
        .map(|s| s.to_string_lossy().chars().take(50).collect())
        .unwrap_or_else(|| "x".to_string())
}

/// `safe_key`: a `?book=` value names one cache directory, never a path.
/// Anything with a separator in it is a traversal attempt, not a book.
pub fn safe_key(k: &str) -> String {
    let k = k.replace('\\', "/");
    let k = k.rsplit('/').next().unwrap_or("").trim();
    if k.is_empty() || k == "." || k == ".." {
        String::new()
    } else {
        k.chars().take(50).collect()
    }
}

pub fn audio_root(work: &Path) -> PathBuf {
    work.join("audio")
}

pub fn book_dir(work: &Path, key: &str) -> PathBuf {
    audio_root(work).join(key)
}

pub fn chapter_dir(work: &Path, key: &str, ci: usize) -> PathBuf {
    book_dir(work, key).join(format!("ch{ci:03}"))
}

pub fn chunk_path(work: &Path, key: &str, ci: usize, i: usize) -> PathBuf {
    chapter_dir(work, key, ci).join(format!("{i:05}.wav"))
}

pub fn plan_path(work: &Path, key: &str) -> PathBuf {
    book_dir(work, key).join("plan.json")
}

// ----------------------------------------------------------------------- wav

/// `(channels, sample_rate, sample_width_bytes, seconds)` from the RIFF header.
/// Forking ffprobe per file is what makes scanning a few thousand cache chunks
/// slow, and the header already has the answer.
pub fn wav_info(path: &Path) -> Result<(u16, u32, u16, f64), PackError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let mut head = [0u8; 12];
    f.read_exact(&mut head)
        .map_err(|_| PackError::NotWav(path.display().to_string()))?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err(PackError::NotWav(path.display().to_string()));
    }
    let (mut ch, mut rate, mut bits, mut byte_rate) = (0u16, 0u32, 0u16, 0u32);
    loop {
        let mut hdr = [0u8; 8];
        if f.read(&mut hdr)? < 8 {
            break;
        }
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        match &hdr[0..4] {
            b"fmt " => {
                let mut fmt = vec![0u8; size as usize];
                f.read_exact(&mut fmt)
                    .map_err(|_| PackError::NotWav(path.display().to_string()))?;
                if fmt.len() < 16 {
                    return Err(PackError::NotWav(path.display().to_string()));
                }
                ch = u16::from_le_bytes([fmt[2], fmt[3]]);
                rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                byte_rate = u32::from_le_bytes([fmt[8], fmt[9], fmt[10], fmt[11]]);
                bits = u16::from_le_bytes([fmt[14], fmt[15]]);
                if size & 1 == 1 {
                    f.seek(SeekFrom::Current(1))?;
                }
            }
            b"data" if byte_rate != 0 => {
                return Ok((ch, rate, bits / 8, size as f64 / byte_rate as f64));
            }
            _ => {
                f.seek(SeekFrom::Current((size + (size & 1)) as i64))?;
            }
        }
    }
    Err(PackError::NotWav(path.display().to_string()))
}

/// Write one chunk: 24 kHz mono s16le, the format `soundfile.write` produces for
/// a float array at that rate. Written to a `.part` and renamed, so the renderer
/// can never hand the reader (or the packer) a half-written file — the python
/// server writes in place and a killed container leaves a truncated wav behind.
pub fn write_wav(path: &Path, samples: &[f32]) -> Result<(), PackError> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let part = path.with_extension("wav.part");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    {
        let mut w = hound::WavWriter::create(&part, spec)
            .map_err(|e| PackError::Ffmpeg(format!("wav: {e}")))?;
        for s in crate::tts::kokoro::to_i16(samples) {
            w.write_sample(s)
                .map_err(|e| PackError::Ffmpeg(format!("wav: {e}")))?;
        }
        w.finalize()
            .map_err(|e| PackError::Ffmpeg(format!("wav: {e}")))?;
    }
    // The rename is only an atom if what it names is already on the disk. Without
    // this, a power cut or a hard reset can land the rename and not the data —
    // which leaves a zero-length or truncated `IIIII.wav` under the real name,
    // the exact file the `.part` exists to prevent, and one that reads as
    // rendered forever after. `finalize` has flushed hound's buffer into the
    // kernel; this is what gets it out of the kernel. One fsync per chunk is
    // nothing against the second or more it took to synthesize.
    std::fs::File::open(&part)?.sync_all()?;
    std::fs::rename(&part, path)?;
    Ok(())
}

/// Silence of exactly `seconds`, matching the chunks' format. The concat demuxer
/// copies streams through without resampling, so a mismatched silence glitches
/// the join.
pub fn write_silence_wav(
    path: &Path,
    seconds: f64,
    channels: u16,
    rate: u32,
    width: u16,
) -> Result<(), PackError> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: width * 8,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w =
        hound::WavWriter::create(path, spec).map_err(|e| PackError::Ffmpeg(format!("wav: {e}")))?;
    let frames = (seconds * rate as f64) as usize;
    for _ in 0..frames * channels as usize {
        w.write_sample(0i16)
            .map_err(|e| PackError::Ffmpeg(format!("wav: {e}")))?;
    }
    w.finalize()
        .map_err(|e| PackError::Ffmpeg(format!("wav: {e}")))?;
    Ok(())
}

// ------------------------------------------------------------------------ gc

fn walk_wavs(root: &Path, out: &mut Vec<(std::time::SystemTime, u64, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_wavs(&p, out),
            Ok(_) if p.extension().is_some_and(|x| x == "wav") => {
                if let Ok(md) = e.metadata() {
                    out.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), md.len(), p));
                }
            }
            _ => {}
        }
    }
}

/// Total bytes of `*.wav` under `work/audio` — and nothing else. `MAX_AUDIO_GB`
/// covers this cache only; packed chapters have their own cap.
pub fn audio_bytes(work: &Path) -> u64 {
    let mut files = Vec::new();
    walk_wavs(&audio_root(work), &mut files);
    files.iter().map(|f| f.1).sum()
}

/// Delete oldest rendered chunks once total audio exceeds `max_gb`, trimming to
/// 90% to avoid thrashing.
///
/// `keep` is the set of chapter directories that must survive: the chapter being
/// read and its prerender span, plus everything the chapter manager is working
/// on. Chunk wavs are the *input* to a pack — evicting them out from under a
/// queued build leaves a chapter permanently one hole short of packable, and
/// nothing recovers from that except re-rendering it.
pub fn gc_audio(work: &Path, max_gb: f64, keep: &HashSet<PathBuf>) -> u64 {
    let root = audio_root(work);
    if !root.is_dir() {
        return 0;
    }
    let mut files = Vec::new();
    walk_wavs(&root, &mut files);
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let cap = (max_gb * 1024.0_f64.powi(3)) as u64;
    if total <= cap {
        return total;
    }
    files.sort(); // oldest first
    let floor = (cap as f64 * 0.9) as u64;
    for (_, sz, p) in files {
        if total <= floor {
            break;
        }
        if p.parent().is_some_and(|d| keep.contains(d)) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            total = total.saturating_sub(sz);
        }
    }
    total
}

/// Indices of chunks not yet rendered — what stands between here and a build.
pub fn missing_chunks(chunk_dir: &Path, n: usize) -> Vec<usize> {
    let mut have = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(chunk_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "wav") {
                if let Some(i) = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    have.insert(i);
                }
            }
        }
    }
    (0..n).filter(|i| !have.contains(i)).collect()
}

/// How many of a chapter's first `n` chunks are on disk.
pub fn rendered_count(chunk_dir: &Path, n: usize) -> usize {
    let mut have = 0;
    if let Ok(rd) = std::fs::read_dir(chunk_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "wav") {
                if let Some(i) = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    if i < n {
                        have += 1;
                    }
                }
            }
        }
    }
    have
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stems_truncated_to_fifty() {
        assert_eq!(
            book_key("/books/The Mom Test (2013).epub"),
            "The Mom Test (2013)"
        );
        let long = format!("/books/{}.epub", "x".repeat(80));
        assert_eq!(book_key(&long).len(), 50);
    }

    #[test]
    fn safe_key_refuses_traversal() {
        assert_eq!(safe_key("../../etc/passwd"), "passwd");
        assert_eq!(safe_key(".."), "");
        assert_eq!(safe_key(""), "");
        assert_eq!(safe_key("a/b"), "b");
        assert_eq!(safe_key("Book (2013)"), "Book (2013)");
    }

    #[test]
    fn paths_match_the_python_layout() {
        let w = Path::new("/work");
        assert_eq!(
            chunk_path(w, "Book", 7, 42),
            Path::new("/work/audio/Book/ch007/00042.wav")
        );
        assert_eq!(
            chapter_dir(w, "Book", 1433),
            Path::new("/work/audio/Book/ch1433")
        );
    }

    #[test]
    fn wav_roundtrip_reports_its_own_duration() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("a.wav");
        write_wav(&p, &vec![0.0f32; SR as usize]).expect("write");
        let (ch, rate, width, secs) = wav_info(&p).expect("info");
        assert_eq!((ch, rate, width), (1, SR, 2));
        assert!((secs - 1.0).abs() < 1e-9, "{secs}");
    }

    #[test]
    fn gc_never_evicts_a_kept_chapter() {
        let d = tempfile::tempdir().expect("tempdir");
        let w = d.path();
        for ci in 0..3usize {
            for i in 0..4usize {
                write_wav(&chunk_path(w, "B", ci, i), &vec![0.0f32; SR as usize]).expect("write");
            }
        }
        let keep: HashSet<_> = [chapter_dir(w, "B", 1)].into_iter().collect();
        // A cap far below what is on disk: everything evictable must go.
        gc_audio(w, 0.0000001, &keep);
        assert_eq!(missing_chunks(&chapter_dir(w, "B", 1), 4).len(), 0);
        assert_eq!(missing_chunks(&chapter_dir(w, "B", 0), 4).len(), 4);
    }
}
