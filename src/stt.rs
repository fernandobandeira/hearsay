//! Voice memo -> text, with whisper.cpp through `whisper-rs`.
//!
//! The python server runs faster-whisper on CPU with `compute_type="int8"`,
//! serialized behind a lock. Same shape here: one quantized ggml model
//! (`large-v3-turbo-q5_0` by default), CPU, and one transcription at a time —
//! notes can be recorded back to back and each POST waits its turn, because
//! queueing on CPU beats parallel transcriptions starving each other while the
//! renderer keeps its own cores either way.
//!
//! Decoding is ffmpeg's job: MediaRecorder sends webm/opus and whisper wants
//! 16 kHz mono f32. ffmpeg is already in the image for packing and export.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;

/// Whisper outranks everything else this process does.
///
/// **The A1, 2026-09-11.** The box is two ARM cores. Kokoro synthesis already
/// runs at 0.26–0.30× realtime there, and whisper at 30–50× realtime; with
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
/// queue behind the whisper mutex as well: two memos arriving together park the
/// renderer once, for both, rather than letting it wake up between them.
/// Everything is released by [`SttGuard`]'s `Drop`, so an early return, a `?`, or
/// a panic inside whisper all open the gate on the way out — there is no manual
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

pub struct Transcript {
    pub text: String,
    pub language: String,
}

/// Resolve `WHISPER_MODEL`: a path is used as given, a bare name (`small`,
/// `large-v3-turbo-q5_0`) becomes `<models>/whisper/ggml-<name>.bin`. That keeps
/// the python server's env value meaningful without making it mean a file that
/// does not exist.
pub fn model_path(spec: &str, models_dir: &Path) -> PathBuf {
    if spec.contains('/') || spec.ends_with(".bin") {
        PathBuf::from(spec)
    } else {
        models_dir.join("whisper").join(format!("ggml-{spec}.bin"))
    }
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

#[cfg(feature = "stt")]
mod real {
    use super::*;
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    pub struct Whisper {
        ctx: Mutex<Option<WhisperContext>>,
        model: PathBuf,
        vad_model: Option<PathBuf>,
        threads: i32,
        gate: SttGate,
    }

    impl Whisper {
        pub fn new(cfg: &Config) -> Self {
            let vad = cfg.whisper_vad_model.clone().filter(|p| p.exists());
            if vad.is_none() {
                tracing::info!("whisper: no VAD model, transcribing the whole recording");
            }
            Self {
                ctx: Mutex::new(None),
                model: cfg.whisper_model.clone(),
                vad_model: vad,
                threads: cfg.whisper_threads.max(1) as i32,
                gate: SttGate::default(),
            }
        }

        pub fn available(&self) -> bool {
            self.model.exists()
        }

        /// The priority gate the renderer and the packer park on.
        pub fn gate(&self) -> &SttGate {
            &self.gate
        }

        pub fn transcribe(&self, path: &Path, prompt: &str) -> Result<Transcript, SttError> {
            // Claimed before anything else, and held across the ffmpeg decode and
            // the wait for the model mutex as well as the transcription itself:
            // a memo queued behind another one is still a memo the box should be
            // working on rather than rendering ahead of.
            let _busy = self.gate.enter();
            if !self.model.exists() {
                return Err(SttError::Unavailable(format!(
                    "{} is missing",
                    self.model.display()
                )));
            }
            let audio = decode_16k_mono(path)?;
            // Serialized: the model is loaded once and one transcription runs at
            // a time.
            //
            // A panic inside whisper.cpp's bindings poisons this lock, and
            // refusing every memo after it would turn one bad recording into no
            // voice notes until a restart. The context that was mid-transcription
            // is not trusted, though: it is dropped and the model loaded afresh.
            let mut guard = match self.ctx.lock() {
                Ok(g) => g,
                Err(p) => {
                    tracing::error!("whisper: a transcription panicked; reloading the model");
                    let mut g = p.into_inner();
                    *g = None;
                    self.ctx.clear_poison();
                    g
                }
            };
            if guard.is_none() {
                let c = WhisperContext::new_with_params(
                    &self.model,
                    WhisperContextParameters::default(),
                )
                .map_err(|e| SttError::Unavailable(format!("{}: {e}", self.model.display())))?;
                *guard = Some(c);
            }
            let Some(ctx) = guard.as_ref() else {
                return Err(SttError::Unavailable("model not loaded".into()));
            };
            let mut state = ctx
                .create_state()
                .map_err(|e| SttError::Failed(e.to_string()))?;

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_n_threads(self.threads);
            params.set_translate(false);
            // Auto-detect, exactly like faster-whisper's default.
            params.set_language(None);
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            // The initial prompt biases decoding toward the words it will
            // actually hear: the book being read and the user's own jargon. It
            // fixes most proper-noun mangling without a bigger model.
            if !prompt.is_empty() {
                params.set_initial_prompt(prompt);
            }
            if let Some(v) = &self.vad_model {
                params.set_vad_model_path(Some(&v.to_string_lossy()));
                params.enable_vad(true);
            }
            state
                .full(params, &audio)
                .map_err(|e| SttError::Failed(e.to_string()))?;

            let mut parts = Vec::new();
            for i in 0..state.full_n_segments() {
                if let Some(seg) = state.get_segment(i) {
                    if let Ok(s) = seg.to_str_lossy() {
                        let t = s.trim();
                        if !t.is_empty() {
                            parts.push(t.to_string());
                        }
                    }
                }
            }
            let lang = whisper_rs::get_lang_str(state.full_lang_id_from_state())
                .unwrap_or("en")
                .to_string();
            Ok(Transcript {
                text: parts.join(" ").trim().to_string(),
                language: lang,
            })
        }
    }
}

#[cfg(not(feature = "stt"))]
mod real {
    use super::*;

    #[derive(Default)]
    pub struct Whisper {
        gate: SttGate,
    }

    impl Whisper {
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
        pub fn transcribe(&self, _p: &Path, _prompt: &str) -> Result<Transcript, SttError> {
            let _busy = self.gate.enter();
            Err(SttError::Unavailable(
                "built without the stt feature".into(),
            ))
        }
    }
}

pub use real::Whisper;

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
        .err()
        .expect("must not succeed");
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
        .err()
        .expect("must not succeed");
        assert!(matches!(e, SttError::Decode(_)), "{e}");
    }

    #[test]
    fn a_bare_model_name_resolves_under_the_models_dir() {
        let m = Path::new("/models");
        assert_eq!(
            model_path("large-v3-turbo-q5_0", m),
            Path::new("/models/whisper/ggml-large-v3-turbo-q5_0.bin")
        );
        assert_eq!(
            model_path("small", m),
            Path::new("/models/whisper/ggml-small.bin")
        );
        assert_eq!(
            model_path("/opt/w/ggml-tiny.bin", m),
            Path::new("/opt/w/ggml-tiny.bin")
        );
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
        // whisper.cpp's bindings — that leaves the renderer parked forever.
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
    fn a_recording_that_is_not_audio_is_an_error_not_a_panic() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("nope.webm");
        std::fs::write(&p, b"not audio").expect("write");
        assert!(decode_16k_mono(&p).is_err());
    }
}
