//! Text -> Kokoro phoneme string.
//!
//! Kokoro's own grapheme-to-phoneme front end is `misaki`, which looks words up
//! in a lexicon and falls back to espeak-ng for anything it does not know. This
//! is the fallback half, applied to every token: espeak-ng in IPA mode, then
//! misaki's `EspeakFallback` character mapping, which is what turns espeak's IPA
//! into the alphabet Kokoro's tokenizer actually has symbols for.
//!
//! espeak-ng runs as a *subprocess*, not through its C library, on purpose:
//! libespeak-ng keeps process-global state, is not thread-safe, and a wedge or a
//! segfault inside it would take the whole server with it. A forked process is a
//! few milliseconds against a chunk that takes a second or two to synthesize, and
//! it can be killed on a timeout.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::err::TtsError;

/// The tie character espeak-ng writes with `--ipa=2`, and which misaki's mapping
/// table expects to see as `^`.
const TIE: char = '\u{0361}';
/// Combining vertical line below: espeak's syllabic consonant mark.
const SYLLABIC: char = '\u{0329}';
/// Combining tilde: nasalisation, which Kokoro has no symbol for.
const TILDE: char = '\u{0303}';

/// Punctuation phonemizer preserves around the phonemized runs. These all have
/// entries in Kokoro's vocabulary, so they survive into the token stream and are
/// what gives a sentence its prosody.
const MARKS: &[char] = &[
    ';', ':', ',', '.', '!', '?', '—', '…', '"', '(', ')', '\u{201C}', '\u{201D}',
];

/// misaki `EspeakFallback.E2M`, longest key first (the table is applied in that
/// order there too, via `sorted(key=lambda kv: -len(kv[0]))`).
const E2M: &[(&str, &str)] = &[
    ("\u{294}\u{32C}n\u{329}", "\u{294}n"), // 'ʔˌn̩'
    ("\u{294}n\u{329}", "\u{294}n"),
    ("\u{2B2}o", "jo"),
    ("\u{2B2}\u{259}", "j\u{259}"),
    ("a^\u{26A}", "I"),
    ("a^\u{28A}", "W"),
    ("d^\u{292}", "\u{2A4}"),
    ("e^\u{26A}", "A"),
    ("t^\u{283}", "\u{2A7}"),
    ("\u{254}^\u{26A}", "Y"),
    ("\u{259}^l", "\u{1D4A}l"),
    ("\u{2B2}", ""),
    ("\u{25A}", "\u{259}\u{279}"),
    ("e", "A"),
    ("r", "\u{279}"),
    ("x", "k"),
    ("\u{E7}", "k"),
    ("\u{250}", "\u{259}"),
    ("\u{26C}", "l"),
];

/// Where the phonemizer lives and which voice it speaks.
#[derive(Debug, Clone)]
pub struct Phonemizer {
    binary: String,
    voice: String,
    timeout: Duration,
}

/// How long espeak-ng gets before it is killed. Generous on purpose: a short
/// phrase is milliseconds even on the A1, so anything near this is a wedge
/// rather than a slow box, and the cost of being wrong in this direction is a
/// chunk that renders late instead of one that never renders at all.
const DEFAULT_TIMEOUT_S: f64 = 15.0;

impl Default for Phonemizer {
    fn default() -> Self {
        let secs = std::env::var("ESPEAK_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|s| *s > 0.0)
            .unwrap_or(DEFAULT_TIMEOUT_S);
        Self {
            binary: std::env::var("ESPEAK_BIN").unwrap_or_else(|_| "espeak-ng".into()),
            voice: std::env::var("ESPEAK_VOICE").unwrap_or_else(|_| "en-us".into()),
            timeout: Duration::from_secs_f64(secs),
        }
    }
}

impl Phonemizer {
    pub fn new(voice: impl Into<String>) -> Self {
        Self {
            voice: voice.into(),
            ..Self::default()
        }
    }

    /// Is espeak-ng actually callable? Checked once at startup so a missing
    /// binary is a log line and a degraded engine, not a panic per chunk.
    ///
    /// Timed out like every other call: `probe` runs inside `Engine::load`, which
    /// the render thread itself calls, so a binary that hangs on `--version`
    /// would wedge the worker before it ever rendered a chunk.
    pub fn probe(&self) -> Result<String, TtsError> {
        let out = self.run(&["--version"])?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Phonemize one chunk of text, preserving the punctuation between runs.
    pub fn phonemize(&self, text: &str) -> Result<String, TtsError> {
        let mut out = String::new();
        for seg in split_punctuation(text) {
            match seg {
                Segment::Mark(c) => out.push(c),
                Segment::Words(w) => {
                    let ipa = self.espeak(w)?;
                    out.push_str(&map_ipa(&ipa));
                }
            }
        }
        Ok(collapse_spaces(&out))
    }

    fn espeak(&self, text: &str) -> Result<String, TtsError> {
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        let out = self.run(&["-q", "--ipa=2", "-v", &self.voice, "--", text])?;
        if !out.status.success() {
            return Err(TtsError::Espeak(format!(
                "espeak-ng exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // espeak breaks its output into lines at clause boundaries; they are one
        // continuous utterance as far as Kokoro is concerned.
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" "))
    }

    /// Run the binary with a deadline, and kill it if it blows through one.
    ///
    /// This is the whole reason espeak-ng is a subprocess. `Command::output()`
    /// waits forever, so a wedged espeak — and it is a C program with a history
    /// of them — takes the render thread with it and the reader waits on a chunk
    /// nobody is rendering any more. Here it costs one dead subprocess and a
    /// typed error that `render_one` logs and steps over.
    ///
    /// The pipes are drained on their own threads rather than read after the
    /// wait: a child that fills the 64 KB pipe buffer while we are polling for
    /// its exit would deadlock against us, which would be the same hang by
    /// another route.
    fn run(&self, args: &[&str]) -> Result<Output, TtsError> {
        let spawn_err = |e: std::io::Error| TtsError::Espeak(format!("spawn {}: {e}", self.binary));
        let mut child = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_err)?;

        let pipes = drain(child.stdout.take()).and_then(|o| Ok((o, drain(child.stderr.take())?)));
        let (out_t, err_t) = match pipes {
            Ok(p) => p,
            // No reader thread means no way to drain the pipes, so waiting for
            // this child is exactly the hang this function exists to prevent.
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(TtsError::Espeak(format!(
                    "could not read from {}: {e}",
                    self.binary
                )));
            }
        };

        let Some(status) = wait_deadline(&mut child, self.timeout) else {
            // Wedged, or not reapable. Either way it is not going to answer:
            // kill it, reap it, and let the caller log and degrade. The reader
            // threads see EOF on the closed pipes and finish on their own.
            let _ = child.kill();
            let _ = child.wait();
            return Err(TtsError::Espeak(format!(
                "{} did not finish within {:.1}s; killed",
                self.binary,
                self.timeout.as_secs_f64()
            )));
        };
        let joined = |t: Option<Drained>| t.and_then(|t| t.join().ok()).unwrap_or_default();
        Ok(Output {
            status,
            stdout: joined(out_t),
            stderr: joined(err_t),
        })
    }

    /// The timeout is part of the struct so callers can shorten it in tests.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Shorten (or lengthen) the deadline. Tests use it to make a wedge cheap.
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }
}

/// Read one of the child's pipes to EOF on its own thread.
type Drained = std::thread::JoinHandle<Vec<u8>>;

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::io::Result<Option<Drained>> {
    match pipe {
        None => Ok(None),
        Some(mut p) => std::thread::Builder::new()
            .name("espeak-pipe".into())
            .spawn(move || {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf);
                buf
            })
            .map(Some),
    }
}

/// What `Command::output()` would have returned, minus the unbounded wait.
struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Wait for `child` for at most `timeout`. `None` means it is still running (or
/// cannot be asked), and the caller kills it.
fn wait_deadline(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let t0 = Instant::now();
    // Polling rather than a blocking wait on another thread, because the thread
    // that blocks is the one that would have to be abandoned. It starts tight —
    // espeak answers a short phrase in single-digit milliseconds and the g2p
    // cost is measured at 0.9% of a render — and backs off to 10 ms for the
    // pathological case nobody is waiting on anyway.
    let mut nap = Duration::from_micros(250);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {}
            Err(_) => return None,
        }
        if t0.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(10));
    }
}

enum Segment<'a> {
    Words(&'a str),
    Mark(char),
}

/// Split on punctuation, keeping it. phonemizer's `preserve_punctuation` does the
/// same thing: phonemize the words, put the marks back where they were.
fn split_punctuation(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        if MARKS.contains(&c) {
            if i > start {
                out.push(Segment::Words(&text[start..i]));
            }
            out.push(Segment::Mark(c));
            start = i + c.len_utf8();
        }
    }
    if start < text.len() {
        out.push(Segment::Words(&text[start..]));
    }
    out
}

/// misaki's `EspeakFallback.__call__`, minus the phonemizer plumbing.
fn map_ipa(ipa: &str) -> String {
    let mut s: String = ipa.replace(TIE, "^").replace(TILDE, "");
    for (from, to) in E2M {
        if s.contains(from) {
            s = s.replace(from, to);
        }
    }
    // `re.sub(r'(\S)̩', r'ᵊ\1', ps)`: a syllabic consonant becomes a schwa
    // plus the consonant, which is what Kokoro's alphabet can say.
    s = syllabic(&s);
    // American English post-processing.
    s = s.replace("o^\u{28A}", "O");
    s = s.replace("\u{25C}\u{2D0}\u{279}", "\u{25C}\u{279}");
    s = s.replace("\u{25C}\u{2D0}", "\u{25C}\u{279}");
    s = s.replace("\u{26A}\u{259}", "i\u{259}");
    s = s.replace('\u{2D0}', "");
    // "for espeak < 1.52" in misaki, applied unconditionally there.
    s = s.replace('o', "\u{254}");
    // misaki only keeps ɾ and ʔ for its own version "2.0"; kokoro passes None.
    s = s.replace('\u{27E}', "T").replace('\u{294}', "t");
    s.replace('^', "")
}

fn syllabic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if c == SYLLABIC {
            match prev.take() {
                // `(\S)` — a non-space before the mark.
                Some(p) if !p.is_whitespace() => {
                    out.push('\u{1D4A}');
                    out.push(p);
                }
                Some(p) => out.push(p),
                None => {}
            }
            continue;
        }
        if let Some(p) = prev.replace(c) {
            out.push(p);
        }
    }
    if let Some(p) = prev {
        out.push(p);
    }
    out
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c == ' ' {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punctuation_is_preserved_in_place() {
        let segs = split_punctuation("a, b. c");
        let rendered: String = segs
            .iter()
            .map(|s| match s {
                Segment::Words(w) => (*w).to_string(),
                Segment::Mark(c) => c.to_string(),
            })
            .collect();
        assert_eq!(rendered, "a, b. c");
    }

    #[test]
    fn syllabic_mark_becomes_schwa_plus_consonant() {
        assert_eq!(syllabic("bʌtn\u{329}"), "bʌtᵊn");
    }

    /// A stand-in for espeak-ng that never answers. `/bin/sh` is the only thing
    /// this needs, so the test runs on a box with no espeak-ng installed — which
    /// is every box the suite runs on.
    #[cfg(unix)]
    fn wedged_binary(dir: &std::path::Path) -> Option<String> {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("espeak-wedged");
        std::fs::write(&p, "#!/bin/sh\nsleep 300\n").ok()?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).ok()?;
        Some(p.to_string_lossy().to_string())
    }

    #[cfg(unix)]
    #[test]
    fn a_wedged_espeak_is_killed_rather_than_waited_on_forever() {
        let d = tempfile::tempdir().expect("tempdir");
        let Some(bin) = wedged_binary(d.path()) else {
            eprintln!("skipping: could not write the stub");
            return;
        };
        let g = Phonemizer {
            binary: bin,
            voice: "en-us".into(),
            timeout: Duration::from_millis(150),
        };
        let t0 = Instant::now();
        let e = g.phonemize("hello there").expect_err("must not succeed");
        let waited = t0.elapsed();
        assert!(
            matches!(e, TtsError::Espeak(ref m) if m.contains("killed")),
            "{e}"
        );
        // The point of the whole exercise: the render thread comes back.
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_is_bounded_too() {
        // `probe` runs inside `Engine::load`, on the render thread.
        let d = tempfile::tempdir().expect("tempdir");
        let Some(bin) = wedged_binary(d.path()) else {
            return;
        };
        let g = Phonemizer {
            binary: bin,
            voice: "en-us".into(),
            timeout: Duration::from_millis(150),
        };
        let t0 = Instant::now();
        assert!(g.probe().is_err());
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_binary_that_is_not_there_is_an_error_not_a_hang() {
        let g = Phonemizer {
            binary: "/nonexistent/espeak-ng".into(),
            voice: "en-us".into(),
            timeout: Duration::from_millis(50),
        };
        assert!(g.probe().is_err());
        assert!(g.phonemize("hello").is_err());
        // Empty text never forks at all.
        assert_eq!(g.phonemize(",").ok(), Some(",".to_string()));
    }

    /// Only where espeak-ng is actually installed — the suite must run without
    /// it. Where it is, this is the check that draining the pipes on threads
    /// gives the same bytes `Command::output()` used to.
    #[test]
    fn a_real_espeak_still_phonemizes() {
        let g = Phonemizer::new("en-us");
        if g.probe().is_err() {
            eprintln!("skipping: no espeak-ng");
            return;
        }
        let ps = g.phonemize("hello there, judge.").expect("phonemize");
        assert!(ps.contains(','), "{ps}");
        assert!(ps.ends_with('.'), "{ps}");
        assert!(ps.len() > 8, "{ps}");
        assert_eq!(ps, g.phonemize("hello there, judge.").expect("again"));
    }

    #[test]
    fn the_timeout_is_readable_and_adjustable() {
        let g = Phonemizer::new("en-us");
        assert_eq!(g.timeout(), Duration::from_secs(15));
        assert_eq!(
            g.with_timeout(Duration::from_secs(2)).timeout(),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn ties_and_diphthongs_map_to_kokoro_symbols() {
        // "boy" -> bˈɔ͡ɪ -> bˈY
        assert_eq!(map_ipa("b\u{2C8}\u{254}\u{361}\u{26A}"), "b\u{2C8}Y");
        // "judge" -> d͡ʒˈʌd͡ʒ -> ʤˈʌʤ
        assert_eq!(
            map_ipa("d\u{361}\u{292}\u{2C8}\u{28C}d\u{361}\u{292}"),
            "\u{2A4}\u{2C8}\u{28C}\u{2A4}"
        );
    }
}
