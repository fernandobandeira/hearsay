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
use std::sync::Mutex;

use crate::config::Config;

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

/// Decode any container ffmpeg understands into 16 kHz mono f32.
pub fn decode_16k_mono(path: &Path) -> Result<Vec<f32>, SttError> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-",
        ])
        .output()
        .map_err(|e| SttError::Decode(format!("ffmpeg: {e}")))?;
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
            }
        }

        pub fn available(&self) -> bool {
            self.model.exists()
        }

        pub fn transcribe(&self, path: &Path, prompt: &str) -> Result<Transcript, SttError> {
            if !self.model.exists() {
                return Err(SttError::Unavailable(format!(
                    "{} is missing",
                    self.model.display()
                )));
            }
            let audio = decode_16k_mono(path)?;
            // Serialized: the model is loaded once and one transcription runs at
            // a time.
            let mut guard = self
                .ctx
                .lock()
                .map_err(|_| SttError::Failed("transcriber mutex poisoned".into()))?;
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

    pub struct Whisper;

    impl Whisper {
        pub fn new(_cfg: &Config) -> Self {
            Self
        }
        pub fn available(&self) -> bool {
            false
        }
        pub fn transcribe(&self, _p: &Path, _prompt: &str) -> Result<Transcript, SttError> {
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
    fn a_recording_that_is_not_audio_is_an_error_not_a_panic() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("nope.webm");
        std::fs::write(&p, b"not audio").expect("write");
        assert!(decode_16k_mono(&p).is_err());
    }
}
