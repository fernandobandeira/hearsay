//! The config surface, read once at startup.
//!
//! Every name here is the one `narrator`'s AGENTS.md documents, with the same
//! default, so a container can be pointed at this binary with its existing
//! `docker run` line unchanged. A value that will not parse logs a warning and
//! falls back to the default — a typo in an env var is not a reason to refuse to
//! start a reader.

use std::path::PathBuf;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn num<T: std::str::FromStr + std::fmt::Display + Copy>(key: &str, default: T) -> T {
    match env(key) {
        None => default,
        Some(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!("{key}={v:?} is not a number, using {default}");
                default
            }
        },
    }
}

fn flag(key: &str, default: bool) -> bool {
    match env(key) {
        None => default,
        Some(v) => v == "1",
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub work: PathBuf,
    pub books: Vec<PathBuf>,
    pub web: PathBuf,
    pub vault: Option<PathBuf>,
    pub notes_dir: PathBuf,
    pub positions_dir: PathBuf,

    pub lookahead: usize,
    pub prefetch_while_paused: bool,
    pub prerender_chapters: usize,
    pub max_audio_gb: f64,
    pub max_chapter_gb: f64,
    pub silence_s: f64,

    pub kokoro_voice: String,
    pub kokoro_speed: f32,
    /// `KOKORO_GAIN` — a linear multiplier applied to every rendered chunk,
    /// with a soft knee so it cannot clip (see [`crate::tts::kokoro::apply_gain`]).
    /// 1.0, the default, is the identity and leaves rendered bytes untouched.
    pub kokoro_gain: f32,
    pub kokoro_model: PathBuf,
    pub kokoro_voices_dir: PathBuf,
    pub fake_tts: bool,

    pub models: PathBuf,
    pub whisper_model: PathBuf,
    pub whisper_vad_model: Option<PathBuf>,
    pub whisper_prompt: String,
    /// `WHISPER_THREADS` — how many threads one transcription may use. Defaults
    /// to every core, which is what it was before this was configurable, and is
    /// measured to be the right answer on the 2-core box (see AGENTS.md).
    pub whisper_threads: usize,

    pub chapter_bitrate: String,
    pub chapter_gap_s: f64,
    pub chapter_para_gap_s: f64,
    pub hls_segment_s: f64,

    pub text_shard_bytes: usize,
    pub text_shard_chapters: usize,
    pub health_stall_s: f64,

    pub autopack: bool,
    pub autopack_every_s: f64,
    pub watch_books: bool,

    pub sse_heartbeat_s: f64,
    pub sse_queue: usize,
    pub sse_render_min_s: f64,
    pub sse_retry_ms: u64,
}

impl Config {
    pub fn from_env() -> Self {
        let work = PathBuf::from(env("NARRATOR_WORK").unwrap_or_else(|| "/work".into()));
        let vault = env("NARRATOR_VAULT").map(PathBuf::from).filter(|p| {
            let ok = p.is_dir();
            if !ok {
                tracing::warn!(
                    "NARRATOR_VAULT={} is not a directory; vault integration off",
                    p.display()
                );
            }
            ok
        });
        let mut books = vec![PathBuf::from(
            env("NARRATOR_BOOKS").unwrap_or_else(|| "/books".into()),
        )];
        let notes_dir = match &vault {
            Some(v) => v.join(env("NOTES_SUBDIR").unwrap_or_else(|| "05 - Fleeting".into())),
            None => work.join("notes"),
        };
        let positions_dir = match &vault {
            Some(v) => v.join(env("POSITIONS_SUBDIR").unwrap_or_else(|| "02 - Studies".into())),
            None => work.clone(),
        };
        if let Some(v) = &vault {
            books
                .push(v.join(env("BOOKS_SUBDIR").unwrap_or_else(|| "03 - Resources/Books".into())));
        }
        let models = PathBuf::from(env("NARRATOR_MODELS").unwrap_or_else(|| "/models".into()));
        Self {
            port: num("NARRATOR_PORT", 7870),
            web: PathBuf::from(env("NARRATOR_WEB").unwrap_or_else(|| "/web".into())),
            work,
            books,
            vault,
            notes_dir,
            positions_dir,

            lookahead: num("LOOKAHEAD", 80),
            prefetch_while_paused: flag("PREFETCH_WHILE_PAUSED", true),
            prerender_chapters: num("PRERENDER_CHAPTERS", 2),
            max_audio_gb: num("MAX_AUDIO_GB", 5.0),
            max_chapter_gb: num("MAX_CHAPTER_GB", 20.0),
            silence_s: num("SILENCE_S", 0.5),

            kokoro_voice: env("KOKORO_VOICE").unwrap_or_else(|| "af_heart".into()),
            kokoro_speed: num("KOKORO_SPEED", 1.0f32),
            kokoro_gain: num("KOKORO_GAIN", 1.0f32),
            kokoro_model: env("KOKORO_MODEL")
                .map(PathBuf::from)
                .unwrap_or_else(|| models.join("kokoro/model.onnx")),
            kokoro_voices_dir: env("KOKORO_VOICES")
                .map(PathBuf::from)
                .unwrap_or_else(|| models.join("kokoro/voices")),
            fake_tts: flag("NARRATOR_FAKE_TTS", false),

            whisper_model: crate::stt::model_path(
                &env("WHISPER_MODEL").unwrap_or_else(|| "large-v3-turbo-q5_0".into()),
                &models,
            ),
            whisper_vad_model: Some(
                env("WHISPER_VAD_MODEL")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| models.join("whisper/ggml-silero-v5.1.2.bin")),
            ),
            whisper_prompt: env("WHISPER_PROMPT").unwrap_or_default(),
            whisper_threads: num(
                "WHISPER_THREADS",
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            )
            .max(1),
            models,

            chapter_bitrate: env("CHAPTER_BITRATE").unwrap_or_else(|| "64k".into()),
            chapter_gap_s: num("CHAPTER_GAP_S", 0.30),
            chapter_para_gap_s: num("CHAPTER_PARA_GAP_S", 0.60),
            hls_segment_s: num("HLS_SEGMENT_S", 6.0),

            text_shard_bytes: num("TEXT_SHARD_BYTES", 1_500_000),
            text_shard_chapters: num("TEXT_SHARD_CHAPTERS", 200),
            health_stall_s: num("HEALTH_STALL_S", 300.0),

            autopack: flag("AUTOPACK", true),
            autopack_every_s: num("AUTOPACK_EVERY_S", 5.0),
            watch_books: flag("NARRATOR_WATCH_BOOKS", true),

            sse_heartbeat_s: num("SSE_HEARTBEAT_S", 15.0),
            sse_queue: num("SSE_QUEUE", 64),
            sse_render_min_s: num("SSE_RENDER_MIN_S", 1.0),
            sse_retry_ms: num("SSE_RETRY_MS", 3000),
        }
    }

    /// A config for tests: everything under one temp root, the fake engine on,
    /// nothing watched, no vault.
    pub fn for_test(root: &std::path::Path) -> Self {
        let mut c = Self::from_env();
        c.work = root.join("work");
        c.books = vec![root.join("books")];
        c.web = root.join("web");
        c.vault = None;
        c.notes_dir = c.work.join("notes");
        c.positions_dir = c.work.clone();
        c.fake_tts = true;
        c.watch_books = false;
        c.autopack = false;
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_number_falls_back_rather_than_failing() {
        std::env::set_var("NARRATOR_TEST_NUM", "not-a-number");
        assert_eq!(num("NARRATOR_TEST_NUM", 7u16), 7);
        std::env::remove_var("NARRATOR_TEST_NUM");
    }
}
