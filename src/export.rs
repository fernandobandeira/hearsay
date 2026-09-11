//! `narrator export` — the streaming cache, packed into one `.m4b` audiobook.
//!
//! A port of `app/export.py`, minus the half of it this server cannot produce.
//! The python script auto-detected two layouts: the batch renderer's one
//! finished `chapter_XXX.wav` per chapter, and the reader's per-chunk streaming
//! cache. There is no batch renderer here — every chunk this server writes lands
//! under `work/audio/<key>/chNNN/IIIII.wav` — so this reassembles the cache, with
//! the same inter-chunk (0.30 s) and paragraph (0.60 s) gaps the chapter packer
//! uses, and the same 1 s between chapters the export used.
//!
//! It is a **CLI path, not a server one**, exactly as it was in python: it is a
//! minutes-long ffmpeg run producing a file for a different device entirely
//! (Apple Books, a car), nobody is waiting on a response, and it must be usable
//! against a work directory whose server is not even running. What it needs from
//! the server is the thing the server already leaves behind: `plan.json`, which
//! carries the chapter titles, the chunk order and the paragraph boundaries the
//! gaps are derived from.
//!
//! Chapter marks come from the *real* wav durations rather than the estimates,
//! so a seek in a player lands where the chapter does.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::book::Chapter;
use crate::cache;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("no plan.json under {0} — open the book in the reader once, or render it")]
    NoPlan(PathBuf),
    #[error("plan.json in {0} will not parse")]
    BadPlan(PathBuf),
    #[error("no rendered audio under {0}")]
    NoAudio(PathBuf),
    #[error("{0} chapter(s) are not fully rendered ({1}) — finish the render, or pass --partial")]
    Incomplete(usize, String),
    #[error("nothing complete to pack under {0}")]
    NothingComplete(PathBuf),
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Usage(String),
}

/// What to pack, and how. The defaults are the python script's.
#[derive(Debug, Clone)]
pub struct ExportArgs {
    /// The EPUB, for the title, the author and the cover. Also what the cache
    /// directory is derived from when `dir` is not given.
    pub book: PathBuf,
    /// `work/audio/<key>` — override for a cache that was moved.
    pub dir: Option<PathBuf>,
    pub work: PathBuf,
    /// Where the `.m4b` lands. Defaults to `work/export`.
    pub out: Option<PathBuf>,
    pub bitrate: String,
    pub gap_s: f64,
    pub para_gap_s: f64,
    pub chapter_gap_s: f64,
    /// Pack whatever is complete instead of refusing a book with holes.
    pub partial: bool,
    pub title: Option<String>,
    pub author: Option<String>,
}

impl ExportArgs {
    pub fn new(book: PathBuf, work: PathBuf) -> Self {
        Self {
            book,
            dir: None,
            work,
            out: None,
            bitrate: "64k".into(),
            gap_s: 0.30,
            para_gap_s: 0.60,
            chapter_gap_s: 1.0,
            partial: false,
            title: None,
            author: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportReport {
    pub out: PathBuf,
    pub chapters: usize,
    /// The chapters left out, by name. Non-empty only with `--partial`.
    pub missing: Vec<String>,
    pub seconds: f64,
    pub bytes: u64,
    pub cover: bool,
}

/// One chapter's worth of input: its name, and the files that make it up in
/// order (chunks interleaved with the gap wavs).
#[derive(Debug)]
struct Packed {
    title: String,
    files: Vec<(PathBuf, f64)>,
}

impl Packed {
    fn seconds(&self) -> f64 {
        self.files.iter().map(|(_, s)| *s).sum()
    }
}

/// ffmetadata escaping: `=`, `;`, `#`, `\` and newlines are the delimiters.
fn ffmeta_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '=' | ';' | '#' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// concat-demuxer quoting: close the quote, escape the quote, reopen.
fn concat_line(p: &Path) -> String {
    format!("file '{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

/// A file name that survives every filesystem anyone will copy this onto.
fn safe_name(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '-',
            _ => c,
        })
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "audiobook".into()
    } else {
        s
    }
}

/// Title, author and cover, straight out of the EPUB. Best effort: a malformed
/// package yields Nones and a note, never a failed export — the audio is the
/// point and the metadata is the garnish.
fn epub_meta(path: &Path, tmp: &Path) -> (Option<String>, Option<String>, Option<PathBuf>) {
    let mut doc = match epub::doc::EpubDoc::new(path) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("could not read EPUB metadata: {e}");
            return (None, None, None);
        }
    };
    let title = doc.mdata("title").map(|m| m.value.trim().to_string());
    let author = doc.mdata("creator").map(|m| m.value.trim().to_string());
    let cover = doc.get_cover().and_then(|(data, mime)| {
        let ext = match mime.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            _ => "jpg",
        };
        let p = tmp.join(format!("cover.{ext}"));
        match std::fs::write(&p, &data) {
            Ok(()) => Some(p),
            Err(e) => {
                tracing::warn!("could not write the cover out: {e}");
                None
            }
        }
    });
    (
        title.filter(|s| !s.is_empty()),
        author.filter(|s| !s.is_empty()),
        cover,
    )
}

/// Reassemble every complete chapter out of the streaming cache.
///
/// A chapter with one missing chunk is **not** packable: the gap would swallow
/// the hole silently and every chapter mark after it would be wrong. So it goes
/// in `missing` and the caller decides (that is what `--partial` is).
fn collect(
    dir: &Path,
    plan: &[Chapter],
    gaps: &(PathBuf, f64, PathBuf, f64),
) -> (Vec<Packed>, Vec<String>) {
    let (gap, gap_s, para, para_s) = gaps;
    let mut have = Vec::new();
    let mut missing = Vec::new();
    for ch in plan {
        let cdir = dir.join(format!("ch{:03}", ch.index));
        let mut files: Vec<(PathBuf, f64)> = Vec::new();
        let mut prev: Option<usize> = None;
        let mut ok = true;
        for (i, k) in ch.chunks.iter().enumerate() {
            let p = cdir.join(format!("{i:05}.wav"));
            let Ok((_, _, _, seconds)) = cache::wav_info(&p) else {
                ok = false;
                break;
            };
            if let Some(pp) = prev {
                if k.para != pp {
                    files.push((para.clone(), *para_s));
                } else {
                    files.push((gap.clone(), *gap_s));
                }
            }
            files.push((p, seconds));
            prev = Some(k.para);
        }
        if ok && !files.is_empty() {
            have.push(Packed {
                title: ch.display_title(),
                files,
            });
        } else {
            missing.push(ch.display_title());
        }
    }
    (have, missing)
}

/// The plan as the server left it. Read raw rather than through
/// [`crate::plancache::load`]: the stamp is about whether a *parse* may be
/// reused, and an export of audio already on disk has no business refusing to
/// run because the epub's mtime moved.
fn read_plan(dir: &Path) -> Result<Vec<Chapter>, ExportError> {
    let p = dir.join("plan.json");
    let raw = std::fs::read(&p).map_err(|_| ExportError::NoPlan(dir.to_path_buf()))?;
    let plan: Vec<Chapter> =
        serde_json::from_slice(&raw).map_err(|_| ExportError::BadPlan(dir.to_path_buf()))?;
    if plan.is_empty() {
        return Err(ExportError::BadPlan(dir.to_path_buf()));
    }
    Ok(plan)
}

/// The first chunk wav in the cache — the format every generated silence has to
/// match, or the concat demuxer glitches at each gap.
fn first_wav(dir: &Path, plan: &[Chapter]) -> Option<PathBuf> {
    for ch in plan {
        let cdir = dir.join(format!("ch{:03}", ch.index));
        for i in 0..ch.chunks.len() {
            let p = cdir.join(format!("{i:05}.wav"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub fn run(args: &ExportArgs) -> Result<ExportReport, ExportError> {
    if !args.book.is_file() {
        return Err(ExportError::Usage(format!(
            "{} is not a file",
            args.book.display()
        )));
    }
    let key = cache::book_key(&args.book.to_string_lossy());
    let dir = args
        .dir
        .clone()
        .unwrap_or_else(|| cache::book_dir(&args.work, &key));
    let plan = read_plan(&dir)?;
    let reference = first_wav(&dir, &plan).ok_or_else(|| ExportError::NoAudio(dir.clone()))?;
    let (channels, rate, width, _) =
        cache::wav_info(&reference).map_err(|e| ExportError::Ffmpeg(e.to_string()))?;

    let tmp = tempfile::tempdir()?;
    let gap = tmp.path().join("gap.wav");
    let para = tmp.path().join("para.wav");
    let cgap = tmp.path().join("chapter.wav");
    cache::write_silence_wav(&gap, args.gap_s, channels, rate, width)
        .map_err(|e| ExportError::Ffmpeg(e.to_string()))?;
    cache::write_silence_wav(&para, args.para_gap_s, channels, rate, width)
        .map_err(|e| ExportError::Ffmpeg(e.to_string()))?;
    cache::write_silence_wav(&cgap, args.chapter_gap_s, channels, rate, width)
        .map_err(|e| ExportError::Ffmpeg(e.to_string()))?;

    let (have, missing) = collect(&dir, &plan, &(gap, args.gap_s, para, args.para_gap_s));
    if !missing.is_empty() && !args.partial {
        let names = missing
            .iter()
            .take(8)
            .map(|m| m.chars().take(40).collect::<String>())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ExportError::Incomplete(missing.len(), names));
    }
    if have.is_empty() {
        return Err(ExportError::NothingComplete(dir));
    }

    let (etitle, eauthor, cover) = epub_meta(&args.book, tmp.path());
    let title = args
        .title
        .clone()
        .or(etitle)
        .or_else(|| {
            args.book
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "audiobook".into());
    let author = args.author.clone().or(eauthor);

    // The concat list and the chapter marks, from the real durations.
    let mut lines: Vec<String> = Vec::new();
    let mut meta = vec![
        ";FFMETADATA1".to_string(),
        format!("title={}", ffmeta_escape(&title)),
        format!("album={}", ffmeta_escape(&title)),
        "genre=Audiobook".to_string(),
    ];
    if let Some(a) = &author {
        meta.push(format!("artist={}", ffmeta_escape(a)));
        meta.push(format!("album_artist={}", ffmeta_escape(a)));
    }
    let mut t = 0.0f64;
    let last = have.len().saturating_sub(1);
    for (gi, c) in have.iter().enumerate() {
        let d = c.seconds();
        meta.push("[CHAPTER]".into());
        meta.push("TIMEBASE=1/1000".into());
        meta.push(format!("START={}", (t * 1000.0) as i64));
        meta.push(format!("END={}", ((t + d) * 1000.0) as i64));
        meta.push(format!("title={}", ffmeta_escape(&c.title)));
        for (p, _) in &c.files {
            lines.push(concat_line(&p.canonicalize().unwrap_or_else(|_| p.clone())));
        }
        t += d;
        if gi < last {
            lines.push(concat_line(&cgap));
            t += args.chapter_gap_s;
        }
    }
    let list = tmp.path().join("list.txt");
    let metafile = tmp.path().join("meta.txt");
    std::fs::write(&list, lines.join("\n") + "\n")?;
    std::fs::write(&metafile, meta.join("\n") + "\n")?;

    let outdir = args.out.clone().unwrap_or_else(|| args.work.join("export"));
    std::fs::create_dir_all(&outdir)?;
    let out = outdir.join(format!("{}.m4b", safe_name(&title)));

    // Written to a `.part` in the destination and renamed, like every other file
    // this server produces: a killed export must not leave something that looks
    // like an audiobook. `-f ipod` is mandatory, because the part suffix leaves
    // ffmpeg no extension to guess a muxer from.
    let part = out.with_extension("m4b.part");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .arg("-i")
        .arg(&metafile);
    if let Some(c) = &cover {
        cmd.arg("-i").arg(c);
    }
    cmd.args(["-map", "0:a", "-map_metadata", "1"]);
    if cover.is_some() {
        // No -frames:v cap: a still image yields one frame anyway, and hitting a
        // video frame limit makes ffmpeg 8 stop before it encodes any audio.
        cmd.args([
            "-map",
            "2:v",
            "-c:v",
            "mjpeg",
            "-disposition:v:0",
            "attached_pic",
        ]);
    }
    cmd.args(["-c:a", "aac", "-b:a", &args.bitrate, "-ac", "1"])
        .args(["-movflags", "+faststart", "-f", "ipod"])
        .arg(&part);
    let r = cmd.output()?;
    if !r.status.success() {
        let _ = std::fs::remove_file(&part);
        let e = String::from_utf8_lossy(&r.stderr);
        return Err(ExportError::Ffmpeg(
            e.trim()
                .lines()
                .last()
                .unwrap_or("ffmpeg failed")
                .to_string(),
        ));
    }
    std::fs::rename(&part, &out)?;

    Ok(ExportReport {
        out: out.clone(),
        chapters: have.len(),
        missing,
        seconds: t,
        bytes: out.metadata().map(|m| m.len()).unwrap_or(0),
        cover: cover.is_some(),
    })
}

/// `narrator export …` — the argument parsing, kept here rather than in main so
/// it can be tested.
pub fn args_from(argv: &[String], work: PathBuf) -> Result<ExportArgs, ExportError> {
    let mut book: Option<PathBuf> = None;
    let mut a = ExportArgs::new(PathBuf::new(), work);
    let mut i = 0;
    let need = |v: Option<&String>, flag: &str| -> Result<String, ExportError> {
        v.cloned()
            .ok_or_else(|| ExportError::Usage(format!("{flag} needs a value")))
    };
    let numeric = |v: &str, flag: &str| -> Result<f64, ExportError> {
        v.parse()
            .map_err(|_| ExportError::Usage(format!("{flag} wants a number, not {v:?}")))
    };
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            // --epub is the python script's name for it; --book is this repo's.
            "--book" | "--epub" => {
                book = Some(PathBuf::from(need(argv.get(i + 1), arg)?));
                i += 1;
            }
            "--dir" => {
                a.dir = Some(PathBuf::from(need(argv.get(i + 1), arg)?));
                i += 1;
            }
            "--work" => {
                a.work = PathBuf::from(need(argv.get(i + 1), arg)?);
                i += 1;
            }
            "--out" => {
                a.out = Some(PathBuf::from(need(argv.get(i + 1), arg)?));
                i += 1;
            }
            "--bitrate" => {
                a.bitrate = need(argv.get(i + 1), arg)?;
                i += 1;
            }
            "--gap" => {
                a.gap_s = numeric(&need(argv.get(i + 1), arg)?, arg)?;
                i += 1;
            }
            "--para-gap" => {
                a.para_gap_s = numeric(&need(argv.get(i + 1), arg)?, arg)?;
                i += 1;
            }
            "--chapter-gap" => {
                a.chapter_gap_s = numeric(&need(argv.get(i + 1), arg)?, arg)?;
                i += 1;
            }
            "--title" => {
                a.title = Some(need(argv.get(i + 1), arg)?);
                i += 1;
            }
            "--author" => {
                a.author = Some(need(argv.get(i + 1), arg)?);
                i += 1;
            }
            "--partial" => a.partial = true,
            other => {
                return Err(ExportError::Usage(format!(
                    "unknown argument {other:?} — see `narrator export --help`"
                )))
            }
        }
        i += 1;
    }
    a.book = book.ok_or_else(|| {
        ExportError::Usage("which book? pass --book <path to the epub>".to_string())
    })?;
    Ok(a)
}

pub const USAGE: &str = "\
narrator export --book <file.epub> [options]

Pack the rendered chunks in the work directory into one .m4b audiobook: AAC,
chapter marks from the real durations, the EPUB's cover art embedded.

  --book <path>       the EPUB (metadata, cover, and which cache to read)
  --dir <path>        the cache directory, if it is not work/audio/<key>
  --work <path>       the work directory (default: $NARRATOR_WORK)
  --out <path>        where the .m4b lands (default: <work>/export)
  --bitrate <rate>    AAC bitrate (default: 64k)
  --partial           pack the complete chapters instead of refusing
  --gap <s>           silence between chunks (default: 0.30)
  --para-gap <s>      silence between paragraphs (default: 0.60)
  --chapter-gap <s>   silence between chapters (default: 1.0)
  --title <text>      override the EPUB title
  --author <text>     override the EPUB author
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Chunk;

    fn plan(chapters: &[(&str, usize)]) -> Vec<Chapter> {
        chapters
            .iter()
            .enumerate()
            .map(|(i, (title, n))| Chapter {
                index: i,
                id: format!("c{i}.xhtml"),
                title: (*title).into(),
                chunks: (0..*n)
                    .map(|k| Chunk {
                        text: format!("chunk {k}"),
                        // Every other chunk starts a paragraph, so both gap
                        // widths are exercised.
                        para: k / 2,
                        silent: false,
                    })
                    .collect(),
            })
            .collect()
    }

    /// A cache like the render worker's: `chNNN/IIIII.wav`, 0.2 s each.
    fn seed(dir: &Path, plan: &[Chapter], skip: &[(usize, usize)]) {
        for ch in plan {
            let d = dir.join(format!("ch{:03}", ch.index));
            std::fs::create_dir_all(&d).expect("chapter dir");
            for i in 0..ch.chunks.len() {
                if skip.contains(&(ch.index, i)) {
                    continue;
                }
                cache::write_silence_wav(&d.join(format!("{i:05}.wav")), 0.2, 1, 24_000, 2)
                    .expect("silence");
            }
        }
        std::fs::write(
            dir.join("plan.json"),
            serde_json::to_vec(plan).expect("plan"),
        )
        .expect("write plan");
    }

    #[test]
    fn metadata_and_paths_are_escaped_the_way_ffmpeg_reads_them() {
        assert_eq!(ffmeta_escape("A = B; #1"), "A \\= B\\; \\#1");
        assert_eq!(ffmeta_escape("back\\slash"), "back\\\\slash");
        assert_eq!(
            concat_line(Path::new("/a/it's here.wav")),
            "file '/a/it'\\''s here.wav'"
        );
        assert_eq!(safe_name("A/B: C?"), "A-B- C-");
        assert_eq!(safe_name("   "), "audiobook");
    }

    #[test]
    fn a_complete_chapter_is_gathered_with_both_gap_widths() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = plan(&[("One", 4)]);
        seed(d.path(), &p, &[]);
        let gaps = (
            d.path().join("gap.wav"),
            0.3,
            d.path().join("para.wav"),
            0.6,
        );
        let (have, missing) = collect(d.path(), &p, &gaps);
        assert!(missing.is_empty());
        assert_eq!(have.len(), 1);
        // 4 chunks, 3 joins: chunk-gap, para-gap, chunk-gap.
        assert_eq!(have[0].files.len(), 7);
        let gap_widths: Vec<f64> = have[0]
            .files
            .iter()
            .filter(|(p, _)| p.ends_with("gap.wav") || p.ends_with("para.wav"))
            .map(|(_, s)| *s)
            .collect();
        assert_eq!(gap_widths, vec![0.3, 0.6, 0.3]);
        assert!((have[0].seconds() - (4.0 * 0.2 + 0.3 + 0.6 + 0.3)).abs() < 1e-6);
    }

    #[test]
    fn a_chapter_with_a_hole_is_left_out_rather_than_shortened() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = plan(&[("One", 3), ("Two", 3)]);
        seed(d.path(), &p, &[(1, 1)]);
        let gaps = (
            d.path().join("gap.wav"),
            0.3,
            d.path().join("para.wav"),
            0.6,
        );
        let (have, missing) = collect(d.path(), &p, &gaps);
        assert_eq!(have.len(), 1);
        assert_eq!(missing, vec!["Two".to_string()]);
    }

    #[test]
    fn a_book_with_holes_is_refused_unless_partial_is_asked_for() {
        let d = tempfile::tempdir().expect("tempdir");
        let work = d.path().join("work");
        let key = cache::book_key(&fixture_epub().to_string_lossy());
        let cache_dir = cache::book_dir(&work, &key);
        std::fs::create_dir_all(&cache_dir).expect("dir");
        let p = plan(&[("One", 2), ("Two", 2)]);
        seed(&cache_dir, &p, &[(1, 0)]);

        let mut args = ExportArgs::new(fixture_epub(), work);
        match run(&args) {
            Err(ExportError::Incomplete(1, names)) => assert!(names.contains("Two")),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // With --partial it packs the one that is whole - if there is an ffmpeg.
        args.partial = true;
        if which_ffmpeg() {
            let r = run(&args).expect("export");
            assert_eq!(r.chapters, 1);
            assert_eq!(r.missing, vec!["Two".to_string()]);
            assert!(r.out.exists() && r.bytes > 0);
            assert_eq!(
                r.out.parent().map(Path::to_path_buf),
                Some(args.work.join("export"))
            );
        }
    }

    #[test]
    fn the_argument_list_is_the_python_scripts() {
        let work = PathBuf::from("/w");
        let a = args_from(
            &[
                "--epub".into(),
                "/b/x.epub".into(),
                "--partial".into(),
                "--bitrate".into(),
                "96k".into(),
                "--chapter-gap".into(),
                "2.5".into(),
                "--title".into(),
                "Renamed".into(),
            ],
            work.clone(),
        )
        .expect("args");
        assert_eq!(a.book, PathBuf::from("/b/x.epub"));
        assert!(a.partial);
        assert_eq!(a.bitrate, "96k");
        assert_eq!(a.chapter_gap_s, 2.5);
        assert_eq!(a.title.as_deref(), Some("Renamed"));
        assert_eq!(a.work, work);

        // And every way of getting it wrong is a message, not a panic.
        assert!(args_from(&[], PathBuf::from("/w")).is_err());
        assert!(args_from(&["--book".into()], PathBuf::from("/w")).is_err());
        assert!(args_from(
            &["--book".into(), "b".into(), "--gap".into(), "soon".into()],
            PathBuf::from("/w")
        )
        .is_err());
        assert!(args_from(&["--wat".into()], PathBuf::from("/w")).is_err());
    }

    fn fixture_epub() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture.epub")
    }

    fn which_ffmpeg() -> bool {
        std::env::var("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("ffmpeg").is_file()))
            .unwrap_or(false)
    }
}
