//! Kokoro-82M through ONNX Runtime.
//!
//! The python server calls `KPipeline`, which is torch. This is the same weights
//! exported to ONNX (`onnx-community/Kokoro-82M-v1.0-ONNX`), driven by `ort`. The
//! model takes three inputs and nothing else:
//!
//! * `input_ids` — the phoneme string mapped through the tokenizer's vocabulary,
//!   wrapped in the boundary token `$` (id 0) at both ends. 510 phonemes max.
//! * `style` — 256 floats picked out of the voice pack by the *phoneme count*.
//!   The pack is 510 style vectors; index `n` is the one trained for an utterance
//!   of `n` phonemes, which is how Kokoro gets its pacing right.
//! * `speed` — one float.
//!
//! Output is float32 mono at 24 kHz, the same rate and layout `app/tts.py`
//! produces, so nothing downstream has to know which engine rendered a chunk.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ndarray::{Array1, Array2, Array3};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Value;

use crate::err::TtsError;

pub const SR: u32 = 24_000;
/// The style pack is 510 vectors of 256 floats; that is also the phoneme ceiling.
pub const MAX_PHONEMES: usize = 510;
const STYLE_DIM: usize = 256;

/// Kokoro's phoneme vocabulary, verbatim from `tokenizer.json` in the ONNX repo.
/// Kept in the source rather than read from the file: it is part of the model
/// contract, it is 178 entries, and a missing tokenizer.json should not be able
/// to silently change how a book is pronounced.
const VOCAB: &[(char, i64)] = &[
    ('$', 0),
    (';', 1),
    (':', 2),
    (',', 3),
    ('.', 4),
    ('!', 5),
    ('?', 6),
    ('\u{2014}', 9),
    ('\u{2026}', 10),
    ('"', 11),
    ('(', 12),
    (')', 13),
    ('\u{201C}', 14),
    ('\u{201D}', 15),
    (' ', 16),
    ('\u{303}', 17),
    ('\u{2A3}', 18),
    ('\u{2A5}', 19),
    ('\u{2A6}', 20),
    ('\u{2A8}', 21),
    ('\u{1D5D}', 22),
    ('\u{AB67}', 23),
    ('A', 24),
    ('I', 25),
    ('O', 31),
    ('Q', 33),
    ('S', 35),
    ('T', 36),
    ('W', 39),
    ('Y', 41),
    ('\u{1D4A}', 42),
    ('a', 43),
    ('b', 44),
    ('c', 45),
    ('d', 46),
    ('e', 47),
    ('f', 48),
    ('h', 50),
    ('i', 51),
    ('j', 52),
    ('k', 53),
    ('l', 54),
    ('m', 55),
    ('n', 56),
    ('o', 57),
    ('p', 58),
    ('q', 59),
    ('r', 60),
    ('s', 61),
    ('t', 62),
    ('u', 63),
    ('v', 64),
    ('w', 65),
    ('x', 66),
    ('y', 67),
    ('z', 68),
    ('\u{251}', 69),
    ('\u{250}', 70),
    ('\u{252}', 71),
    ('\u{E6}', 72),
    ('\u{3B2}', 75),
    ('\u{254}', 76),
    ('\u{255}', 77),
    ('\u{E7}', 78),
    ('\u{256}', 80),
    ('\u{F0}', 81),
    ('\u{2A4}', 82),
    ('\u{259}', 83),
    ('\u{25A}', 85),
    ('\u{25B}', 86),
    ('\u{25C}', 87),
    ('\u{25F}', 90),
    ('\u{261}', 92),
    ('\u{265}', 99),
    ('\u{268}', 101),
    ('\u{26A}', 102),
    ('\u{29D}', 103),
    ('\u{26F}', 110),
    ('\u{270}', 111),
    ('\u{14B}', 112),
    ('\u{273}', 113),
    ('\u{272}', 114),
    ('\u{274}', 115),
    ('\u{F8}', 116),
    ('\u{278}', 118),
    ('\u{3B8}', 119),
    ('\u{153}', 120),
    ('\u{279}', 123),
    ('\u{27E}', 125),
    ('\u{27B}', 126),
    ('\u{281}', 128),
    ('\u{27D}', 129),
    ('\u{282}', 130),
    ('\u{283}', 131),
    ('\u{288}', 132),
    ('\u{2A7}', 133),
    ('\u{28A}', 135),
    ('\u{28B}', 136),
    ('\u{28C}', 138),
    ('\u{263}', 139),
    ('\u{264}', 140),
    ('\u{3C7}', 142),
    ('\u{28E}', 143),
    ('\u{292}', 147),
    ('\u{294}', 148),
    ('\u{2C8}', 156),
    ('\u{2CC}', 157),
    ('\u{2D0}', 158),
    ('\u{2B0}', 162),
    ('\u{2B2}', 164),
    ('\u{2193}', 169),
    ('\u{2192}', 171),
    ('\u{2197}', 172),
    ('\u{2198}', 173),
    ('\u{1DFB}', 177),
];

fn token_id(c: char) -> Option<i64> {
    VOCAB.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

/// Phoneme string -> token ids, dropping anything the model has no symbol for
/// (rather than refusing the chunk: one unmappable character is not worth a
/// silent gap in a book).
pub fn tokenize(phonemes: &str) -> Vec<i64> {
    phonemes
        .chars()
        .filter_map(token_id)
        .take(MAX_PHONEMES)
        .collect()
}

/// One voice pack: 510 style vectors of 256 floats, little-endian f32.
#[derive(Debug, Clone)]
pub struct Voice {
    pub name: String,
    styles: Vec<f32>,
}

impl Voice {
    pub fn load(dir: &Path, name: &str) -> Result<Self, TtsError> {
        let p = dir.join(format!("{name}.bin"));
        let raw = std::fs::read(&p)
            .map_err(|e| TtsError::Voice(name.into(), format!("{}: {e}", p.display())))?;
        if raw.len() != MAX_PHONEMES * STYLE_DIM * 4 {
            return Err(TtsError::Voice(
                name.into(),
                format!(
                    "expected {} bytes, got {}",
                    MAX_PHONEMES * STYLE_DIM * 4,
                    raw.len()
                ),
            ));
        }
        let styles = raw
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Ok(Self {
            name: name.into(),
            styles,
        })
    }

    /// The style vector for an utterance of `n` phonemes.
    fn style(&self, n: usize) -> Array2<f32> {
        let i = n.min(MAX_PHONEMES - 1);
        let row = &self.styles[i * STYLE_DIM..(i + 1) * STYLE_DIM];
        Array2::from_shape_vec((1, STYLE_DIM), row.to_vec())
            .unwrap_or_else(|_| Array2::zeros((1, STYLE_DIM)))
    }
}

/// The synthesis engine. `Session` is `Send + Sync` but ONNX Runtime is happiest
/// with one inference at a time on a CPU already shared with the packer, so the
/// session sits behind a mutex and the render worker is the only caller.
pub struct Kokoro {
    session: Mutex<Session>,
    voice: Voice,
    speed: f32,
    pub model_path: PathBuf,
}

impl Kokoro {
    pub fn load(
        model: &Path,
        voices_dir: &Path,
        voice: &str,
        speed: f32,
        threads: usize,
    ) -> Result<Self, TtsError> {
        let voice = Voice::load(voices_dir, voice)?;
        let session = Session::builder()
            .map_err(|e| TtsError::Model(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| TtsError::Model(e.to_string()))?
            .with_intra_threads(threads)
            .map_err(|e| TtsError::Model(e.to_string()))?
            .commit_from_file(model)
            .map_err(|e| TtsError::Model(format!("{}: {e}", model.display())))?;
        Ok(Self {
            session: Mutex::new(session),
            voice,
            speed,
            model_path: model.to_path_buf(),
        })
    }

    pub fn voice_name(&self) -> &str {
        &self.voice.name
    }

    /// Render one phoneme string. Returns mono f32 at [`SR`].
    pub fn synth(&self, phonemes: &str) -> Result<Vec<f32>, TtsError> {
        let tokens = tokenize(phonemes);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let n = tokens.len();
        let mut ids = Vec::with_capacity(n + 2);
        ids.push(0);
        ids.extend_from_slice(&tokens);
        ids.push(0);

        let input_ids = Array2::from_shape_vec((1, ids.len()), ids)
            .map_err(|e| TtsError::Infer(e.to_string()))?;
        let style = self.voice.style(n);
        let speed = Array1::from_vec(vec![self.speed]);

        let mut sess = self
            .session
            .lock()
            .map_err(|_| TtsError::Infer("session mutex poisoned".into()))?;
        let outputs = sess
            .run(ort::inputs![
                "input_ids" => Value::from_array(input_ids).map_err(|e| TtsError::Infer(e.to_string()))?,
                "style" => Value::from_array(style).map_err(|e| TtsError::Infer(e.to_string()))?,
                "speed" => Value::from_array(speed).map_err(|e| TtsError::Infer(e.to_string()))?,
            ])
            .map_err(|e| TtsError::Infer(e.to_string()))?;
        let (_, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| TtsError::Infer(e.to_string()))?;
        Ok(data.to_vec())
    }
}

/// `silence(seconds)` from `app/tts.py`, same shape.
pub fn silence(seconds: f32) -> Vec<f32> {
    vec![0.0; (seconds * SR as f32) as usize]
}

/// The deterministic stand-in engine (`NARRATOR_FAKE_TTS=1` / `tts.use()` in the
/// python server): a quiet tone as long as the real thing would be, so the whole
/// render/pack/manifest path can be tested without 325 MB of weights.
pub fn fake(text: &str) -> Vec<f32> {
    let seconds = (text.chars().count() as f32 / 14.0).max(0.25);
    let n = (seconds * SR as f32) as usize;
    let hz = 110.0 + (fnv1a(text) % 220) as f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / SR as f32;
            0.05 * (2.0 * std::f32::consts::PI * hz * t).sin()
        })
        .collect()
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Where the soft knee starts. Below it a sample is multiplied and nothing else
/// happens, which is what makes `KOKORO_GAIN` a *flat* gain for all normal
/// material: the ONNX render's loudest sample in 6.5 million was one that
/// clipped, so at any gain at or below 1.0 this function is a plain multiply.
const KNEE: f32 = 0.95;

/// `KOKORO_GAIN`, applied post-synthesis.
///
/// The measured delta that motivates it: the ONNX render is uniformly ~1.4×
/// (≈ +3 dB) louder than the PyTorch render Fernando accepted. A gain is the
/// honest fix for that — but multiplying a waveform that already touches ±1.0
/// and then clamping is how a flat gain becomes audible distortion on exactly
/// the loudest words. So everything under the knee scales linearly and
/// everything above it is compressed into the remaining headroom with a `tanh`,
/// which is smooth, monotonic and can never leave the interval:
///
/// ```text
/// |y| <= KNEE          y = g·x
/// |y| >  KNEE          y = ±(KNEE + (1-KNEE)·tanh((|g·x| - KNEE)/(1-KNEE)))
/// ```
///
/// `g == 1.0` is the identity on any sample already in range, so the default
/// costs nothing and changes not one byte of a rendered chunk.
pub fn apply_gain(samples: &mut [f32], gain: f32) {
    if !gain.is_finite() || gain <= 0.0 || (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    let head = 1.0 - KNEE;
    for s in samples.iter_mut() {
        let y = *s * gain;
        let mag = y.abs();
        *s = if mag <= KNEE {
            y
        } else {
            y.signum() * (KNEE + head * ((mag - KNEE) / head).tanh())
        };
    }
}

/// f32 -> s16, the conversion `soundfile.write(path, wav, 24000)` does for a
/// 16-bit PCM wav. Clamped, because Kokoro can overshoot by a hair.
pub fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// Unused today, but the 3-D shape is what a batched export would return; kept so
/// the extraction path is obvious if the model is ever run over several chunks.
#[allow(dead_code)]
fn first_row(a: &Array3<f32>) -> Vec<f32> {
    a.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_has_no_duplicate_symbols() {
        let mut seen = std::collections::HashSet::new();
        for (c, _) in VOCAB {
            assert!(seen.insert(*c), "duplicate symbol {c:?}");
        }
    }

    #[test]
    fn unknown_symbols_are_dropped_not_fatal() {
        assert_eq!(tokenize("a\u{1F600}b"), vec![43, 44]);
    }

    #[test]
    fn tokens_are_capped_at_the_style_pack_size() {
        let long = "a".repeat(MAX_PHONEMES + 50);
        assert_eq!(tokenize(&long).len(), MAX_PHONEMES);
    }

    #[test]
    fn unity_gain_is_the_identity() {
        let orig = vec![-1.0, -0.5, 0.0, 0.25, 0.9999, 1.0];
        let mut s = orig.clone();
        apply_gain(&mut s, 1.0);
        assert_eq!(s, orig);
        // And so is anything nonsensical, rather than silence.
        for bad in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            let mut s = orig.clone();
            apply_gain(&mut s, bad);
            assert_eq!(s, orig, "gain {bad} should have been ignored");
        }
    }

    #[test]
    fn a_gain_is_flat_below_the_knee_and_never_clips_above_it() {
        let mut s = vec![0.1, -0.2, 0.3];
        apply_gain(&mut s, 2.0);
        // 2 x 0.3 = 0.6, still under the knee: a plain multiply.
        assert!((s[0] - 0.2).abs() < 1e-6);
        assert!((s[1] + 0.4).abs() < 1e-6);
        assert!((s[2] - 0.6).abs() < 1e-6);

        // Full-scale material at a big gain is compressed, not clamped to a
        // square wave: still inside the interval, and still distinguishable.
        let mut loud = vec![0.8, 0.95, 1.0, -1.0];
        apply_gain(&mut loud, 4.0);
        for v in &loud {
            assert!(v.abs() <= 1.0, "{v} left the interval");
            assert!(v.abs() > KNEE, "{v} lost its loudness");
        }
        // Non-decreasing: well past the knee the tanh has saturated, which is
        // the point — it is a limiter, not a clamp, and it got there smoothly.
        assert!(loud[1] >= loud[0], "monotonic through the knee");
        assert!(loud[2] >= loud[1], "monotonic above the knee");
        assert!((loud[3] + loud[2]).abs() < 1e-6, "symmetric");

        // The gain that actually matters: undoing the ~+3 dB delta the other
        // way. Near-full-scale material still moves, and still fits.
        let mut near = vec![0.9, 0.96, 1.0];
        apply_gain(&mut near, 1.4);
        assert!((near[0] - 1.26_f32).abs() > 0.2, "the knee did its work");
        assert!(near.iter().all(|v| *v <= 1.0 && *v > KNEE));
        assert!(near[1] >= near[0] && near[2] >= near[1]);
    }

    #[test]
    fn fake_is_deterministic_and_length_proportional() {
        let a = fake("hello world");
        let b = fake("hello world");
        assert_eq!(a, b);
        assert!(fake("a longer sentence than the other one").len() > a.len());
    }
}
