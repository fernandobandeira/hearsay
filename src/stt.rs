//! Voice memo -> text, with Phonon-2 through the vendored C engine.
//!
//! **The model.** [Phonon-2](https://huggingface.co/FermionResearch/Phonon-2) is
//! Fermion Research's five-value re-training of NVIDIA's Parakeet-TDT 0.6B v3: a
//! FastConformer encoder whose linear weights each take one of five values per
//! row, an LSTM prediction network and a token-and-duration (TDT) joint. 255 MB
//! of weights against the 574 MB of the whisper model it replaced, a lower
//! word-error rate on English (5.21 % against `large-v3-turbo`'s 6.58 % over the
//! Open ASR Leaderboard's seven sets), and — the reason for the swap — fast
//! enough on the A1's two cores that a memo is seconds of CPU rather than half an
//! hour of it. See AGENTS.md for the measurement.
//!
//! **What it cannot do that whisper could**, all accepted on purpose: it is
//! English only (every memo is English, so `language` is always `"en"`), and a
//! transducer takes no initial prompt, so book and chapter titles no longer bias
//! the spelling of names. In exchange it does not hallucinate: whisper's decoder
//! invents sentences over silence, which is what its VAD model was there to
//! paper over, and a transducer emits nothing for nothing.
//!
//! **The engine** is [phonon-2-c](https://github.com/eschmidbauer/phonon-2-c),
//! vendored under `vendor/phonon-2-c` and compiled by `build.rs` — pure C, no
//! dependencies beyond libc, libm and pthreads, int8 `SDOT` kernels on ARM, and
//! verified token for token against the transformers reference. It cuts audio
//! longer than 35 s into pause-aligned windows itself, which is the model's
//! regime. `native/phonon_shim.c` is the only code that calls it.
//!
//! One transcription at a time, behind a mutex, exactly as before: memos can be
//! recorded back to back and each POST waits its turn, because queueing on CPU
//! beats parallel transcriptions starving each other.
//!
//! Decoding is ffmpeg's job: MediaRecorder sends webm/opus and the engine wants
//! mono f32. ffmpeg is already in the image for packing and export.

use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;

/// Transcription outranks everything else this process does.
///
/// **The A1, 2026-09-11.** The box is two ARM cores. Kokoro synthesis already
/// runs at 0.26–0.30× realtime there, and whisper (the transcriber then) took
/// 30–50× realtime; with
/// twenty chapters queued, a ~45 s memo took **over seven minutes** to come back
/// because the two of them were fighting over the same two cores, and the render
/// RTF fell to 0.26 for the duration. Neither job got a machine.
///
/// The tie-break is not about speed, it is about what is recoverable. A memo
/// exists in exactly one place — IndexedDB on a phone — until `/api/note`
/// answers 2xx; until then it is one browser-data clear away from being gone for
/// good. A rendered chunk is a file the server can make again from a book it
/// still has. So when a transcription is in flight, the renderer and the packer
/// stand down and let it have the box.
///
/// The gate counts memos that are *transcribing or waiting to*, so it covers the
/// queue behind the model mutex as well: two memos arriving together park the
/// renderer once, for both, rather than letting it wake up between them.
/// Everything is released by [`SttGuard`]'s `Drop`, so an early return, a `?`, or
/// a panic inside the transcriber all open the gate on the way out — there is no manual
/// release path to forget.
#[derive(Default)]
pub struct SttGate {
    /// Transcriptions in flight or queued behind the model mutex.
    n: Mutex<usize>,
    cv: Condvar,
}

/// The claim. Dropping it gives the machine back.
pub struct SttGuard<'a>(&'a SttGate);

impl SttGate {
    /// Take the box for a transcription, from now until the guard is dropped.
    pub fn enter(&self) -> SttGuard<'_> {
        *self.count() += 1;
        SttGuard(self)
    }

    /// Is anything transcribing (or waiting to)?
    pub fn held(&self) -> bool {
        *self.count() > 0
    }

    /// Block until nothing is transcribing, or `timeout` runs out. Returns
    /// whether the gate is clear — false means "still held, come back".
    ///
    /// A bounded wait rather than a sleep loop: the waiter wakes the instant the
    /// last guard drops, and the timeout is only there so the caller keeps
    /// checking its own stop flag.
    pub fn wait_clear(&self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        let mut g = self.count();
        while *g > 0 {
            let left = timeout.saturating_sub(t0.elapsed());
            if left.is_zero() {
                return false;
            }
            match self.cv.wait_timeout(g, left) {
                Ok((next, _)) => g = next,
                // A poisoned count is a count, not a reason to park forever.
                Err(e) => g = e.into_inner().0,
            }
        }
        true
    }

    /// The count, recovering from poisoning rather than panicking: a gate that
    /// cannot be read is a renderer parked for the life of the process.
    fn count(&self) -> std::sync::MutexGuard<'_, usize> {
        match self.n.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }
}

impl Drop for SttGuard<'_> {
    fn drop(&mut self) {
        {
            let mut g = self.0.count();
            *g = g.saturating_sub(1);
        }
        self.0.cv.notify_all();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("no transcriber: {0}")]
    Unavailable(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("transcription failed: {0}")]
    Failed(String),
}

#[derive(Debug)]
pub struct Transcript {
    pub text: String,
    pub language: String,
}

/// How long ffmpeg gets to decode one memo before it is killed.
///
/// Decoding is the cheap part — seconds for a long memo even on the A1 — so a
/// decode still running after two minutes is wedged, not busy: a recording
/// ffmpeg is looping on, a filesystem that stopped answering. And it is wedged
/// *while holding the STT gate*, which parks the renderer and the packer, so an
/// unbounded wait here is the whole box standing still for one bad file.
pub const DECODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Decode any container ffmpeg understands into 16 kHz mono f32.
pub fn decode_16k_mono(path: &Path) -> Result<Vec<f32>, SttError> {
    decode_16k_mono_within(path, "ffmpeg", DECODE_TIMEOUT)
}

/// [`decode_16k_mono`] with the binary and the deadline named, so a test can
/// make a wedge cheap.
fn decode_16k_mono_within(
    path: &Path,
    ffmpeg: &str,
    timeout: std::time::Duration,
) -> Result<Vec<f32>, SttError> {
    let mut cmd = std::process::Command::new(ffmpeg);
    // `-nostdin`: ffmpeg reads the terminal for interactive keys unless told
    // not to, and a server has no terminal to give it.
    cmd.args(["-nostdin", "-v", "error", "-i"]).arg(path).args([
        "-f",
        "f32le",
        "-acodec",
        "pcm_f32le",
        "-ac",
        "1",
        "-ar",
        "16000",
        "-",
    ]);
    use crate::tts::g2p::{output_within, Bounded};
    let out = output_within(&mut cmd, timeout).map_err(|e| match e {
        Bounded::Spawn(e) | Bounded::Pipes(e) => SttError::Decode(format!("{ffmpeg}: {e}")),
        Bounded::TimedOut => SttError::Decode(format!(
            "{ffmpeg} did not finish within {}s; killed",
            timeout.as_secs()
        )),
    })?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        return Err(SttError::Decode(
            e.trim()
                .lines()
                .last()
                .unwrap_or("ffmpeg failed")
                .to_string(),
        ));
    }
    if out.stdout.len() < 4 {
        return Err(SttError::Decode("no audio in the recording".into()));
    }
    Ok(out
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

/// Every memo is English; the model knows no other language.
pub const LANGUAGE: &str = "en";

/// The files a weights directory has to hold, as `scripts/fetch-models.sh`
/// unpacks them. `weights.bin` is the one that is 255 MB, and the one checked.
pub fn weights_present(dir: &Path) -> bool {
    dir.join("weights.bin").is_file()
}

#[cfg(feature = "stt")]
mod real {
    use super::*;
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::path::PathBuf;
    use std::ptr::NonNull;

    // `native/phonon_shim.c`. Opaque pointers only; the engine's own structs never
    // cross the boundary.
    extern "C" {
        fn hearsay_p2_pool_create(n_threads: c_int) -> *mut c_void;
        fn hearsay_p2_pool_destroy(pool: *mut c_void);
        fn hearsay_p2_open(weights_dir: *const c_char, pool: *mut c_void) -> *mut c_void;
        fn hearsay_p2_close(engine: *mut c_void);
        fn hearsay_p2_transcribe(
            engine: *mut c_void,
            pool: *mut c_void,
            audio: *const f32,
            n_samples: c_int,
            rate: c_int,
            text: *mut *mut c_char,
            seconds_audio: *mut f64,
            seconds_encoder: *mut f64,
        ) -> c_int;
        fn hearsay_p2_free(text: *mut c_char);
    }

    /// A loaded engine and the thread pool it runs on. Both are owned here and
    /// released in `Drop`, the engine first: it was opened on the pool.
    struct Loaded {
        engine: NonNull<c_void>,
        pool: NonNull<c_void>,
    }

    // SAFETY: the engine is documented as callable from any thread
    // (`p2_transcribe` "may be called from any number of threads over one
    // engine"), and the pool is only ever installed on the calling thread for the
    // length of one call. Neither is touched outside the mutex in `Transcriber`.
    unsafe impl Send for Loaded {}

    impl Loaded {
        fn open(dir: &Path, threads: usize) -> Result<Self, SttError> {
            let cdir = CString::new(dir.to_string_lossy().as_bytes())
                .map_err(|_| SttError::Unavailable(format!("{}: bad path", dir.display())))?;
            let n = c_int::try_from(threads.max(1)).unwrap_or(c_int::MAX);
            // SAFETY: plain constructor; NULL on failure, checked below.
            let pool = NonNull::new(unsafe { hearsay_p2_pool_create(n) })
                .ok_or_else(|| SttError::Unavailable("could not start the thread pool".into()))?;
            // SAFETY: `cdir` outlives the call; `pool` is live. The engine logs
            // the reason for a NULL to stderr, which is the container log.
            match NonNull::new(unsafe { hearsay_p2_open(cdir.as_ptr(), pool.as_ptr()) }) {
                Some(engine) => Ok(Self { engine, pool }),
                None => {
                    // SAFETY: created just above and not shared with anything.
                    unsafe { hearsay_p2_pool_destroy(pool.as_ptr()) };
                    Err(SttError::Unavailable(format!(
                        "{}: the engine would not load these weights",
                        dir.display()
                    )))
                }
            }
        }

        /// Mono f32 at `rate` -> (text, seconds of audio, seconds in the encoder).
        fn transcribe(&self, audio: &[f32], rate: u32) -> Result<(String, f64, f64), SttError> {
            let n = c_int::try_from(audio.len())
                .map_err(|_| SttError::Decode("recording too long to transcribe".into()))?;
            let rate = c_int::try_from(rate).unwrap_or(c_int::MAX);
            let mut text: *mut c_char = std::ptr::null_mut();
            let (mut secs, mut enc) = (0.0f64, 0.0f64);
            // SAFETY: `audio` is `n` valid floats for the length of the call; the
            // engine and pool are live while `self` is; the out-pointers are to
            // locals. On success `text` is a malloc'd NUL-terminated string that
            // only `hearsay_p2_free` releases.
            let rc = unsafe {
                hearsay_p2_transcribe(
                    self.engine.as_ptr(),
                    self.pool.as_ptr(),
                    audio.as_ptr(),
                    n,
                    rate,
                    &mut text,
                    &mut secs,
                    &mut enc,
                )
            };
            if rc != 0 || text.is_null() {
                return Err(SttError::Failed("the engine refused this recording".into()));
            }
            // SAFETY: non-null and NUL-terminated per the shim's contract, and
            // freed exactly once, after the copy.
            let s = unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned();
            unsafe { hearsay_p2_free(text) };
            Ok((s, secs, enc))
        }
    }

    impl Drop for Loaded {
        fn drop(&mut self) {
            // SAFETY: both were created by `open` and are released once, here.
            unsafe {
                hearsay_p2_close(self.engine.as_ptr());
                hearsay_p2_pool_destroy(self.pool.as_ptr());
            }
        }
    }

    pub struct Transcriber {
        engine: Mutex<Option<Loaded>>,
        model: PathBuf,
        threads: usize,
        gate: SttGate,
    }

    impl Transcriber {
        pub fn new(cfg: &Config) -> Self {
            // Said once, at startup, rather than discovered at the first memo:
            // without weights every memo is a 500 that the reader's outbox keeps
            // retrying, which is safe and completely silent.
            if !weights_present(&cfg.stt_model) {
                tracing::warn!(
                    "stt: no Phonon-2 weights in {}; voice memos will wait in the \
                     reader's outbox until they are there (scripts/fetch-models.sh)",
                    cfg.stt_model.display()
                );
            }
            Self {
                engine: Mutex::new(None),
                model: cfg.stt_model.clone(),
                threads: cfg.stt_threads.max(1),
                gate: SttGate::default(),
            }
        }

        pub fn available(&self) -> bool {
            weights_present(&self.model)
        }

        /// The priority gate the renderer and the packer park on.
        pub fn gate(&self) -> &SttGate {
            &self.gate
        }

        pub fn transcribe(&self, path: &Path) -> Result<Transcript, SttError> {
            // Claimed before anything else, and held across the ffmpeg decode and
            // the wait for the model mutex as well as the transcription itself:
            // a memo queued behind another one is still a memo the box should be
            // working on rather than rendering ahead of.
            let _busy = self.gate.enter();
            if !self.available() {
                return Err(SttError::Unavailable(format!(
                    "no Phonon-2 weights in {}",
                    self.model.display()
                )));
            }
            let audio = decode_16k_mono(path)?;
            // Serialized: the model is loaded once and one transcription runs at
            // a time.
            //
            // Nothing in the C engine can unwind into this lock, but a panic in
            // the Rust around it still could, and refusing every memo after one
            // would mean no voice notes until a restart. The engine that was in
            // use is not trusted after that: dropped, and loaded afresh.
            let mut guard = match self.engine.lock() {
                Ok(g) => g,
                Err(p) => {
                    tracing::error!("stt: a transcription panicked; reloading the model");
                    let mut g = p.into_inner();
                    *g = None;
                    self.engine.clear_poison();
                    g
                }
            };
            if guard.is_none() {
                let t0 = Instant::now();
                *guard = Some(Loaded::open(&self.model, self.threads)?);
                tracing::info!(
                    "stt: Phonon-2 loaded from {} in {:.2}s, {} threads",
                    self.model.display(),
                    t0.elapsed().as_secs_f64(),
                    self.threads
                );
            }
            let Some(engine) = guard.as_ref() else {
                return Err(SttError::Unavailable("model not loaded".into()));
            };
            let t0 = Instant::now();
            let (text, secs, enc) = engine.transcribe(&audio, 16_000)?;
            let wall = t0.elapsed().as_secs_f64();
            tracing::info!(
                "stt: {secs:.1}s of audio in {wall:.2}s ({:.1}x realtime, encoder {enc:.2}s)",
                if wall > 0.0 { secs / wall } else { 0.0 }
            );
            Ok(Transcript {
                text: text.trim().to_string(),
                language: LANGUAGE.to_string(),
            })
        }
    }
}

#[cfg(not(feature = "stt"))]
mod real {
    use super::*;

    #[derive(Default)]
    pub struct Transcriber {
        gate: SttGate,
    }

    impl Transcriber {
        pub fn new(_cfg: &Config) -> Self {
            Self::default()
        }
        pub fn available(&self) -> bool {
            false
        }
        /// Present in both builds, so nothing that parks on it has to know
        /// whether this binary can actually transcribe.
        pub fn gate(&self) -> &SttGate {
            &self.gate
        }
        pub fn transcribe(&self, _p: &Path) -> Result<Transcript, SttError> {
            let _busy = self.gate.enter();
            Err(SttError::Unavailable(
                "built without the stt feature".into(),
            ))
        }
    }
}

pub use real::Transcriber;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_wedged_ffmpeg_is_killed_rather_than_holding_the_gate_forever() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().expect("tempdir");
        let bin = d.path().join("ffmpeg-wedged");
        std::fs::write(&bin, "#!/bin/sh\nsleep 300\n").expect("stub");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let memo = d.path().join("memo.webm");
        std::fs::write(&memo, b"not really a webm").expect("memo");
        let t0 = std::time::Instant::now();
        let e = decode_16k_mono_within(
            &memo,
            &bin.to_string_lossy(),
            std::time::Duration::from_millis(150),
        )
        .expect_err("must not succeed");
        assert!(
            matches!(e, SttError::Decode(ref m) if m.contains("killed")),
            "{e}"
        );
        assert!(t0.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_missing_ffmpeg_is_a_decode_error_not_a_hang() {
        let e = decode_16k_mono_within(
            Path::new("/nonexistent.webm"),
            "/nonexistent/ffmpeg",
            std::time::Duration::from_secs(1),
        )
        .expect_err("must not succeed");
        assert!(matches!(e, SttError::Decode(_)), "{e}");
    }

    #[test]
    fn the_gate_is_clear_until_something_claims_it() {
        let g = SttGate::default();
        assert!(!g.held());
        assert!(
            g.wait_clear(Duration::from_millis(1)),
            "nothing to wait for"
        );
        {
            let _a = g.enter();
            assert!(g.held());
            // A second memo queued behind the first keeps it held, so the
            // renderer does not wake up in the gap between them.
            let _b = g.enter();
            assert!(g.held());
            assert!(!g.wait_clear(Duration::from_millis(20)), "still busy");
        }
        assert!(!g.held(), "both guards released on the way out of scope");
        assert!(g.wait_clear(Duration::from_millis(1)));
    }

    #[test]
    fn a_waiter_wakes_the_moment_the_last_guard_drops() {
        let g = std::sync::Arc::new(SttGate::default());
        let held = g.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let busy = held.enter();
            tx.send(()).ok();
            std::thread::sleep(Duration::from_millis(60));
            drop(busy);
        });
        rx.recv().ok();
        let t0 = Instant::now();
        // Ten seconds of headroom: what is asserted is that it does not *use*
        // them, not the exact wake-up latency.
        assert!(g.wait_clear(Duration::from_secs(10)));
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        t.join().ok();
    }

    #[test]
    fn a_transcription_that_panics_still_opens_the_gate() {
        // The whole reason this is an RAII guard and not a pair of calls: there
        // is no path out of `transcribe` — early return, `?`, or a panic inside
        // the transcriber — that leaves the renderer parked forever.
        let g = std::sync::Arc::new(SttGate::default());
        let g2 = g.clone();
        let t = std::thread::spawn(move || {
            let _busy = g2.enter();
            panic!("a transcription blowing up, on purpose");
        });
        assert!(t.join().is_err(), "the thread was supposed to panic");
        assert!(!g.held(), "unwinding ran Drop");
    }

    #[test]
    fn missing_weights_are_unavailable_and_still_open_the_gate() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.stt_model = d.path().join("no-such-weights");
        let t = Transcriber::new(&cfg);
        assert!(!t.available());
        let memo = d.path().join("memo.webm");
        std::fs::write(&memo, b"anything").expect("memo");
        let e = t.transcribe(&memo).expect_err("no weights, no transcript");
        assert!(matches!(e, SttError::Unavailable(_)), "{e}");
        assert!(!t.gate().held(), "the refusal released the gate");
    }

    /// The real model on a real recording — opt-in, because the suite carries
    /// no weights. Runs whenever `models/phonon-2` (or `STT_MODEL`) holds them,
    /// which `./narrator models` arranges.
    ///
    /// The clip is LibriSpeech test-clean 5105-28240-0004 (CC BY 4.0, see
    /// `tests/fixtures/stt/README.md`), decoded through ffmpeg exactly as a memo
    /// is. What is asserted is the words, not the punctuation or the casing the
    /// model adds: this proves the FFI, the decode and the build flags all line
    /// up, and a regression in any of them is a garbled transcript, not a
    /// slightly different one.
    #[cfg(feature = "stt")]
    #[test]
    fn phonon_transcribes_a_librispeech_clip_when_the_weights_are_here() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = std::env::var_os("STT_MODEL")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.join("models/phonon-2"));
        if !weights_present(&dir) {
            eprintln!("skipped: no Phonon-2 weights in {}", dir.display());
            return;
        }
        let d = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::for_test(d.path());
        cfg.stt_model = dir;
        cfg.stt_threads = 2;
        let t = Transcriber::new(&cfg);
        let fx = root.join("tests/fixtures/stt");
        let got = t
            .transcribe(&fx.join("5105-28240-0004.wav"))
            .expect("transcribed");
        let want = std::fs::read_to_string(fx.join("5105-28240-0004.txt")).expect("reference");
        let words = |s: &str| {
            s.split_whitespace()
                .map(|w| {
                    w.chars()
                        .filter(|c| c.is_alphanumeric() || *c == '\'')
                        .collect::<String>()
                        .to_lowercase()
                })
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
        };
        assert_eq!(words(&got.text), words(&want), "{:?}", got.text);
        assert_eq!(got.language, "en");
        assert!(!t.gate().held());
    }

    #[test]
    fn a_recording_that_is_not_audio_is_an_error_not_a_panic() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("nope.webm");
        std::fs::write(&p, b"not audio").expect("write");
        assert!(decode_16k_mono(&p).is_err());
    }
}
