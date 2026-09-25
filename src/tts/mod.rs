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
use std::time::{Duration, Instant};

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
    /// Set once, and only by a load that worked. A failure used to be cached
    /// here too — `OnceLock<Result<..>>` — which made one bad moment permanent:
    /// a model file still being copied in, an espeak-ng probe that timed out
    /// under load, and the engine never tried again until the process was
    /// restarted. That is the opposite of healing.
    inner: OnceLock<Loaded>,
    /// When a load was last attempted and failed. Serialises attempts too: the
    /// lock is held across one, so two callers never load the model at once.
    attempted: Mutex<Option<Instant>>,
    /// How long after a failed load the next one may be tried. A failed load
    /// is a subprocess spawn and a model read; the render worker asks on every
    /// chunk, so without a floor a missing model is a load attempt per chunk.
    retry_every: Duration,
    /// The last load error, kept so `/healthz` and `/api/status` can say *why*
    /// nothing is rendering instead of just "not ready". Cleared by a load that
    /// succeeds.
    last_error: Mutex<Option<String>>,
}

/// The floor between two failed load attempts — the render worker's own
/// failure backoff cap, so a healing engine is noticed within one of its
/// pauses.
pub const LOAD_RETRY: Duration = Duration::from_secs(30);

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
            attempted: Mutex::new(None),
            retry_every: LOAD_RETRY,
            last_error: Mutex::new(None),
        }
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }

    pub fn error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Why the model is not loaded, if a load was tried and failed. `None`
    /// while it is loaded, while nothing has asked for it yet — loading is lazy,
    /// and a server that has not rendered anything is not broken — and always
    /// under the fake, which cannot fail.
    pub fn load_failure(&self) -> Option<String> {
        if self.fake || self.ready() {
            return None;
        }
        self.error()
    }

    /// Build the pipeline, or say it cannot be built yet. Returns whether the
    /// engine can speak.
    ///
    /// Success is kept for the life of the process; failure is kept for
    /// [`LOAD_RETRY`] and then tried again, so a model that turns up — or an
    /// espeak-ng that stops being wedged — is picked up without a restart.
    pub fn load(&self) -> bool {
        self.load_via(|| self.build())
    }

    fn load_via(&self, build: impl FnOnce() -> Result<Loaded, String>) -> bool {
        if self.inner.get().is_some() {
            return true;
        }
        let mut attempted = self
            .attempted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Another caller may have loaded it while this one waited for the lock.
        if self.inner.get().is_some() {
            return true;
        }
        if attempted.is_some_and(|t| t.elapsed() < self.retry_every) {
            return false;
        }
        match build() {
            Ok(l) => {
                let _ = self.inner.set(l);
                *attempted = None;
                let mut e = self
                    .last_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if e.take().is_some() {
                    tracing::info!("tts: loaded after an earlier failure");
                }
                true
            }
            Err(e) => {
                *attempted = Some(Instant::now());
                let mut g = self
                    .last_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if g.as_deref() != Some(e.as_str()) {
                    tracing::error!(
                        "tts unavailable: {e} (retrying every {}s)",
                        self.retry_every.as_secs()
                    );
                    *g = Some(e);
                }
                false
            }
        }
    }

    fn build(&self) -> Result<Loaded, String> {
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
    }

    pub fn ready(&self) -> bool {
        self.inner.get().is_some()
    }

    /// Render one chunk of text to f32 at [`SR`].
    pub fn generate(&self, text: &str) -> Result<Vec<f32>, TtsError> {
        if !self.load() {
            return Err(TtsError::Model(
                self.error().unwrap_or_else(|| "not loaded".into()),
            ));
        }
        match self.inner.get() {
            Some(Loaded::Fake) => Ok(kokoro::fake(text)),
            Some(Loaded::Real(k, g)) => {
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
            None => Err(TtsError::Model("not loaded".into())),
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
        assert!(e.load_failure().is_some(), "a failed load is reported");
    }

    #[test]
    fn a_failed_load_is_retried_and_heals() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.fake_tts = false;
        let mut e = Engine::new(&cfg);
        assert!(
            e.load_failure().is_none(),
            "nothing tried yet is not a failure"
        );
        e.retry_every = Duration::from_millis(50);
        let tries = std::cell::Cell::new(0);
        let fail = || {
            tries.set(tries.get() + 1);
            Err("model not there yet".to_string())
        };
        assert!(!e.load_via(fail));
        assert_eq!(e.load_failure().as_deref(), Some("model not there yet"));
        // Inside the floor, nobody tries again.
        assert!(!e.load_via(fail));
        assert_eq!(tries.get(), 1);
        std::thread::sleep(Duration::from_millis(60));
        // Past it, the load is attempted again, and this time it works.
        assert!(e.load_via(|| Ok(Loaded::Fake)));
        assert!(e.ready());
        assert!(e.load_failure().is_none());
        assert!(
            e.error().is_none(),
            "a healed engine has no error to report"
        );
        assert!(e.generate("hello").is_ok());
    }

    #[test]
    fn the_fake_never_reports_a_load_failure() {
        let d = tempfile::tempdir().expect("tempdir");
        let e = Engine::new(&Config::for_test(d.path()));
        assert!(e.load_failure().is_none());
    }
}
