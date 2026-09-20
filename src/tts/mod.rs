//! The synthesis engine, behind one call: `generate(text) -> f32 @ 24 kHz`.
//!
//! Two implementations sit behind it. Kokoro-82M through ONNX Runtime is the
//! real one; a deterministic fake (`NARRATOR_FAKE_TTS=1`) is a quiet tone as long
//! as the real thing would be, which is how the whole render/pack/manifest path
//! is tested in milliseconds with no weights and no network — the same trick
//! `app/tts.py`'s `use()` plays.
//!
//! Loading is lazy and **never fatal**. A missing model file, a voice pack that
//! will not parse, an espeak-ng that is not installed: each of those leaves the
//! engine unavailable and the renderer logging and idling, not a process that
//! exits. The reader still serves text, positions and voice notes without a
//! single rendered chunk, and that is the state the server has to survive in.

pub mod g2p;
pub mod kokoro;
pub mod numbers;

use std::sync::{Mutex, OnceLock};

use crate::config::Config;
use crate::err::TtsError;

pub use kokoro::{silence, to_i16, SR};

enum Loaded {
    Real(Box<kokoro::Kokoro>, g2p::Phonemizer),
    Fake,
}

pub struct Engine {
    model: std::path::PathBuf,
    voices: std::path::PathBuf,
    voice: String,
    speed: f32,
    gain: f32,
    threads: usize,
    fake: bool,
    inner: OnceLock<Result<Loaded, String>>,
    /// The last load error, kept so `/healthz` and `/api/status` can say *why*
    /// nothing is rendering instead of just "not ready".
    last_error: Mutex<Option<String>>,
}

impl Engine {
    pub fn new(cfg: &Config) -> Self {
        Self {
            model: cfg.kokoro_model.clone(),
            voices: cfg.kokoro_voices_dir.clone(),
            voice: cfg.kokoro_voice.clone(),
            speed: cfg.kokoro_speed,
            gain: cfg.kokoro_gain,
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            fake: cfg.fake_tts,
            inner: OnceLock::new(),
            last_error: Mutex::new(None),
        }
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }

    pub fn error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    /// Build the pipeline once. Returns whether the engine can speak.
    pub fn load(&self) -> bool {
        let r = self.inner.get_or_init(|| {
            if self.fake {
                return Ok(Loaded::Fake);
            }
            let g = g2p::Phonemizer::default();
            match g.probe() {
                Ok(v) => tracing::info!("espeak-ng: {v}"),
                Err(e) => return Err(e.to_string()),
            }
            match kokoro::Kokoro::load(
                &self.model,
                &self.voices,
                &self.voice,
                self.speed,
                self.threads,
            ) {
                Ok(k) => {
                    tracing::info!(
                        "kokoro: {} voice {} ({} threads)",
                        self.model.display(),
                        self.voice,
                        self.threads
                    );
                    Ok(Loaded::Real(Box::new(k), g))
                }
                Err(e) => Err(e.to_string()),
            }
        });
        match r {
            Ok(_) => true,
            Err(e) => {
                if let Ok(mut g) = self.last_error.lock() {
                    if g.as_deref() != Some(e.as_str()) {
                        tracing::error!("tts unavailable: {e}");
                        *g = Some(e.clone());
                    }
                }
                false
            }
        }
    }

    pub fn ready(&self) -> bool {
        matches!(self.inner.get(), Some(Ok(_)))
    }

    /// Render one chunk of text to f32 at [`SR`].
    pub fn generate(&self, text: &str) -> Result<Vec<f32>, TtsError> {
        if !self.load() {
            return Err(TtsError::Model(
                self.error().unwrap_or_else(|| "not loaded".into()),
            ));
        }
        match self.inner.get() {
            Some(Ok(Loaded::Fake)) => Ok(kokoro::fake(text)),
            Some(Ok(Loaded::Real(k, g))) => {
                let phonemes = g.phonemize(text)?;
                let mut wav = k.synth(&phonemes)?;
                // The one thing done to the model's output, and only when asked
                // for: the ONNX render measures ~+3 dB over the PyTorch render,
                // and `KOKORO_GAIN` is how that is answered without touching the
                // model. At the default 1.0 this is a no-op.
                kokoro::apply_gain(&mut wav, self.gain);
                // Kokoro hands back each utterance inside ~0.8 s of its own
                // silence. Left in, it is added to every gap the packer
                // inserts, and it is the larger half of both.
                kokoro::trim_padding(&mut wav);
                if wav.is_empty() {
                    // Nothing came back — a beat is a better chunk file than a
                    // zero-length wav, which the reader would happily "play" as
                    // an instant gap.
                    return Ok(kokoro::silence(0.1));
                }
                Ok(wav)
            }
            _ => Err(TtsError::Model("not loaded".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_engine_needs_no_weights() {
        let d = tempfile::tempdir().expect("tempdir");
        let cfg = Config::for_test(d.path());
        let e = Engine::new(&cfg);
        assert!(e.load());
        let a = e.generate("hello there").expect("generate");
        assert!(!a.is_empty());
        assert_eq!(a, e.generate("hello there").expect("generate"));
    }

    #[test]
    fn a_missing_model_degrades_instead_of_panicking() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.fake_tts = false;
        cfg.kokoro_model = d.path().join("nope.onnx");
        cfg.kokoro_voices_dir = d.path().join("nope");
        let e = Engine::new(&cfg);
        assert!(!e.load());
        assert!(e.generate("x").is_err());
        assert!(e.error().is_some());
        // And calling again is still not a panic.
        assert!(!e.load());
    }
}
