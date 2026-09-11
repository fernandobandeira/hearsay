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

use std::process::{Command, Stdio};
use std::time::Duration;

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

impl Default for Phonemizer {
    fn default() -> Self {
        Self {
            binary: std::env::var("ESPEAK_BIN").unwrap_or_else(|_| "espeak-ng".into()),
            voice: std::env::var("ESPEAK_VOICE").unwrap_or_else(|_| "en-us".into()),
            timeout: Duration::from_secs(15),
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
    pub fn probe(&self) -> Result<String, TtsError> {
        let out = Command::new(&self.binary)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| TtsError::Espeak(format!("{}: {e}", self.binary)))?;
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
        let out = Command::new(&self.binary)
            .args(["-q", "--ipa=2", "-v", &self.voice, "--", text])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| TtsError::Espeak(format!("spawn {}: {e}", self.binary)))?;
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

    /// The timeout is part of the struct so callers can shorten it in tests.
    pub fn timeout(&self) -> Duration {
        self.timeout
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
