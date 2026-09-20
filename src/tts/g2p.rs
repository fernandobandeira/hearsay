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
    // 'ʔˌn̩' — the mark between is U+02CC, espeak-ng's secondary stress, not
    // U+032C, a combining caron below. With the wrong one this never matched
    // and a glottalised syllabic n kept a stray schwa.
    ("\u{294}\u{2CC}n\u{329}", "\u{294}n"),
    ("\u{294}n\u{329}", "\u{294}n"),
    ("a^\u{26A}", "I"),
    ("a^\u{28A}", "W"),
    ("d^\u{292}", "\u{2A4}"),
    ("e^\u{26A}", "A"),
    ("t^\u{283}", "\u{2A7}"),
    ("\u{254}^\u{26A}", "Y"),
    ("\u{259}^l", "\u{1D4A}l"),
    ("\u{2B2}o", "jo"),
    ("\u{2B2}\u{259}", "j\u{259}"),
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
        // Numbers first: espeak-ng reads currency and years wrong, and the
        // rewrite is in words, so everything after this is ordinary text.
        let text = crate::tts::numbers::normalize(text);
        let mut out = String::new();
        for seg in split_punctuation(&text) {
            match seg {
                Segment::Mark(c) => out.push(c),
                // The space either side of a run of words is a word boundary
                // and Kokoro has a symbol for it, but espeak-ng does not put
                // one back — so `begin, he said` came out as `bɪɡˈɪn,hi sˈɛd`
                // with the two words run together across the comma.
                Segment::Words(w) => {
                    if w.starts_with(char::is_whitespace) {
                        out.push(' ');
                    }
                    let ipa = self.espeak(w)?;
                    out.push_str(&map_ipa(&ipa));
                    if w.ends_with(char::is_whitespace) {
                        out.push(' ');
                    }
                }
            }
        }
        Ok(collapse_spaces(&out))
    }

    fn espeak(&self, text: &str) -> Result<String, TtsError> {
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        let text = apply_overrides(text);
        let out = self.run(&["-q", "--ipa=2", "-v", &self.voice, "--", &text])?;
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

/// The override table: words espeak-ng gets wrong often enough to be worth
/// saying outright, spelled in espeak-ng's own phoneme alphabet.
///
/// This is the small version of the lexicon this front end does not have.
/// Kokoro's real G2P looks a word up in misaki's gold lexicon first and only
/// falls back to espeak-ng; here everything goes to espeak-ng, and for `I` that
/// is audibly wrong. espeak-ng applies sentence prosody and de-stresses a
/// subject pronoun — `I think it is fine` phonemizes to `a͡ɪ θˈɪŋk …` with no
/// stress mark at all — where misaki's lexicon says `ˈI` unconditionally.
/// Kokoro renders the difference as 60 ms at half the amplitude of the word
/// after it, against 120 ms at full amplitude, which is why it sounds like the
/// `I` was skipped at the start of a sentence.
///
/// The substitution is espeak-ng's own `[[…]]` escape rather than a separate
/// call per word, which matters: it forces the pronunciation of exactly one
/// word and leaves the rest of the sentence in one utterance, so every
/// neighbour keeps the stress espeak-ng chose for it. Phonemizing the word on
/// its own instead would make each fragment a fresh utterance and give the
/// words around it citation stress.
///
/// The escape is the one thing here that depends on espeak-ng's own syntax, so
/// it is checked against the binary rather than assumed: verified on 1.52
/// (this desktop) and on **1.51**, which is what the bookworm-slim runtime
/// carries and what the box actually runs.
///
/// Grow it a word at a time, with a test, when one is actually reported wrong.
const OVERRIDES: &[(&str, &str)] = &[("I", "'aI")];

/// Replace whole words in the override table with their `[[…]]` escapes.
///
/// A word here is a run of alphanumerics *and apostrophes*, so `I'm` is one
/// word and does not match `I` — espeak-ng reads `[['aI]]'m` as "I em".
fn apply_overrides(text: &str) -> std::borrow::Cow<'_, str> {
    let mut out: Option<String> = None;
    let mut last = 0usize;
    for (start, end) in word_spans(text) {
        let Some((_, ps)) = OVERRIDES.iter().find(|(w, _)| *w == &text[start..end]) else {
            continue;
        };
        let o = out.get_or_insert_with(|| String::with_capacity(text.len() + 8));
        o.push_str(&text[last..start]);
        o.push_str("[[");
        o.push_str(ps);
        o.push_str("]]");
        last = end;
    }
    match out {
        None => std::borrow::Cow::Borrowed(text),
        Some(mut o) => {
            o.push_str(&text[last..]);
            std::borrow::Cow::Owned(o)
        }
    }
}

fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '\'' || c == '\u{2019}';
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (is_word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                spans.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
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
        if MARKS.contains(&c) && !inside_a_number(text, i, c) {
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

/// Is this `,`, `.` or `:` part of a number rather than punctuation?
///
/// Splitting there is what turned `1,000` into "one, zero zero zero", `1.8`
/// into "one. eight" and `3:45` into "three: forty-five": each half went to
/// espeak-ng as its own utterance with a spoken pause between them, and a
/// bare `000` is three zeroes. espeak-ng reads all three correctly when it is
/// handed the whole number, so the fix is to stop taking them apart — a
/// separator is only punctuation when it is *not* between two digits.
///
/// Deliberately strict about "between": the digits have to be immediately
/// either side, so a sentence ending in a year keeps its full stop and
/// `page 12, line 3` keeps its comma.
fn inside_a_number(text: &str, i: usize, c: char) -> bool {
    if !matches!(c, ',' | '.' | ':') {
        return false;
    }
    let before = text[..i].chars().next_back();
    let after = text[i + c.len_utf8()..].chars().next();
    matches!((before, after), (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit())
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

    fn rendered(text: &str) -> String {
        split_punctuation(text)
            .iter()
            .map(|s| match s {
                Segment::Words(w) => (*w).to_string(),
                Segment::Mark(c) => format!("<{c}>"),
            })
            .collect()
    }

    #[test]
    fn a_separator_between_two_digits_is_part_of_the_number() {
        // The bug: each of these went to espeak-ng in halves, so `1,000` was
        // read "one, zero zero zero" and `1.8` was read "one. eight".
        assert_eq!(
            rendered("There were 1,000 of them"),
            "There were 1,000 of them"
        );
        assert_eq!(rendered("He was 1.8 meters"), "He was 1.8 meters");
        assert_eq!(rendered("It was 3:45"), "It was 3:45");
        assert_eq!(rendered("$1,500.50 a year"), "$1,500.50 a year");
        assert_eq!(rendered("1,000,000"), "1,000,000");
    }

    #[test]
    fn a_separator_that_is_not_between_two_digits_still_splits() {
        assert_eq!(rendered("In 2016."), "In 2016<.>");
        assert_eq!(rendered("page 12, line 3"), "page 12<,> line 3");
        assert_eq!(rendered("Chapter 1. The start"), "Chapter 1<.> The start");
        assert_eq!(
            rendered("He turned 40. 5 minutes on"),
            "He turned 40<.> 5 minutes on"
        );
        assert_eq!(rendered("a, b. c"), "a<,> b<.> c");
        // Not every mark is a number separator.
        assert_eq!(rendered("1;2"), "1<;>2");
    }

    #[test]
    fn overrides_only_match_whole_words() {
        assert_eq!(apply_overrides("I think"), "[['aI]] think");
        assert_eq!(apply_overrides("so I think"), "so [['aI]] think");
        assert_eq!(
            apply_overrides("But I saw, and I knew"),
            "But [['aI]] saw, and [['aI]] knew"
        );
        // `[['aI]]'m` is read "I em", and there is no `I` inside `It` or `aIr`.
        for untouched in ["I'm sure", "It is fine", "an Idea", "hI", "I\u{2019}ll go"] {
            assert_eq!(apply_overrides(untouched), untouched);
        }
        // Nothing to do is the borrowed path.
        assert!(matches!(
            apply_overrides("nothing here"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// misaki sorts `E2M` by descending key length and applies it in that
    /// order, so a longer pattern always wins over a shorter one it contains.
    #[test]
    fn the_mapping_table_is_longest_key_first() {
        let lens: Vec<usize> = E2M.iter().map(|(k, _)| k.chars().count()).collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "{lens:?}");
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
        // A mark does not glue the words either side of it together.
        assert!(ps.contains(", "), "{ps}");
    }

    /// The three defects this module had, against the real binary. Skipped
    /// where espeak-ng is not installed, like every other test that needs it.
    #[test]
    fn a_real_espeak_reads_numbers_and_reduced_vowels_and_i() {
        let g = Phonemizer::new("en-us");
        if g.probe().is_err() {
            eprintln!("skipping: no espeak-ng");
            return;
        }
        // "one thousand", not "one, zero zero zero".
        let thousand = g.phonemize("1,000").expect("phonemize");
        assert!(!thousand.contains(','), "{thousand}");
        assert_eq!(thousand, g.phonemize("one thousand").expect("phonemize"));
        // "one point eight", not "one. eight". Compared without the stress
        // marks, which espeak-ng places differently on digits and on words.
        let unstressed = |s: &str| s.replace(['\u{2C8}', '\u{2CC}'], "");
        let decimal = g.phonemize("1.8").expect("phonemize");
        assert!(!decimal.contains('.'), "{decimal}");
        assert_eq!(
            unstressed(&decimal),
            unstressed(&g.phonemize("one point eight").expect("phonemize"))
        );

        // The reduced vowel survives into the token stream instead of being
        // dropped, which is the difference between "before" and "fore".
        let before = g.phonemize("before").expect("phonemize");
        assert!(before.contains('\u{1D7B}'), "{before}");
        assert_eq!(
            crate::tts::kokoro::tokenize(&before).len(),
            before.chars().count(),
            "{before} lost a symbol"
        );

        // espeak-ng honours the `[[...]]` escape, so `I` keeps its stress at
        // the head of a sentence where espeak-ng would otherwise drop it.
        let stressed = g.phonemize("I think it is fine.").expect("phonemize");
        assert!(stressed.starts_with('\u{2C8}'), "{stressed}");
        assert!(!stressed.contains('['), "escape not honoured: {stressed}");
        assert!(!stressed.contains(']'), "escape not honoured: {stressed}");
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
